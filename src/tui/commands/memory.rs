use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

/// /memory command — add, list, or remove memory entries
pub struct MemoryCommand;

impl CommandHandler for MemoryCommand {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Add, list, or remove memory entries"
    }

    fn usage(&self) -> &str {
        "/memory [add|list|remove] [args]"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        let subcmd = parts.first().copied().unwrap_or("");

        match subcmd {
            "add" | "list" | "remove" => Ok(CommandResult::Message(
                "Integration with memory backend pending".to_string(),
            )),
            _ => Ok(CommandResult::Message(
                "Usage: /memory [add|list|remove] [args]\n\nManage persistent memory entries."
                    .to_string(),
            )),
        }
    }
}

/// /forget command — remove a specific memory entry by key
pub struct ForgetCommand;

impl CommandHandler for ForgetCommand {
    fn name(&self) -> &str {
        "forget"
    }

    fn description(&self) -> &str {
        "Remove a specific memory entry by key"
    }

    fn usage(&self) -> &str {
        "/forget <key>"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let key = args.trim();
        if key.is_empty() {
            return Ok(CommandResult::Message("Usage: /forget <key>".to_string()));
        }

        Ok(CommandResult::Message(format!(
            "Forget '{}': Integration with memory backend pending",
            key
        )))
    }
}

/// /compress command — summarize and compress the current context window
pub struct CompressCommand;

impl CommandHandler for CompressCommand {
    fn name(&self) -> &str {
        "compress"
    }

    fn description(&self) -> &str {
        "Compress the current context by summarizing older messages"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let count = ctx.messages.len();
        if count < 10 {
            return Ok(CommandResult::Message(
                "Context is small enough, no compression needed.".to_string(),
            ));
        }

        Ok(CommandResult::Message(format!(
            "Context compression: {} messages would be summarized. Full integration pending.",
            count
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionStore;

    fn test_context() -> TuiContext {
        let store = SessionStore::in_memory().unwrap();
        TuiContext::new(store, "test", None).unwrap()
    }

    #[test]
    fn memory_command_shows_usage() {
        let cmd = MemoryCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Usage"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn memory_command_add_returns_pending() {
        let cmd = MemoryCommand;
        let mut ctx = test_context();

        let result = cmd.execute("add some-key value", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("pending"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn forget_command_shows_usage_on_empty_args() {
        let cmd = ForgetCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Usage"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn forget_command_returns_key_in_message() {
        let cmd = ForgetCommand;
        let mut ctx = test_context();

        let result = cmd.execute("some-key", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("some-key"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn compress_command_skips_small_context() {
        let cmd = CompressCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("small enough"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn compress_command_reports_large_context() {
        let cmd = CompressCommand;
        let mut ctx = test_context();

        for i in 0..10 {
            ctx.append_user_message(&format!("message {}", i)).unwrap();
        }

        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("messages would be summarized"));
            }
            _ => panic!("Expected Message result"),
        }
    }
}
