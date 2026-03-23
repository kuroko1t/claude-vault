mod db;
mod import;

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "claude-vault")]
#[command(about = "Archive Claude Code conversations into SQLite with FTS5 full-text search")]
#[command(version)]
struct Cli {
    /// Path to the SQLite database file
    #[arg(long, env = "CLAUDE_VAULT_DB")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Import all JSONL files from ~/.claude/projects/
    Import {
        /// Path to Claude config directory (default: ~/.claude)
        #[arg(long)]
        claude_dir: Option<PathBuf>,
    },
    /// Import a single JSONL session file
    ImportFile {
        /// Path to the JSONL file
        path: PathBuf,
        /// Project name
        #[arg(long)]
        project: Option<String>,
    },
    /// Search conversations using full-text search
    Search {
        /// Search query (FTS5 syntax)
        query: String,
        /// Maximum number of results
        #[arg(short, long, default_value = "10")]
        limit: usize,
        /// Filter by project name (substring match)
        #[arg(short, long)]
        project: Option<String>,
        /// Filter by role (user or assistant)
        #[arg(short, long)]
        role: Option<String>,
        /// Only show results after this date (e.g. 2024-01-01)
        #[arg(long)]
        since: Option<String>,
        /// Only show results before this date (e.g. 2024-12-31)
        #[arg(long)]
        until: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Include tool_use lines in results (hidden by default)
        #[arg(long)]
        include_tools: bool,
    },
    /// Export a session in various formats
    Export {
        /// Session ID or prefix (e.g. "47cf1f2e")
        #[arg(required_unless_present = "last")]
        session_id: Option<String>,
        /// Export the Nth most recent session (1 = latest, 2 = second latest, ...)
        #[arg(long, default_missing_value = "1", num_args = 0..=1, value_name = "N")]
        last: Option<usize>,
        /// Output format
        #[arg(short, long, default_value = "markdown")]
        format: ExportFormat,
    },
    /// List sessions
    List {
        /// Number of sessions to show (0 = all)
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// Filter by project name (substring match)
        #[arg(short, long)]
        project: Option<String>,
        /// Only show sessions after this date (e.g. 2024-01-01)
        #[arg(long)]
        since: Option<String>,
        /// Only show sessions before this date (e.g. 2024-12-31)
        #[arg(long)]
        until: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Delete a session and all its messages
    Delete {
        /// Session ID or prefix
        session_id: String,
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Show database statistics
    Stats,
    /// Verify database integrity
    Verify,
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
}

#[derive(Clone, ValueEnum)]
enum ExportFormat {
    Markdown,
    Json,
    Text,
}

fn format_project_name(raw: &str) -> String {
    db::format_project_name(raw)
}

/// Remove lines that start with `[tool_use: ` from content.
fn strip_tool_lines(content: &str) -> String {
    content
        .lines()
        .filter(|line| !line.starts_with("[tool_use: "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn default_db_path() -> Result<PathBuf> {
    let data_dir = dirs::data_dir().context("Could not determine data directory")?;
    let vault_dir = data_dir.join("claude-vault");
    std::fs::create_dir_all(&vault_dir)?;
    Ok(vault_dir.join("vault.db"))
}

fn default_claude_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join(".claude"))
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // Handle completions before opening DB (no DB needed)
    if let Commands::Completions { shell } = &cli.command {
        let mut cmd = Cli::command();
        generate(*shell, &mut cmd, "claude-vault", &mut std::io::stdout());
        return Ok(());
    }

    let db_path = match cli.db {
        Some(p) => p,
        None => default_db_path()?,
    };

    let conn = db::open_db(&db_path)?;

    match cli.command {
        Commands::Import { claude_dir } => {
            let claude_dir = match claude_dir {
                Some(d) => d,
                None => default_claude_dir()?,
            };
            import::import_all(&conn, &claude_dir)?;
        }
        Commands::ImportFile { path, project } => {
            let project = project.unwrap_or_else(|| "unknown".to_string());
            let stats = import::import_jsonl_file(&conn, &path, &project)?;
            println!(
                "Imported {} messages ({} skipped, {} filtered, {} errors)",
                stats.imported, stats.skipped, stats.filtered, stats.errors
            );
        }
        Commands::Search {
            query,
            limit,
            project,
            role,
            since,
            until,
            json,
            include_tools,
        } => {
            let results = db::search(
                &conn,
                &query,
                limit,
                project.as_deref(),
                role.as_deref(),
                since.as_deref(),
                until.as_deref(),
            )?;
            let strip = !include_tools;
            if json {
                let json_results: Vec<serde_json::Value> = results
                    .iter()
                    .filter_map(|r| {
                        let content = if strip {
                            strip_tool_lines(&r.content)
                        } else {
                            r.content.clone()
                        };
                        if strip && content.trim().is_empty() {
                            return None;
                        }
                        Some(serde_json::json!({
                            "session_id": r.session_id,
                            "project": format_project_name(&r.project),
                            "role": r.role,
                            "content": content,
                            "timestamp": r.timestamp,
                        }))
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_results)?);
                return Ok(());
            }
            if results.is_empty() {
                println!("No results found.");
                return Ok(());
            }
            let mut printed = 0;
            for r in results.iter() {
                let content = if strip {
                    strip_tool_lines(&r.content)
                } else {
                    r.content.clone()
                };
                if strip && content.trim().is_empty() {
                    continue;
                }
                if printed > 0 {
                    println!("---");
                }
                let project = format_project_name(&r.project);
                println!(
                    "[{}] {} | {} | {}",
                    r.role,
                    project,
                    r.session_id,
                    r.timestamp.as_deref().unwrap_or("unknown")
                );
                let content: String = if content.chars().count() > 300 {
                    let truncated: String = content.chars().take(300).collect();
                    format!("{truncated}...")
                } else {
                    content
                };
                println!("{}", content);
                printed += 1;
            }
            if printed == 0 {
                println!("No results found.");
            }
        }
        Commands::Export {
            session_id,
            last,
            format,
        } => {
            let resolved_id = match (session_id, last) {
                (_, Some(0)) => bail!("--last must be at least 1"),
                (_, Some(n)) => db::nth_recent_session_id(&conn, n.saturating_sub(1))?,
                (Some(prefix), None) => db::resolve_session_id(&conn, &prefix)?,
                (None, None) => bail!("Specify a session ID or use --last"),
            };
            let messages = db::get_session_messages(&conn, &resolved_id)?;
            if messages.is_empty() {
                bail!("No messages found for session: {resolved_id}");
            }
            let session_id = &resolved_id;
            let project = format_project_name(&messages[0].project);
            match format {
                ExportFormat::Markdown => {
                    println!("# Session: {session_id}");
                    println!("**Project:** {project}  ");
                    if let Some(ts) = &messages[0].timestamp {
                        println!("**Started:** {ts}  ");
                    }
                    println!();
                    for m in &messages {
                        let role_label = match m.role.as_str() {
                            "user" => "User",
                            "assistant" => "Assistant",
                            _ => &m.role,
                        };
                        println!("## {role_label}");
                        if let Some(ts) = &m.timestamp {
                            println!("*{ts}*\n");
                        }
                        println!("{}\n", m.content);
                    }
                }
                ExportFormat::Json => {
                    let json_messages: Vec<serde_json::Value> = messages
                        .iter()
                        .map(|m| {
                            serde_json::json!({
                                "role": m.role,
                                "content": m.content,
                                "timestamp": m.timestamp,
                            })
                        })
                        .collect();
                    let output = serde_json::json!({
                        "session_id": session_id,
                        "project": project,
                        "messages": json_messages,
                    });
                    println!("{}", serde_json::to_string_pretty(&output)?);
                }
                ExportFormat::Text => {
                    println!("Session: {session_id}");
                    println!("Project: {project}");
                    println!();
                    for m in &messages {
                        let ts = m.timestamp.as_deref().unwrap_or("");
                        println!("[{}] {} {}", m.role, ts, m.content);
                        println!();
                    }
                }
            }
        }
        Commands::List {
            limit,
            project,
            since,
            until,
            json,
        } => {
            let limit = if limit == 0 { usize::MAX } else { limit };
            let sessions = db::list_sessions(
                &conn,
                limit,
                project.as_deref(),
                since.as_deref(),
                until.as_deref(),
            )?;
            if json {
                let json_sessions: Vec<serde_json::Value> = sessions
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "session_id": s.session_id,
                            "project": format_project_name(&s.project),
                            "started_at": s.started_at,
                            "message_count": s.message_count,
                            "first_user_message": s.first_user_message,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_sessions)?);
                return Ok(());
            }
            if sessions.is_empty() {
                println!("No sessions found.");
                return Ok(());
            }
            println!(
                "{:<10} {:<22} {:>5}  {:<25} PREVIEW",
                "ID", "DATE", "MSGS", "PROJECT"
            );
            println!("{}", "-".repeat(100));
            for s in &sessions {
                let ts = s
                    .started_at
                    .as_deref()
                    .unwrap_or("unknown")
                    .get(..19)
                    .unwrap_or("unknown");
                let short_id = &s.session_id[..8.min(s.session_id.len())];
                let project = format_project_name(&s.project);
                let project_display: String = if project.chars().count() > 25 {
                    project.chars().take(22).collect::<String>() + "..."
                } else {
                    project
                };
                let preview = s
                    .first_user_message
                    .as_deref()
                    .unwrap_or("")
                    .replace('\n', " ");
                let preview: String = if preview.chars().count() > 50 {
                    preview.chars().take(50).collect::<String>() + "..."
                } else {
                    preview
                };
                println!(
                    "{:<10} {:<22} {:>5}  {:<25} {}",
                    short_id, ts, s.message_count, project_display, preview
                );
            }
            println!("\nExport: claude-vault export <ID>");
        }
        Commands::Delete { session_id, yes } => {
            let resolved_id = db::resolve_session_id(&conn, &session_id)?;
            let messages = db::get_session_messages(&conn, &resolved_id)?;
            let msg_count = messages.len();
            let project = messages
                .first()
                .map(|m| format_project_name(&m.project))
                .unwrap_or_default();

            if !yes {
                eprintln!("Delete session {resolved_id} ({project}, {msg_count} messages)? [y/N] ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    println!("Aborted.");
                    return Ok(());
                }
            }

            let deleted = db::delete_session(&conn, &resolved_id)?;
            println!("Deleted session {resolved_id} ({deleted} messages removed)");
        }
        Commands::Stats => {
            let (sessions, messages) = db::stats(&conn)?;
            println!("Database: {}", db_path.display());
            println!("Sessions: {}", sessions);
            println!("Messages: {}", messages);
        }
        Commands::Verify => {
            db::verify(&conn)?;
        }
        Commands::Completions { .. } => unreachable!(),
    }

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_tool_lines_removes_tool_use() {
        let input = "Here is my plan\n[tool_use: Edit] {\"file\":\"foo.rs\"}\nDone editing.";
        assert_eq!(strip_tool_lines(input), "Here is my plan\nDone editing.");
    }

    #[test]
    fn test_strip_tool_lines_preserves_plain_text() {
        let input = "Just a normal message\nwith multiple lines";
        assert_eq!(strip_tool_lines(input), input);
    }

    #[test]
    fn test_strip_tool_lines_all_tools_becomes_empty() {
        let input = "[tool_use: Bash] {\"command\":\"ls\"}\n[tool_use: Edit] {\"file\":\"x\"}";
        assert_eq!(strip_tool_lines(input), "");
    }

    #[test]
    fn test_strip_tool_lines_preserves_brackets_in_text() {
        let input = "Use [this] syntax for arrays\n[tool_use: Write] {\"file\":\"x\"}\nEnd";
        assert_eq!(strip_tool_lines(input), "Use [this] syntax for arrays\nEnd");
    }

    #[test]
    fn test_strip_tool_lines_empty_input() {
        assert_eq!(strip_tool_lines(""), "");
    }

    #[test]
    fn test_strip_tool_lines_single_tool_line() {
        let input = "[tool_use: Bash] {\"command\":\"cargo test\"}";
        assert_eq!(strip_tool_lines(input), "");
    }
}
