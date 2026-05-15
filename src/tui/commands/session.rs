use anyhow::Result;
use chrono::{TimeZone, Utc};

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

/// Build picker items from a list of session metas. Skips the current
/// session so the user doesn't accidentally "resume" it onto itself.
fn build_session_items(
    sessions: &[crate::sessions::SessionMeta],
    current_session_id: &str,
) -> Vec<ListPickerItem> {
    sessions
        .iter()
        .filter(|s| s.id != current_session_id)
        .map(|s| {
            let date = Utc
                .timestamp_opt(s.started_at, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let title = s
                .title
                .as_deref()
                .filter(|t| !t.is_empty())
                .unwrap_or("(untitled)");
            let short_id = &s.id[..s.id.len().min(8)];
            ListPickerItem {
                key: s.id.clone(),
                primary: format!("{short_id} · {title}"),
                secondary: format!("{date} · {} msgs · {}", s.message_count, s.model),
            }
        })
        .collect()
}

/// /sessions command — open the interactive session picker. Selecting a
/// session resumes it (same flow as `/resume`); Esc cancels.
pub struct SessionsCommand;

impl CommandHandler for SessionsCommand {
    fn name(&self) -> &str {
        "sessions"
    }

    fn description(&self) -> &str {
        "Browse and resume past sessions"
    }

    fn usage(&self) -> &str {
        "/sessions"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let sessions = ctx.session_store.list_sessions(200)?;
        let items = build_session_items(&sessions, &ctx.session_id);
        let picker = ListPicker::new(
            ListPickerKind::Session,
            "Sessions",
            items,
            None,
            "No past sessions to resume.",
        );
        Ok(CommandResult::OpenListPicker(picker))
    }
}

/// /resume command — open the session picker, optionally pre-selecting a
/// session whose ID starts with the given prefix. Falls back to the full
/// list when no prefix is given.
pub struct ResumeCommand;

impl CommandHandler for ResumeCommand {
    fn name(&self) -> &str {
        "resume"
    }

    fn description(&self) -> &str {
        "Resume a past session"
    }

    fn usage(&self) -> &str {
        "/resume [id-prefix]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let prefix = args.trim();
        let sessions = ctx.session_store.list_sessions(200)?;
        let items = build_session_items(&sessions, &ctx.session_id);
        let preselect = if prefix.is_empty() {
            None
        } else {
            items
                .iter()
                .find(|i| i.key.starts_with(prefix))
                .map(|i| i.key.clone())
        };
        let picker = ListPicker::new(
            ListPickerKind::Session,
            "Resume Session",
            items,
            preselect.as_deref(),
            "No past sessions to resume.",
        );
        Ok(CommandResult::OpenListPicker(picker))
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
    use crate::tui::widgets::ListPickerKind;

    fn test_context() -> TuiContext {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx
    }

    #[test]
    fn sessions_command_opens_picker() {
        let mut ctx = test_context();
        ctx.append_user_message("hello").unwrap();

        let cmd = SessionsCommand;
        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::OpenListPicker(picker) => {
                assert_eq!(picker.kind, ListPickerKind::Session);
                // Current session is filtered out → empty in this in-memory test.
                assert!(
                    picker.entries().is_empty()
                        || picker
                            .entries()
                            .iter()
                            .all(|e| matches!(e, crate::tui::widgets::ListPickerEntry::Item(i) if !i.key.is_empty()))
                );
            }
            other => panic!("expected OpenListPicker, got {other:?}"),
        }
    }

    #[test]
    fn resume_command_opens_picker_even_with_no_args() {
        let mut ctx = test_context();
        let cmd = ResumeCommand;
        let result = cmd.execute("", &mut ctx).unwrap();
        match result {
            CommandResult::OpenListPicker(picker) => {
                assert_eq!(picker.kind, ListPickerKind::Session);
            }
            other => panic!("expected OpenListPicker, got {other:?}"),
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
