use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
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
    if trimmed.contains("eval-loop iteration") || trimmed.contains("eval_commands") {
        return true;
    }
    if trimmed.contains("# eval-loop Command") {
        return true;
    }

    false
}

pub fn import_jsonl_file(conn: &Connection, path: &Path, project: &str) -> Result<ImportStats> {
    let file =
        fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut stats = ImportStats::default();
    let mut session_id: Option<String> = None;
    let mut first_timestamp: Option<String> = None;

    for line in reader.lines() {
        let line = line?;
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

                db::upsert_session(conn, sid, project, first_timestamp.as_deref())?;

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

    Ok(stats)
}

#[derive(Default, Debug)]
pub struct ImportStats {
    pub imported: usize,
    pub skipped: usize,
    pub filtered: usize,
    pub errors: usize,
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

    let file_count = files.len();
    for (i, (path, project)) in files.iter().enumerate() {
        if file_count > 10 && (i + 1) % 10 == 0 {
            eprint!("\rProcessing {}/{} files...", i + 1, file_count);
        }
        match import_jsonl_file(conn, path, project) {
            Ok(stats) => {
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
        "Imported {} messages ({} skipped, {} filtered, {} errors) from {} files",
        total.imported,
        total.skipped,
        total.filtered,
        total.errors,
        files.len()
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
}
