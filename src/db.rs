use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;

/// Convert project directory names like "-home-murou-ghq-github-com-user-repo" to "user/repo"
pub fn format_project_name(raw: &str) -> String {
    let parts: Vec<&str> = raw.trim_start_matches('-').split('-').collect();

    // Try to find github-com pattern and extract user/repo
    for (i, part) in parts.iter().enumerate() {
        if *part == "github" && parts.get(i + 1) == Some(&"com") && i + 3 < parts.len() {
            let user = parts[i + 2];
            let repo = parts[i + 3..].join("-");
            if let Some(pos) = repo.find("--worktrees") {
                return format!("{user}/{}", &repo[..pos]);
            }
            return format!("{user}/{repo}");
        }
    }

    // Fallback: take last meaningful segments
    let meaningful: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if meaningful.len() > 2 {
        meaningful[meaningful.len() - 2..].join("/")
    } else {
        raw.to_string()
    }
}

pub fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("Failed to open database: {}", path.display()))?;

    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;
         PRAGMA foreign_keys=ON;",
    )?;

    init_schema(&conn)?;

    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sessions (
            session_id TEXT PRIMARY KEY,
            project    TEXT NOT NULL,
            started_at TEXT,
            imported_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS messages (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            uuid       TEXT,
            role       TEXT NOT NULL,
            content    TEXT NOT NULL,
            timestamp  TEXT,
            FOREIGN KEY (session_id) REFERENCES sessions(session_id)
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_uuid
            ON messages(uuid) WHERE uuid IS NOT NULL;
        ",
    )?;

    // Create FTS table with Porter stemming, or migrate from old schema
    migrate_fts(conn)?;

    conn.execute_batch(
        "
        CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
            INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
        END;
        ",
    )?;
    Ok(())
}

/// Ensure the FTS table uses Porter stemming tokenizer.
/// Migrates from the old tokenizer if needed.
fn migrate_fts(conn: &Connection) -> Result<()> {
    let fts_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='messages_fts')",
        [],
        |row| row.get(0),
    )?;

    if fts_exists {
        // Check if the FTS table already uses porter tokenizer
        let create_sql: String = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='messages_fts'",
            [],
            |row| row.get(0),
        )?;

        if create_sql.contains("porter") {
            return Ok(());
        }

        // Old schema — drop and recreate with porter tokenizer
        eprintln!("Migrating FTS index to Porter stemming tokenizer...");
        conn.execute_batch(
            "
            DROP TRIGGER IF EXISTS messages_ai;
            DROP TRIGGER IF EXISTS messages_ad;
            DROP TRIGGER IF EXISTS messages_au;
            DROP TABLE messages_fts;
            ",
        )?;
    }

    conn.execute_batch(
        "
        CREATE VIRTUAL TABLE messages_fts USING fts5(
            content,
            content_rowid='id',
            content='messages',
            tokenize='porter unicode61'
        );
        ",
    )?;

    // If migrating from old schema, rebuild the index from existing messages
    if fts_exists {
        conn.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES('rebuild')")?;
        eprintln!("FTS index rebuilt with Porter stemming.");
    }

    Ok(())
}

pub fn upsert_session(
    conn: &Connection,
    session_id: &str,
    project: &str,
    started_at: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (session_id, project, started_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO UPDATE SET
            project = excluded.project,
            started_at = COALESCE(excluded.started_at, sessions.started_at)",
        params![session_id, project, started_at],
    )?;
    Ok(())
}

pub fn insert_message(
    conn: &Connection,
    session_id: &str,
    uuid: Option<&str>,
    role: &str,
    content: &str,
    timestamp: Option<&str>,
) -> Result<bool> {
    // Skip if uuid already exists
    if let Some(uuid_val) = uuid {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE uuid = ?1)",
            params![uuid_val],
            |row| row.get(0),
        )?;
        if exists {
            return Ok(false);
        }
    }

    conn.execute(
        "INSERT INTO messages (session_id, uuid, role, content, timestamp)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, uuid, role, content, timestamp],
    )?;
    Ok(true)
}

pub struct SearchResult {
    pub session_id: String,
    pub project: String,
    pub role: String,
    pub content: String,
    pub timestamp: Option<String>,
}

