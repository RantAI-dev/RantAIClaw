use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

/// /retry command — remove the last assistant message to allow re-generation
pub struct RetryCommand;

impl CommandHandler for RetryCommand {
    fn name(&self) -> &str {
        "retry"
    }

    fn description(&self) -> &str {
        "Remove the last assistant message for re-generation"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let pos = ctx.messages.iter().rposition(|m| m.role == "assistant");

        match pos {
            Some(idx) => {
                ctx.messages.remove(idx);
                Ok(CommandResult::Message(
                    "Last assistant message removed. Resend your message to retry.".to_string(),
                ))
            }
            None => Ok(CommandResult::Message(
                "No assistant message to remove.".to_string(),
            )),
        }
    }
}

/// /undo command — remove the last assistant message and the last user message
pub struct UndoCommand;

impl CommandHandler for UndoCommand {
    fn name(&self) -> &str {
        "undo"
    }

    fn description(&self) -> &str {
        "Remove the last assistant and user message exchange"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let mut removed = 0;

        // Remove last assistant message first
        if let Some(idx) = ctx.messages.iter().rposition(|m| m.role == "assistant") {
            ctx.messages.remove(idx);
            removed += 1;
        }

        // Then remove last user message
        if let Some(idx) = ctx.messages.iter().rposition(|m| m.role == "user") {
            ctx.messages.remove(idx);
            removed += 1;
        }

        if removed == 0 {
            Ok(CommandResult::Message("No messages to undo.".to_string()))
        } else {
            Ok(CommandResult::Message(format!(
                "Removed {} message(s).",
                removed
            )))
        }
    }
}

/// /stop command — signal intent to cancel ongoing streaming (future integration)
pub struct StopCommand;

impl CommandHandler for StopCommand {
    fn name(&self) -> &str {
        "stop"
    }

    fn description(&self) -> &str {
        "Stop ongoing agent generation"
    }

    fn execute(&self, _args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        Ok(CommandResult::Message(
            "Stop command received. (Streaming cancellation will be integrated with agent loop)"
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionStore;

    fn test_context() -> TuiContext {
        let store = SessionStore::in_memory().unwrap();
        TuiContext::new(store, "test-model", None).unwrap()
    }

    #[test]
    fn retry_removes_last_assistant_message() {
        let cmd = RetryCommand;
        let mut ctx = test_context();

        ctx.append_user_message("hello").unwrap();
        ctx.append_assistant_message("world").unwrap();
        assert_eq!(ctx.messages.len(), 2);

        let result = cmd.execute("", &mut ctx).unwrap();

        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].role, "user");
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[test]
    fn undo_removes_last_exchange() {
        let cmd = UndoCommand;
        let mut ctx = test_context();

        ctx.append_user_message("first question").unwrap();
        ctx.append_assistant_message("first answer").unwrap();
        ctx.append_user_message("second question").unwrap();
        ctx.append_assistant_message("second answer").unwrap();
        assert_eq!(ctx.messages.len(), 4);

        let result = cmd.execute("", &mut ctx).unwrap();

        // Both the last assistant and last user message should be removed
        assert_eq!(ctx.messages.len(), 2);
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("2"));
            }
            _ => panic!("Expected Message result"),
        }
    }
}
