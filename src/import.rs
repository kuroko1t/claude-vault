use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::db;

/// Low-value tool_use calls that only reference file paths or simple lookups.
/// These are skipped to reduce noise in the archive.
const SKIP_TOOL_NAMES: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LSP",
    "ToolSearch",
    "mcp__playwright__browser_snapshot",
    "mcp__playwright__browser_take_screenshot",
    "mcp__playwright__browser_navigate",
    "mcp__playwright__browser_click",
    "mcp__playwright__browser_tabs",
    "TaskGet",
    "TaskOutput",
    "TaskList",
];

fn extract_text_content(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let texts: Vec<String> = arr
                .iter()
                .filter_map(|item| match item.get("type").and_then(|t| t.as_str()) {
                    Some("text") => item.get("text").and_then(|t| t.as_str()).map(String::from),
                    Some("tool_use") => {
                        let name = item
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("unknown");
                        // Skip low-value tool calls
                        if SKIP_TOOL_NAMES.contains(&name) {
                            return None;
                        }
                        let input = item.get("input").map(|i| i.to_string()).unwrap_or_default();
                        Some(format!("[tool_use: {name}] {input}"))
                    }
                    Some("tool_result") => None,
                    _ => None,
                })
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

/// Remove system-injected noise tags from message content.
fn clean_content(content: &str) -> String {
    let mut result = content.to_string();

    // Remove XML-like system tags and their content
    let tag_patterns = [
        "system-reminder",
        "local-command-caveat",
        "local-command-stdout",
        "command-name",
        "command-message",
        "command-args",
        "task-notification",
    ];
    for tag in &tag_patterns {
        while let Some(start) = result.find(&format!("<{tag}>")) {
            if let Some(end) = result.find(&format!("</{tag}>")) {
                let end = end + format!("</{tag}>").len();
                result.replace_range(start..end, "");
            } else {
                break;
            }
        }
        // Also remove self-closing or opening-only variants
        while let Some(start) = result.find(&format!("<{tag}")) {
            if let Some(end) = result[start..].find('>') {
                result.replace_range(start..start + end + 1, "");
            } else {
                break;
            }
        }
    }

    result
}

/// Check if a message is noise that should be skipped entirely.
fn is_noise(content: &str) -> bool {
    let trimmed = content.trim();

    // Zero-width spaces, empty, or trivially short non-meaningful
    if trimmed.is_empty() {
        return true;
    }
    if trimmed
        .chars()
        .all(|c| c.is_whitespace() || c == '\u{200b}' || c == '\u{feff}')
    {
        return true;
    }

    // Stop hook / eval-loop meta messages
    if trimmed.contains("Stop hook feedback:") {
        return true;
    }
    if trimmed.starts_with("eval-loop iteration") {
        return true;
    }
    if trimmed.starts_with("# eval-loop Command") {
        return true;
    }

    false
}

/// Import a whole JSONL file in one transaction. Does not record import
/// progress; `import_all` is the incremental path.
pub fn import_jsonl_file(conn: &Connection, path: &Path, project: &str) -> Result<ImportStats> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let (stats, _) = import_jsonl_range(&tx, path, project, 0, u64::MAX, None)?;
    tx.commit()?;
    Ok(stats)
}

/// Where a file's import stopped: the byte just past the last complete line
/// consumed, and the session ID resolved so far.
struct RangeEnd {
    offset: u64,
    session_id: Option<String>,
}