/// Escape a query string for safe use in FTS5 MATCH.
/// Wraps each token in double quotes to prevent FTS5 operator interpretation.
fn escape_fts_query(query: &str) -> String {
    // If the user already used explicit FTS5 syntax (AND, OR, NOT, quotes), pass through
    if query.contains('"')
        || query.contains(" AND ")
        || query.contains(" OR ")
        || query.contains(" NOT ")
    {
        return query.to_string();
    }
    // Otherwise, quote each whitespace-separated token
    query
        .split_whitespace()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize a project filter so that "user/repo" also matches raw dir names like
/// "-home-foo-ghq-github-com-user-repo". Replaces "/" with "-" for LIKE matching.
fn normalize_project_filter(filter: &str) -> String {
    let normalized = filter.replace('/', "-");
    format!("%{normalized}%")
}

pub fn search(
    conn: &Connection,
    query: &str,
    limit: usize,
    project_filter: Option<&str>,
    role_filter: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SearchResult>> {
    let escaped = escape_fts_query(query);
    if escaped.is_empty() {
        return Ok(vec![]);
    }

    let mut sql = String::from(
        "SELECT m.session_id, s.project, m.role, m.content, m.timestamp
         FROM messages_fts f
         JOIN messages m ON m.id = f.rowid
         JOIN sessions s ON s.session_id = m.session_id
         WHERE messages_fts MATCH ?1",
    );
    let mut param_idx = 2;

    if project_filter.is_some() {
        sql.push_str(&format!(" AND s.project LIKE ?{param_idx}"));
        param_idx += 1;
    }
    if role_filter.is_some() {
        sql.push_str(&format!(" AND m.role = ?{param_idx}"));
        param_idx += 1;
    }
    if since.is_some() {
        sql.push_str(&format!(" AND m.timestamp >= ?{param_idx}"));
        param_idx += 1;
    }
    if until.is_some() {
        sql.push_str(&format!(" AND m.timestamp <= ?{param_idx}"));
        param_idx += 1;
    }
    sql.push_str(&format!(" ORDER BY rank LIMIT ?{param_idx}"));

    let mut stmt = conn.prepare(&sql)?;

    // Build dynamic params
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    params_vec.push(Box::new(escaped));
    if let Some(proj) = project_filter {
        params_vec.push(Box::new(normalize_project_filter(proj)));
    }
    if let Some(role) = role_filter {
        params_vec.push(Box::new(role.to_string()));
    }
    if let Some(s) = since {
        params_vec.push(Box::new(s.to_string()));
    }
    if let Some(u) = until {
        params_vec.push(Box::new(u.to_string()));
    }
    params_vec.push(Box::new(limit as i64));

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();

    let results = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SearchResult {
                session_id: row.get(0)?,
                project: row.get(1)?,
                role: row.get(2)?,
                content: row.get(3)?,
                timestamp: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(results)
}

pub fn stats(conn: &Connection) -> Result<(i64, i64)> {
    let session_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
    let message_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
    Ok((session_count, message_count))
}

/// Resolve a session ID prefix to a full session ID.
/// Returns an error if the prefix matches zero or multiple sessions.
pub fn resolve_session_id(conn: &Connection, prefix: &str) -> Result<String> {
    let mut stmt =
        conn.prepare("SELECT session_id FROM sessions WHERE session_id LIKE ?1 || '%'")?;
    let matches: Vec<String> = stmt
        .query_map(params![prefix], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    match matches.len() {
        0 => anyhow::bail!("No session found matching: {prefix}"),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => {
            let previews: Vec<String> = matches.iter().take(5).cloned().collect();
            anyhow::bail!(
                "Ambiguous prefix '{prefix}' matches {n} sessions:\n  {}",
                previews.join("\n  ")
            );
        }
    }
}

pub fn get_session_messages(conn: &Connection, session_id: &str) -> Result<Vec<SearchResult>> {
    let mut stmt = conn.prepare(
        "SELECT m.session_id, s.project, m.role, m.content, m.timestamp
         FROM messages m
         JOIN sessions s ON s.session_id = m.session_id
         WHERE m.session_id = ?1
         ORDER BY m.id ASC",
    )?;

    let results = stmt
        .query_map(params![session_id], |row| {
            Ok(SearchResult {
                session_id: row.get(0)?,
                project: row.get(1)?,
                role: row.get(2)?,
                content: row.get(3)?,
                timestamp: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(results)
}

/// Get the Nth most recent session ID (0-indexed).
pub fn nth_recent_session_id(conn: &Connection, n: usize) -> Result<String> {
    conn.query_row(
        "SELECT session_id FROM sessions
         ORDER BY COALESCE(started_at, imported_at) DESC
         LIMIT 1 OFFSET ?1",
        params![n as i64],
        |row| row.get(0),
    )
    .with_context(|| format!("No session found at position {}", n + 1))
}

pub struct SessionSummary {
    pub session_id: String,
    pub project: String,
    pub started_at: Option<String>,
    pub message_count: i64,
    pub first_user_message: Option<String>,
}

pub fn list_sessions(
    conn: &Connection,
    limit: usize,
    project_filter: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SessionSummary>> {
    let mut sql = String::from(
        "SELECT s.session_id, s.project, s.started_at,
                (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.session_id) as msg_count,
                NULL as first_msg
         FROM sessions s",
    );
    let mut conditions = Vec::new();
    let mut param_idx = 1;
    if project_filter.is_some() {
        conditions.push(format!("s.project LIKE ?{param_idx}"));
        param_idx += 1;
    }
    if since.is_some() {
        conditions.push(format!(
            "COALESCE(s.started_at, s.imported_at) >= ?{param_idx}"
        ));
        param_idx += 1;
    }
    if until.is_some() {
        conditions.push(format!(
            "COALESCE(s.started_at, s.imported_at) <= ?{param_idx}"
        ));
        param_idx += 1;
    }
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(&format!(
        " ORDER BY COALESCE(s.started_at, s.imported_at) DESC LIMIT ?{param_idx}"
    ));

    let mut stmt = conn.prepare(&sql)?;

    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(proj) = project_filter {
        params_vec.push(Box::new(normalize_project_filter(proj)));
    }
    if let Some(s) = since {
        params_vec.push(Box::new(s.to_string()));
    }
    if let Some(u) = until {
        params_vec.push(Box::new(u.to_string()));
    }
    params_vec.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();

    let mut sessions: Vec<SessionSummary> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(SessionSummary {
                session_id: row.get(0)?,
                project: row.get(1)?,
                started_at: row.get(2)?,
                message_count: row.get(3)?,
                first_user_message: None,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Fetch first meaningful user message for each session
    let mut user_stmt = conn.prepare(
        "SELECT substr(content, 1, 200) FROM messages
         WHERE session_id = ?1 AND role = 'user'
         ORDER BY id ASC LIMIT 10",
    )?;
    // Fallback: first assistant message
    let mut asst_stmt = conn.prepare(
        "SELECT substr(content, 1, 200) FROM messages
         WHERE session_id = ?1 AND role = 'assistant'
         ORDER BY id ASC LIMIT 3",
    )?;

    for session in &mut sessions {
        let candidates: Vec<String> = user_stmt
            .query_map(params![&session.session_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        session.first_user_message = candidates.into_iter().find(|c| is_meaningful_preview(c));

        // If no meaningful user message found, try assistant messages
        if session.first_user_message.is_none() {
            let asst_candidates: Vec<String> = asst_stmt
                .query_map(params![&session.session_id], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            session.first_user_message = asst_candidates
                .into_iter()
                .find(|c| is_meaningful_preview(c));
        }
    }

    Ok(sessions)
}

/// Check if a message is suitable as a session preview in `recent`.
/// Skips tool_result artifacts and system meta-messages.
fn is_meaningful_preview(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.len() < 5 {
        return false;
    }
    // Skip messages starting with JSON/XML/path characters (likely tool output)
    let first_char = trimmed.chars().next().unwrap_or(' ');
    if matches!(first_char, '{' | '[' | '<') {
        return false;
    }
    // System/meta messages that are never human input
    let noise_prefixes = [
        "Tool loaded",
        "This session is being continued",
        "Your task is to create a detailed summary",
    ];
    for prefix in &noise_prefixes {
        if trimmed.starts_with(prefix) {
            return false;
        }
    }
    true
}

pub fn delete_session(conn: &Connection, session_id: &str) -> Result<u64> {
    let msg_deleted = conn.execute(
        "DELETE FROM messages WHERE session_id = ?1",
        params![session_id],
    )?;
    conn.execute(
        "DELETE FROM sessions WHERE session_id = ?1",
        params![session_id],
    )?;
    Ok(msg_deleted as u64)
}

pub fn verify(conn: &Connection) -> Result<()> {
    println!("=== 1. Messages by role ===");
    let mut stmt = conn.prepare("SELECT role, COUNT(*) FROM messages GROUP BY role")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (role, count) = row?;
        println!("  {role}: {count}");
    }

    println!("\n=== 2. Sessions and projects ===");
    let session_count: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    let project_count: i64 =
        conn.query_row("SELECT COUNT(DISTINCT project) FROM sessions", [], |r| {
            r.get(0)
        })?;
    println!("  Sessions: {session_count}, Projects: {project_count}");

    println!("\n=== 3. Top 5 projects by message count ===");
    let mut stmt = conn.prepare(
        "SELECT s.project, COUNT(*) as cnt FROM messages m
         JOIN sessions s ON s.session_id = m.session_id
         GROUP BY s.project ORDER BY cnt DESC LIMIT 5",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (proj, count) = row?;
        println!("  {}: {count}", format_project_name(&proj));
    }

    println!("\n=== 4. FTS index integrity ===");
    let msg_count: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    let fts_count: i64 = conn.query_row("SELECT COUNT(*) FROM messages_fts", [], |r| r.get(0))?;
    let fts_ok = msg_count == fts_count;
    println!("  messages: {msg_count} rows");
    println!("  messages_fts: {fts_count} rows");
    println!("  Match: {}", if fts_ok { "OK" } else { "FAIL" });

    println!("\n=== 5. UUID deduplication ===");
    let dup_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM (SELECT uuid, COUNT(*) as cnt FROM messages WHERE uuid IS NOT NULL GROUP BY uuid HAVING cnt > 1)",
        [],
        |r| r.get(0),
    )?;
    let dup_ok = dup_count == 0;
    println!(
        "  Duplicate UUIDs: {dup_count} {}",
        if dup_ok { "OK" } else { "FAIL" }
    );

    println!("\n=== 6. Recent message samples ===");
    let mut stmt = conn.prepare(
        "SELECT m.role, substr(m.content, 1, 120), m.timestamp
         FROM messages m JOIN sessions s ON s.session_id = m.session_id
         ORDER BY m.timestamp DESC LIMIT 10",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (role, content, ts) = row?;
        let content = content.replace('\n', " ");
        println!("  [{role}] {} | {content}", ts.as_deref().unwrap_or("?"));
    }

    println!("\n=== 7. FTS5 search test ===");
    for query in ["import", "SQLite", "cargo"] {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
            params![query],
            |r| r.get(0),
        )?;
        println!("  \"{query}\": {count} hits");
    }

    if !fts_ok || !dup_ok {
        anyhow::bail!("Verification failed");
    }
    println!("\nAll checks passed.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn setup_db() -> (Connection, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let conn = open_db(tmp.path()).unwrap();
        (conn, tmp)
    }

    #[test]
    fn test_open_and_init() {
        let (_conn, _tmp) = setup_db();
    }

    #[test]
    fn test_upsert_session() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "sess-1", "my-project", Some("2024-01-01T00:00:00Z")).unwrap();
        upsert_session(&conn, "sess-1", "my-project", None).unwrap();

        let project: String = conn
            .query_row(
                "SELECT project FROM sessions WHERE session_id = 'sess-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(project, "my-project");
    }

    #[test]
    fn test_insert_and_search() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "sess-1", "proj", None).unwrap();
        insert_message(
            &conn,
            "sess-1",
            Some("u1"),
            "user",
            "hello world",
            Some("2024-01-01T00:00:00Z"),
        )
        .unwrap();
        insert_message(
            &conn,
            "sess-1",
            Some("u2"),
            "assistant",
            "hi there",
            Some("2024-01-01T00:00:01Z"),
        )
        .unwrap();

        let results = search(&conn, "hello", 10, None, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].role, "user");
        assert_eq!(results[0].content, "hello world");
    }

    #[test]
    fn test_porter_stemming() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            "the server is running fine",
            None,
        )
        .unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u2"),
            "user",
            "configure the database settings",
            None,
        )
        .unwrap();

        // "run" should match "running" via stemming
        let results = search(&conn, "run", 10, None, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("running"));

        // "configuration" should match "configure" via stemming
        let results = search(&conn, "configuration", 10, None, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("configure"));
    }

    #[test]
    fn test_duplicate_uuid_skipped() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "sess-1", "proj", None).unwrap();
        let inserted = insert_message(&conn, "sess-1", Some("u1"), "user", "first", None).unwrap();
        assert!(inserted);
        let inserted =
            insert_message(&conn, "sess-1", Some("u1"), "user", "duplicate", None).unwrap();
        assert!(!inserted);

        let (_, msg_count) = stats(&conn).unwrap();
        assert_eq!(msg_count, 1);
    }

    #[test]
    fn test_stats() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "p1", None).unwrap();
        upsert_session(&conn, "s2", "p2", None).unwrap();
        insert_message(&conn, "s1", Some("u1"), "user", "msg1", None).unwrap();
        insert_message(&conn, "s1", Some("u2"), "assistant", "msg2", None).unwrap();
        insert_message(&conn, "s2", Some("u3"), "user", "msg3", None).unwrap();

        let (sessions, messages) = stats(&conn).unwrap();
        assert_eq!(sessions, 2);
        assert_eq!(messages, 3);
    }

    #[test]
    fn test_search_no_results() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        insert_message(&conn, "s1", Some("u1"), "user", "hello", None).unwrap();

        let results = search(&conn, "nonexistent", 10, None, None, None, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_with_date_filter() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u1"),
            "user",
            "early message",
            Some("2024-01-01T00:00:00Z"),
        )
        .unwrap();
        insert_message(
            &conn,
            "s1",
            Some("u2"),
            "user",
            "late message",
            Some("2024-06-01T00:00:00Z"),
        )
        .unwrap();

        let results = search(&conn, "message", 10, None, None, Some("2024-03-01"), None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("late"));

        let results = search(&conn, "message", 10, None, None, None, Some("2024-03-01")).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("early"));
    }

    #[test]
    fn test_search_empty_query() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        insert_message(&conn, "s1", Some("u1"), "user", "hello", None).unwrap();

        let results = search(&conn, "", 10, None, None, None, None).unwrap();
        assert!(results.is_empty());

        let results = search(&conn, "   ", 10, None, None, None, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_project_filter_with_slash() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "-home-user-ghq-github-com-owner-repo", None).unwrap();
        insert_message(&conn, "s1", Some("u1"), "user", "test msg", None).unwrap();

        // Filter with formatted name "owner/repo"
        let results = search(&conn, "test", 10, Some("owner/repo"), None, None, None).unwrap();
        assert_eq!(results.len(), 1);

        // Filter with just "repo"
        let results = search(&conn, "test", 10, Some("repo"), None, None, None).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_delete_session() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        insert_message(&conn, "s1", Some("u1"), "user", "msg1", None).unwrap();
        insert_message(&conn, "s1", Some("u2"), "assistant", "msg2", None).unwrap();

        let deleted = delete_session(&conn, "s1").unwrap();
        assert_eq!(deleted, 2);

        let (sessions, messages) = stats(&conn).unwrap();
        assert_eq!(sessions, 0);
        assert_eq!(messages, 0);
    }

    #[test]
    fn test_list_sessions_with_date_filter() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", Some("2024-01-15T00:00:00Z")).unwrap();
        upsert_session(&conn, "s2", "proj", Some("2024-06-15T00:00:00Z")).unwrap();
        insert_message(&conn, "s1", Some("u1"), "user", "old session", None).unwrap();
        insert_message(&conn, "s2", Some("u2"), "user", "new session", None).unwrap();

        let sessions = list_sessions(&conn, 100, None, Some("2024-03-01"), None).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s2");

        let sessions = list_sessions(&conn, 100, None, None, Some("2024-03-01")).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s1");
    }

    #[test]
    fn test_insert_without_uuid() {
        let (conn, _tmp) = setup_db();
        upsert_session(&conn, "s1", "proj", None).unwrap();
        let inserted = insert_message(&conn, "s1", None, "user", "msg1", None).unwrap();
        assert!(inserted);
        // Without uuid, duplicate check is skipped, so second insert also succeeds
        let inserted = insert_message(&conn, "s1", None, "user", "msg2", None).unwrap();
        assert!(inserted);

        let (_, count) = stats(&conn).unwrap();
        assert_eq!(count, 2);
    }
}
