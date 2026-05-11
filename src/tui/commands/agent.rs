use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

/// /retry command — drop the last assistant reply and re-dispatch
/// the previous user message to the agent without making the user retype.
pub struct RetryCommand;

impl CommandHandler for RetryCommand {
    fn name(&self) -> &str {
        "retry"
    }

    fn description(&self) -> &str {
        "Re-run the last user message"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let Some(assistant_idx) = ctx.messages.iter().rposition(|m| m.role == "assistant") else {
            return Ok(CommandResult::Message(
                "No previous response to retry.".to_string(),
            ));
        };
        let Some(user_idx) = ctx.messages[..assistant_idx]
            .iter()
            .rposition(|m| m.role == "user")
        else {
            return Ok(CommandResult::Message(
                "No previous user message to retry.".to_string(),
            ));
        };

        let prompt = ctx.messages[user_idx].content.clone();
        ctx.messages.remove(assistant_idx);
        Ok(CommandResult::Resubmit(prompt))
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

/// /continue command — extend the current task with a fresh tool-call
/// budget after the agent hit `max_tool_iterations`. Submits a literal
/// "continue" message so the conversation history is preserved and
/// the agent picks up where it left off; the next turn gets its own
/// full budget (max_tool_iterations is per-turn). Mirrors Cursor's
/// "Continue" button behavior.
pub struct ContinueCommand;

impl CommandHandler for ContinueCommand {
    fn name(&self) -> &str {
        "continue"
    }

    fn description(&self) -> &str {
        "Continue the previous task with a fresh tool-call budget"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let extra = args.trim();
        // If the user types `/continue install the missing deps`, pass
        // that as steering. Plain `/continue` is the common case.
        let prompt = if extra.is_empty() {
            "Continue from where you left off. You have a fresh tool-call budget for this turn."
                .to_string()
        } else {
            format!("Continue from where you left off ({extra}). You have a fresh tool-call budget for this turn.")
        };
        Ok(CommandResult::Resubmit(prompt))
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

    fn test_context() -> TuiContext {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx
    }

    #[test]
    fn retry_drops_last_assistant_and_returns_resubmit_with_last_user_text() {
        let cmd = RetryCommand;
        let mut ctx = test_context();

        ctx.append_user_message("hello").unwrap();
        ctx.append_assistant_message("world").unwrap();
        assert_eq!(ctx.messages.len(), 2);

        let result = cmd.execute("", &mut ctx).unwrap();

        // The user message is retained; the assistant reply is dropped.
        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].role, "user");
        assert_eq!(ctx.messages[0].content, "hello");
        match result {
            CommandResult::Resubmit(text) => assert_eq!(text, "hello"),
            other => panic!("expected Resubmit, got {other:?}"),
        }
    }

    #[test]
    fn retry_with_no_assistant_reports_message() {
        let cmd = RetryCommand;
        let mut ctx = test_context();

        ctx.append_user_message("hello").unwrap();

        let result = cmd.execute("", &mut ctx).unwrap();
        assert_eq!(ctx.messages.len(), 1);
        match result {
            CommandResult::Message(msg) => assert!(msg.contains("No previous response")),
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn retry_with_no_messages_reports_message() {
        let cmd = RetryCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();
        assert!(ctx.messages.is_empty());
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[test]
    fn retry_uses_most_recent_user_when_multiple_turns() {
        let cmd = RetryCommand;
        let mut ctx = test_context();

        ctx.append_user_message("first").unwrap();
        ctx.append_assistant_message("ans1").unwrap();
        ctx.append_user_message("second").unwrap();
        ctx.append_assistant_message("ans2").unwrap();

        let result = cmd.execute("", &mut ctx).unwrap();

        // "ans2" is removed, "second" stays as the next prompt to resubmit.
        assert_eq!(ctx.messages.len(), 3);
        assert_eq!(ctx.messages[2].content, "second");
        match result {
            CommandResult::Resubmit(text) => assert_eq!(text, "second"),
            other => panic!("expected Resubmit, got {other:?}"),
        }
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
                assert!(msg.contains('2'));
            }
            _ => panic!("Expected Message result"),
        }
    }
}