/// Parse and import lines from byte `start` up to at most `limit` bytes.
///
/// Only complete (newline-terminated) lines advance the returned offset. A
/// trailing line without a newline is consumed only if it is already a full
/// JSON record; otherwise it is assumed to be mid-write and is left for the
/// next run. Caller is responsible for transaction handling.
fn import_jsonl_range(
    conn: &Connection,
    path: &Path,
    project: &str,
    start: u64,
    limit: u64,
    known_session_id: Option<String>,
) -> Result<(ImportStats, RangeEnd)> {
    let mut file =
        fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(256 * 1024, file.take(limit));

    let mut stats = ImportStats::default();
    let mut session_id: Option<String> = known_session_id;
    let mut first_timestamp: Option<String> = None;
    let mut session_upserted = false;
    let mut offset = start;
    let mut buf: Vec<u8> = Vec::new();

    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        let complete = buf.last() == Some(&b'\n');
        let line = String::from_utf8_lossy(&buf);
        if !complete && serde_json::from_str::<Value>(line.trim()).is_err() {
            // Partial line still being written: stop before it.
            break;
        }
        offset += n as u64;

        if line.trim().is_empty() {
            continue;
        }

        let record: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };

        let record_type = match record.get("type").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => continue,
        };

        // Extract session ID from any record that has it
        if session_id.is_none() {
            if let Some(sid) = record.get("sessionId").and_then(|s| s.as_str()) {
                session_id = Some(sid.to_string());
            }
        }

        match record_type {
            "user" | "assistant" => {
                let message = match record.get("message") {
                    Some(m) => m,
                    None => continue,
                };

                let role = match message.get("role").and_then(|r| r.as_str()) {
                    Some(r) => r,
                    None => continue,
                };

                let raw_content = match message.get("content").and_then(extract_text_content) {
                    Some(c) => c,
                    None => continue,
                };

                // Clean and filter content
                let content = clean_content(&raw_content);
                if is_noise(&content) {
                    stats.filtered += 1;
                    continue;
                }

                // Skip empty content after cleaning
                if content.trim().is_empty() {
                    stats.filtered += 1;
                    continue;
                }

                let uuid = record.get("uuid").and_then(|u| u.as_str());
                let timestamp = record.get("timestamp").and_then(|t| t.as_str());

                if first_timestamp.is_none() {
                    first_timestamp = timestamp.map(String::from);
                }

                let sid = match &session_id {
                    Some(s) => s.as_str(),
                    None => {
                        // Derive session ID from filename
                        let stem = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown");
                        session_id = Some(stem.to_string());
                        session_id.as_ref().unwrap().as_str()
                    }
                };

                // session_id and first_timestamp are fixed once set, so one
                // upsert per call carries everything the old per-message
                // upsert did.
                if !session_upserted {
                    db::upsert_session(conn, sid, project, first_timestamp.as_deref())?;
                    session_upserted = true;
                }

                if db::insert_message(conn, sid, uuid, role, &content, timestamp)? {
                    stats.imported += 1;
                } else {
                    stats.skipped += 1;
                }
            }
            _ => {
                // Skip non-message types (file-history-snapshot, progress, queue-operation, system)
            }
        }
    }

    Ok((stats, RangeEnd { offset, session_id }))
}

#[derive(Default, Debug)]
pub struct ImportStats {
    pub imported: usize,
    pub skipped: usize,
    pub filtered: usize,
    pub errors: usize,
    /// Files whose size and mtime matched the recorded state; not opened.
    pub files_unchanged: usize,
    /// Files that were opened and read (fully or from a saved offset).
    pub files_read: usize,
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// True when byte `offset - 1` of the file is a newline, i.e. the saved
/// offset still sits on a line boundary. Guards against a file that was
/// rewritten in place to the same or larger size.
fn offset_on_line_boundary(path: &Path, offset: u64) -> bool {
    if offset == 0 {
        return true;
    }
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut byte = [0u8; 1];
    f.seek(SeekFrom::Start(offset - 1)).is_ok()
        && f.read_exact(&mut byte).is_ok()
        && byte[0] == b'\n'
}

/// Import whatever is new in one file, inside a single IMMEDIATE
/// transaction. The saved state is re-read under the write lock, so
/// concurrent `import` processes never redo each other's work.
fn import_file_incremental(
    conn: &Connection,
    path: &Path,
    project: &str,
    size: u64,
    mtime: i64,
) -> Result<Option<ImportStats>> {
    let key = path.to_string_lossy();
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;

    let prev = db::get_import_state(&tx, &key)?;
    if let Some(p) = &prev {
        if p.size as u64 == size && p.mtime_ns == mtime {
            // Another process imported it while we waited for the lock.
            return Ok(None);
        }
    }

    let (start, known_sid) = match prev {
        Some(p) if (p.offset as u64) <= size && offset_on_line_boundary(path, p.offset as u64) => {
            (p.offset as u64, p.session_id)
        }
        // Truncated or rewritten: start over; uuid dedup prevents duplicates.
        _ => (0, None),
    };

    let (stats, end) = import_jsonl_range(&tx, path, project, start, size - start, known_sid)?;

    db::set_import_state(
        &tx,
        &key,
        &db::ImportState {
            size: size as i64,
            mtime_ns: mtime,
            offset: end.offset as i64,
            session_id: end.session_id,
        },
    )?;
    tx.commit()?;
    Ok(Some(stats))
}

pub fn discover_jsonl_files(claude_dir: &Path) -> Result<Vec<(PathBuf, String)>> {
    let projects_dir = claude_dir.join("projects");
    if !projects_dir.exists() {
        anyhow::bail!("Projects directory not found: {}", projects_dir.display());
    }

    let mut files = Vec::new();

    for entry in fs::read_dir(&projects_dir)? {
        let entry = entry?;
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }

        let project_name = entry.file_name().to_string_lossy().to_string();

        // Find .jsonl files in the project directory and subagent directories
        collect_jsonl_files(&project_dir, &project_name, &mut files)?;
    }

