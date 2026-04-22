use anyhow::Result;
use chrono::{TimeZone, Utc};

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

/// /sessions command — list past sessions
pub struct SessionsCommand;

impl CommandHandler for SessionsCommand {
    fn name(&self) -> &str {
        "sessions"
    }

    fn description(&self) -> &str {
        "List past sessions"
    }

    fn usage(&self) -> &str {
        "/sessions [--days N]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let limit = if args.contains("--days") { 100 } else { 20 };
        let sessions = ctx.session_store.list_sessions(limit)?;

        if sessions.is_empty() {
            return Ok(CommandResult::Message(
                "No past sessions found.".to_string(),
            ));
        }

        let mut lines = vec!["Past sessions:".to_string()];
        for s in &sessions {
            let date = Utc
                .timestamp_opt(s.started_at, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let title = s.title.as_deref().unwrap_or("(untitled)");
            let short_id = &s.id[..s.id.len().min(8)];
            lines.push(format!(
                "  {} | {} | {} msgs | {}",
                short_id, date, s.message_count, title
            ));
        }

        Ok(CommandResult::Message(lines.join("\n")))
    }
}

/// /resume command — resume a past session by ID prefix
pub struct ResumeCommand;

impl CommandHandler for ResumeCommand {
    fn name(&self) -> &str {
        "resume"
    }

    fn description(&self) -> &str {
        "Resume a past session by ID prefix"
    }

    fn usage(&self) -> &str {
        "/resume <id-prefix>"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let prefix = args.trim();
        if prefix.is_empty() {
            return Ok(CommandResult::Message(
                "Usage: /resume <id-prefix>".to_string(),
            ));
        }

        // Search for sessions whose ID starts with the given prefix
        let sessions = ctx.session_store.list_sessions(200)?;
        let matches: Vec<_> = sessions
            .iter()
            .filter(|s| s.id.starts_with(prefix))
            .collect();

        match matches.len() {
            0 => Ok(CommandResult::Message(format!(
                "No session found matching prefix: {}",
                prefix
            ))),
            1 => {
                let target = matches[0];
                // Load the full session to get the model
                let session = ctx
                    .session_store
                    .get_session(&target.id)?
                    .ok_or_else(|| anyhow::anyhow!("session disappeared during resume"))?;

                ctx.session_id = session.id.clone();
                ctx.model = session.model.clone();
                ctx.messages.clear();
                ctx.load_session_messages()?;

                Ok(CommandResult::Message(format!(
                    "Resumed session {} ({} messages)",
                    &session.id[..session.id.len().min(8)],
                    ctx.messages.len()
                )))
            }
            _ => {
                let mut msg = format!(
                    "Multiple sessions match prefix '{}'. Please use a longer prefix:\n",
                    prefix
                );
                for s in &matches {
                    let short_id = &s.id[..s.id.len().min(8)];
                    let title = s.title.as_deref().unwrap_or("(untitled)");
                    use std::fmt::Write as _;
                    writeln!(msg, "  {} — {}", short_id, title).unwrap();
                }
                Ok(CommandResult::Message(msg.trim_end().to_string()))
            }
        }
    }
}

/// /search command — full-text search across message history
pub struct SearchCommand;

impl CommandHandler for SearchCommand {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "Search message history"
    }

    fn usage(&self) -> &str {
        "/search <query>"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let query = args.trim();
        if query.is_empty() {
            return Ok(CommandResult::Message("Usage: /search <query>".to_string()));
        }

        let results = ctx.session_store.search(query, 10)?;

        if results.is_empty() {
            return Ok(CommandResult::Message(format!(
                "No messages found for: {}",
                query
            )));
        }

        let mut lines = vec![format!("Search results for '{}':", query)];
        for r in &results {
            let date = Utc
                .timestamp_opt(r.timestamp, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let short_id = &r.session_id[..r.session_id.len().min(8)];
            let preview: String = r.content.chars().take(60).collect();
            let preview = if r.content.len() > 60 {
                format!("{}...", preview)
            } else {
                preview
            };
            lines.push(format!(
                "  [{}] {} | {} | {}",
                short_id, date, r.role, preview
            ));
        }

        Ok(CommandResult::Message(lines.join("\n")))
    }
}

/// /title command — set a title on the current session
pub struct TitleCommand;

impl CommandHandler for TitleCommand {
    fn name(&self) -> &str {
        "title"
    }

    fn description(&self) -> &str {
        "Set a title for the current session"
    }

    fn usage(&self) -> &str {
        "/title <name>"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let title = args.trim();
        if title.is_empty() {
            return Ok(CommandResult::Message("Usage: /title <name>".to_string()));
        }

        ctx.session_store.set_title(&ctx.session_id, title)?;

        Ok(CommandResult::Message(format!(
            "Session title set to: {}",
            title
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> TuiContext {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx
    }

    #[test]
    fn sessions_command_lists_sessions() {
        let mut ctx = test_context();

        // The current session already exists; add a message so it shows up with count
        ctx.append_user_message("hello").unwrap();

        let cmd = SessionsCommand;
        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Past sessions:"));
                // Should show the current session
                assert!(msg.contains("msgs"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn search_command_finds_messages() {
        let mut ctx = test_context();

        ctx.append_user_message("the quick brown fox").unwrap();
        ctx.append_assistant_message("an unrelated reply").unwrap();

        let cmd = SearchCommand;
        let result = cmd.execute("quick", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("quick"));
                assert!(msg.contains("the quick brown fox"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn title_command_sets_title() {
        let mut ctx = test_context();

        let cmd = TitleCommand;
        let result = cmd.execute("my-session-title", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("my-session-title"));
            }
            _ => panic!("Expected Message result"),
        }

        // Verify it was actually persisted
        let session = ctx
            .session_store
            .get_session(&ctx.session_id)
            .unwrap()
            .unwrap();
        assert_eq!(session.title.as_deref(), Some("my-session-title"));
    }
}
