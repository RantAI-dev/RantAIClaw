use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

/// /help command
pub struct HelpCommand;

impl CommandHandler for HelpCommand {
    fn name(&self) -> &str {
        "help"
    }
    fn description(&self) -> &str {
        "Show help for commands"
    }
    fn usage(&self) -> &str {
        "/help [command]"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        if args.is_empty() {
            let help_text = r#"Available commands:
  /help [cmd]     Show help
  /quit, /exit    Exit the application
  /new, /clear    Start a new session
  /model [name]   Change model
  /sessions       List past sessions
  /resume <id>    Resume a session
  /search <query> Search message history

Press Ctrl+Enter to send a message.
Press Ctrl+C to quit."#;
            Ok(CommandResult::Message(help_text.to_string()))
        } else {
            Ok(CommandResult::Message(format!(
                "Help for /{}: Not yet implemented",
                args
            )))
        }
    }
}

/// /quit command
pub struct QuitCommand;

impl CommandHandler for QuitCommand {
    fn name(&self) -> &str {
        "quit"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["exit"]
    }
    fn description(&self) -> &str {
        "Exit the application"
    }

    fn execute(&self, _args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        Ok(CommandResult::Quit)
    }
}

/// /new command
pub struct NewCommand;

impl CommandHandler for NewCommand {
    fn name(&self) -> &str {
        "new"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["clear"]
    }
    fn description(&self) -> &str {
        "Start a new session"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        ctx.clear_session()?;
        Ok(CommandResult::Message("Started new session".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionStore;

    fn test_context() -> TuiContext {
        let store = SessionStore::in_memory().expect("in-memory store");
        TuiContext::new(store, "test-model", None).expect("context creation")
    }

    #[test]
    fn help_command_returns_help_text() {
        let cmd = HelpCommand;
        let mut ctx = test_context();
        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("/help"));
                assert!(msg.contains("/quit"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn quit_command_returns_quit() {
        let cmd = QuitCommand;
        let mut ctx = test_context();
        let result = cmd.execute("", &mut ctx).unwrap();
        assert!(matches!(result, CommandResult::Quit));
    }

    #[test]
    fn new_command_creates_new_session() {
        let cmd = NewCommand;
        let mut ctx = test_context();
        let old_id = ctx.session_id.clone();

        cmd.execute("", &mut ctx).unwrap();

        assert_ne!(ctx.session_id, old_id);
        assert!(ctx.messages.is_empty());
    }
}