    Ok(files)
}

fn collect_jsonl_files(
    dir: &Path,
    project_name: &str,
    files: &mut Vec<(PathBuf, String)>,
) -> Result<()> {
    for file_entry in fs::read_dir(dir)? {
        let file_entry = file_entry?;
        let file_path = file_entry.path();
        if file_path.is_file() && file_path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push((file_path, project_name.to_string()));
        } else if file_path.is_dir() {
            // Recurse into subdirectories (e.g., session_id/subagents/)
            collect_jsonl_files(&file_path, project_name, files)?;
        }
    }
    Ok(())
}

pub fn import_all(conn: &Connection, claude_dir: &Path) -> Result<ImportStats> {
    let files = discover_jsonl_files(claude_dir)?;
    let mut total = ImportStats::default();

    // One lock-free read of all saved progress; unchanged files are skipped
    // without being opened or taking the write lock.
    let states = db::load_import_states(conn)?;

    let file_count = files.len();
    for (i, (path, project)) in files.iter().enumerate() {
        if file_count > 10 && (i + 1) % 10 == 0 {
            eprint!("\rProcessing {}/{} files...", i + 1, file_count);
        }
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                // Deleted between discovery and stat; nothing to do.
                eprintln!("Warning: Failed to stat {}: {}", path.display(), e);
                continue;
            }
        };
        let size = meta.len();
        let mtime = mtime_ns(&meta);
        if let Some(s) = states.get(path.to_string_lossy().as_ref()) {
            if s.size as u64 == size && s.mtime_ns == mtime {
                total.files_unchanged += 1;
                continue;
            }
        }
        match import_file_incremental(conn, path, project, size, mtime) {
            Ok(None) => total.files_unchanged += 1,
            Ok(Some(stats)) => {
                total.files_read += 1;
                total.imported += stats.imported;
                total.skipped += stats.skipped;
                total.filtered += stats.filtered;
                total.errors += stats.errors;
            }
            Err(e) => {
                eprintln!("Warning: Failed to import {}: {}", path.display(), e);
                total.errors += 1;
            }
        }
    }
    if file_count > 10 {
        eprintln!("\rProcessing {}/{} files... done.", file_count, file_count);
    }

    println!(
        "Imported {} messages ({} skipped, {} filtered, {} errors) from {} files ({} read, {} unchanged)",
        total.imported,
        total.skipped,
        total.filtered,
        total.errors,
        files.len(),
        total.files_read,
        total.files_unchanged
    );

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn setup_db() -> (Connection, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let conn = db::open_db(tmp.path()).unwrap();
        (conn, tmp)
    }

    #[test]
    fn test_extract_text_content_string() {
        let v = Value::String("hello".into());
        assert_eq!(extract_text_content(&v), Some("hello".into()));
    }

    #[test]
    fn test_extract_text_content_array() {
        let v: Value = serde_json::json!([
            {"type": "text", "text": "hello"},
            {"type": "text", "text": "world"}
        ]);
        assert_eq!(extract_text_content(&v), Some("hello\nworld".into()));
    }

    #[test]
    fn test_import_jsonl_file() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        writeln!(jsonl, r#"{{"type":"user","sessionId":"s1","uuid":"u1","timestamp":"2024-01-01T00:00:00Z","message":{{"role":"user","content":"hello world"}}}}"#).unwrap();
        writeln!(jsonl, r#"{{"type":"assistant","sessionId":"s1","uuid":"u2","timestamp":"2024-01-01T00:00:01Z","message":{{"role":"assistant","content":"hi there"}}}}"#).unwrap();
        writeln!(
            jsonl,
            r#"{{"type":"file-history-snapshot","messageId":"x"}}"#
        )
        .unwrap();

        let stats = import_jsonl_file(&conn, jsonl.path(), "test-project").unwrap();
        assert_eq!(stats.imported, 2);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.errors, 0);

        let (sessions, messages) = db::stats(&conn).unwrap();
        assert_eq!(sessions, 1);
        assert_eq!(messages, 2);
    }

    #[test]
    fn test_import_skips_duplicates() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        writeln!(jsonl, r#"{{"type":"user","sessionId":"s1","uuid":"u1","timestamp":"2024-01-01T00:00:00Z","message":{{"role":"user","content":"hello"}}}}"#).unwrap();

        import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.imported, 0);
        assert_eq!(stats.skipped, 1);
    }

    #[test]
    fn test_discover_jsonl_files() {
        let dir = TempDir::new().unwrap();
        let projects = dir.path().join("projects");
        let project = projects.join("my-project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("session1.jsonl"), "{}").unwrap();
        fs::write(project.join("session2.jsonl"), "{}").unwrap();
        fs::write(project.join("not-jsonl.txt"), "{}").unwrap();

        // subagents dir is now also scanned recursively
        let subagents = project.join("session1").join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(subagents.join("agent.jsonl"), "{}").unwrap();

        let files = discover_jsonl_files(dir.path()).unwrap();
        assert_eq!(files.len(), 3); // 2 top-level + 1 subagent
        assert!(files.iter().all(|(_, name)| name == "my-project"));
    }

    #[test]
    fn test_extract_tool_use_skips_read() {
        // Read is a low-value tool, should be skipped
        let v: Value = serde_json::json!([
            {"type": "tool_use", "name": "Read", "input": {"path": "/tmp/foo"}},
            {"type": "text", "text": "done"}
        ]);
        let result = extract_text_content(&v).unwrap();
        assert!(!result.contains("Read"));
        assert!(result.contains("done"));
    }

    #[test]
    fn test_extract_tool_use_keeps_edit() {
        let v: Value = serde_json::json!([
            {"type": "tool_use", "name": "Edit", "input": {"file": "foo.rs", "content": "fn main() {}"}}
        ]);
        let result = extract_text_content(&v).unwrap();
        assert!(result.contains("[tool_use: Edit]"));
    }

    #[test]
    fn test_extract_read_only_returns_none() {
        // A message with only a Read tool_use should produce None
        let v: Value = serde_json::json!([
            {"type": "tool_use", "name": "Read", "input": {"path": "/tmp/foo"}}
        ]);
        assert_eq!(extract_text_content(&v), None);
    }

    #[test]
    fn test_extract_empty_array() {
        let v: Value = serde_json::json!([]);
        assert_eq!(extract_text_content(&v), None);
    }

    #[test]
    fn test_clean_content_removes_system_reminder() {
        let input = "hello <system-reminder>noise here</system-reminder> world";
        assert_eq!(clean_content(input), "hello  world");
    }

    #[test]
    fn test_clean_content_removes_caveat() {
        let input = "<local-command-caveat>blah</local-command-caveat>real content";
        assert_eq!(clean_content(input), "real content");
    }

    #[test]
    fn test_is_noise() {
        assert!(is_noise(""));
        assert!(is_noise("\u{200b}"));
        assert!(is_noise("Stop hook feedback: some task"));
        assert!(is_noise("some prefix\nStop hook feedback: task"));
        assert!(is_noise("eval-loop iteration 3/30."));
        assert!(is_noise("# eval-loop Command\n\nExecute the setup..."));
        assert!(!is_noise("hello world"));
        // Messages discussing eval-loop should NOT be filtered
        assert!(!is_noise(
            "The eval_commands field contains the test scripts"
        ));
        assert!(!is_noise(
            "Check the eval-loop iteration results in the output"
        ));
    }

    #[test]
    fn test_clean_content_removes_command_tags() {
        let input = "<command-name>/mcp</command-name>\n<command-message>mcp</command-message>\n<command-args></command-args>";
        let cleaned = clean_content(input);
        assert!(!cleaned.contains("command-name"));
        assert!(cleaned.trim().is_empty());
    }

    #[test]
    fn test_import_empty_content_skipped() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        writeln!(
            jsonl,
            r#"{{"type":"user","sessionId":"s1","uuid":"u1","message":{{"role":"user","content":"   "}}}}"#
        )
        .unwrap();

        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.imported, 0);
    }

    #[test]
    fn test_import_filters_system_reminder() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        // Message that is purely a system-reminder
        writeln!(
            jsonl,
            r#"{{"type":"user","sessionId":"s1","uuid":"u1","timestamp":"2024-01-01T00:00:00Z","message":{{"role":"user","content":"<system-reminder>You are Claude</system-reminder>"}}}}"#
        )
        .unwrap();
        // Message with real content + system-reminder (should keep cleaned version)
        writeln!(
            jsonl,
            r#"{{"type":"user","sessionId":"s1","uuid":"u2","timestamp":"2024-01-01T00:00:01Z","message":{{"role":"user","content":"real question <system-reminder>noise</system-reminder>"}}}}"#
        )
        .unwrap();

        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.imported, 1); // Only the one with real content
        assert_eq!(stats.filtered, 1); // The pure system-reminder was filtered

        let results = db::search(&conn, "real", 10, None, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].content.contains("system-reminder"));
    }

    #[test]
    fn test_import_no_uuid() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        writeln!(
            jsonl,
            r#"{{"type":"user","sessionId":"s1","message":{{"role":"user","content":"no uuid msg"}}}}"#
        )
        .unwrap();

        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.imported, 1);

        let results = db::search(&conn, "uuid", 10, None, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_import_malformed_json() {
        let (conn, _tmp) = setup_db();

        let mut jsonl = NamedTempFile::new().unwrap();
        writeln!(jsonl, "not valid json").unwrap();
        writeln!(
            jsonl,
            r#"{{"type":"user","sessionId":"s1","uuid":"u1","message":{{"role":"user","content":"valid"}}}}"#
        )
        .unwrap();

        let stats = import_jsonl_file(&conn, jsonl.path(), "proj").unwrap();
        assert_eq!(stats.imported, 1);
        assert_eq!(stats.errors, 1);
    }

    // ---- incremental import ----

    fn msg_line(sid: Option<&str>, uuid: &str, ts: &str, text: &str) -> String {
        let sid = sid
            .map(|s| format!(r#""sessionId":"{s}","#))
            .unwrap_or_default();
        format!(
            r#"{{"type":"user",{sid}"uuid":"{uuid}","timestamp":"{ts}","message":{{"role":"user","content":"{text}"}}}}"#
        ) + "\n"
    }

    /// A fake ~/.claude with one project containing one session file.
    fn claude_dir_with(content: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("projects").join("proj");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("sess.jsonl");
        fs::write(&file, content).unwrap();
        (dir, file)
    }

    fn append(path: &Path, s: &str) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    fn message_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn test_incremental_second_run_opens_nothing() {
        let (conn, _db) = setup_db();
        let content = msg_line(Some("s1"), "u1", "2024-01-01T00:00:00Z", "one")
            + &msg_line(Some("s1"), "u2", "2024-01-01T00:00:01Z", "two");
        let (dir, _file) = claude_dir_with(&content);

        let first = import_all(&conn, dir.path()).unwrap();
        assert_eq!(first.imported, 2);
        assert_eq!(first.files_read, 1);

        let second = import_all(&conn, dir.path()).unwrap();
        assert_eq!(second.imported, 0);
        assert_eq!(second.skipped, 0, "unchanged file must not be re-parsed");
        assert_eq!(second.files_read, 0);
        assert_eq!(second.files_unchanged, 1);
        assert_eq!(message_count(&conn), 2);
    }

    #[test]
    fn test_incremental_append_reads_only_new_bytes() {
        let (conn, _db) = setup_db();
        let (dir, file) =
            claude_dir_with(&msg_line(Some("s1"), "u1", "2024-01-01T00:00:00Z", "one"));
        import_all(&conn, dir.path()).unwrap();

        append(
            &file,
            &msg_line(Some("s1"), "u2", "2024-01-01T00:00:01Z", "two"),
        );
        let stats = import_all(&conn, dir.path()).unwrap();
        assert_eq!(stats.imported, 1);
        // skipped == 0 proves u1 was not re-read from the start of the file
        assert_eq!(stats.skipped, 0);
        assert_eq!(message_count(&conn), 2);

        let st = db::get_import_state(&conn, &file.to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(st.offset as u64, fs::metadata(&file).unwrap().len());
    }

    #[test]
    fn test_resume_keeps_session_id_from_earlier_bytes() {
        let (conn, _db) = setup_db();
        let (dir, file) = claude_dir_with(&msg_line(
            Some("real-sid"),
            "u1",
            "2024-01-01T00:00:00Z",
            "one",
        ));
        import_all(&conn, dir.path()).unwrap();

        // Appended record has no sessionId; it must not fall back to the
        // filename stem ("sess") just because the run started mid-file.
        append(&file, &msg_line(None, "u2", "2024-01-01T00:00:01Z", "two"));
        import_all(&conn, dir.path()).unwrap();

        let sid: String = conn
            .query_row("SELECT session_id FROM messages WHERE uuid='u2'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(sid, "real-sid");
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1);
    }

    #[test]
    fn test_partial_trailing_line_is_deferred_until_complete() {
        let (conn, _db) = setup_db();
        let full = msg_line(Some("s1"), "u2", "2024-01-01T00:00:01Z", "two");
        let (head, tail) = full.split_at(full.len() / 2);
        let (dir, file) =
            claude_dir_with(&(msg_line(Some("s1"), "u1", "2024-01-01T00:00:00Z", "one") + head));

        let first = import_all(&conn, dir.path()).unwrap();
        assert_eq!(first.imported, 1);
        assert_eq!(
            first.errors, 0,
            "half-written line must not count as an error"
        );

        append(&file, tail);
        let second = import_all(&conn, dir.path()).unwrap();
        assert_eq!(second.imported, 1);
        assert_eq!(second.errors, 0);
        assert_eq!(message_count(&conn), 2);
    }

    #[test]
    fn test_complete_last_line_without_newline_is_imported() {
        let (conn, _db) = setup_db();
        let line = msg_line(Some("s1"), "u1", "2024-01-01T00:00:00Z", "one");
        let (dir, _file) = claude_dir_with(line.trim_end());
        let stats = import_all(&conn, dir.path()).unwrap();
        assert_eq!(stats.imported, 1);
    }

    #[test]
    fn test_rewritten_file_restarts_from_zero() {
        let (conn, _db) = setup_db();
        let (dir, file) = claude_dir_with(
            &(msg_line(Some("s1"), "u1", "2024-01-01T00:00:00Z", "one")
                + &msg_line(Some("s1"), "u2", "2024-01-01T00:00:01Z", "two")),
        );
        import_all(&conn, dir.path()).unwrap();

        // Shrunk below the saved offset: must re-read from the start.
        fs::write(
            &file,
            msg_line(Some("s1"), "u3", "2024-01-01T00:00:02Z", "three"),
        )
        .unwrap();
        let stats = import_all(&conn, dir.path()).unwrap();
        assert_eq!(stats.imported, 1);
        assert_eq!(message_count(&conn), 3);
    }

    #[test]
    fn test_started_at_keeps_earliest() {
        let (conn, _db) = setup_db();
        db::upsert_session(&conn, "s1", "p", Some("2024-01-02T00:00:00Z")).unwrap();
        db::upsert_session(&conn, "s1", "p", Some("2024-01-01T00:00:00Z")).unwrap();
        db::upsert_session(&conn, "s1", "p", Some("2024-01-03T00:00:00Z")).unwrap();
        db::upsert_session(&conn, "s1", "p", None).unwrap();
        let started: String = conn
            .query_row(
                "SELECT started_at FROM sessions WHERE session_id='s1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(started, "2024-01-01T00:00:00Z");
    }

    #[test]
    fn test_concurrent_imports_do_not_duplicate_or_fail() {
        let db_file = NamedTempFile::new().unwrap();
        db::open_db(db_file.path()).unwrap();

        let dir = TempDir::new().unwrap();
        let project = dir.path().join("projects").join("proj");
        fs::create_dir_all(&project).unwrap();
        for f in 0..20 {
            let mut content = String::new();
            for m in 0..200 {
                content += &msg_line(
                    Some(&format!("s{f}")),
                    &format!("u{f}-{m}"),
                    "2024-01-01T00:00:00Z",
                    "hello",
                );
            }
            fs::write(project.join(format!("s{f}.jsonl")), content).unwrap();
        }

        let handles: Vec<_> = (0..6)
            .map(|_| {
                let db_path = db_file.path().to_path_buf();
                let claude = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    let conn = db::open_db(&db_path).unwrap();
                    import_all(&conn, &claude).unwrap()
                })
            })
            .collect();
        let results: Vec<ImportStats> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let conn = db::open_db(db_file.path()).unwrap();
        assert_eq!(message_count(&conn), 4000);
        assert_eq!(results.iter().map(|r| r.imported).sum::<usize>(), 4000);
        assert_eq!(results.iter().map(|r| r.errors).sum::<usize>(), 0);
        // Each file is read by exactly one process; the rest see fresh state.
        assert_eq!(results.iter().map(|r| r.files_read).sum::<usize>(), 20);
    }
}
