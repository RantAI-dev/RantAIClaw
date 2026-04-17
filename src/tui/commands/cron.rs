use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

/// /cron command — manage scheduled tasks
pub struct CronCommand;

impl CommandHandler for CronCommand {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Manage scheduled tasks"
    }

    fn usage(&self) -> &str {
        "/cron [add|remove|pause|resume|list] [args]"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        let subcmd = parts.first().copied().unwrap_or("");

        match subcmd {
            "add" => {
                // add requires at least 2 more parts: <schedule> <task>
                if parts.len() < 3 {
                    return Ok(CommandResult::Message(
                        "Usage: /cron add <schedule> <task>".to_string(),
                    ));
                }
                Ok(CommandResult::Message(
                    "Cron add: Integration with cron scheduler pending.".to_string(),
                ))
            }
            "remove" => {
                if parts.len() < 2 {
                    return Ok(CommandResult::Message(
                        "Usage: /cron remove <id>".to_string(),
                    ));
                }
                Ok(CommandResult::Message(
                    "Cron remove: Integration with cron scheduler pending.".to_string(),
                ))
            }
            "pause" => Ok(CommandResult::Message(
                "Cron pause: Integration with cron scheduler pending.".to_string(),
            )),
            "resume" => Ok(CommandResult::Message(
                "Cron resume: Integration with cron scheduler pending.".to_string(),
            )),
            "list" | "" => Ok(CommandResult::Message(
                "Scheduled tasks:\n  (No cron jobs configured)\n\nUse /cron add <schedule> <task> to create one.".to_string(),
            )),
            unknown => Ok(CommandResult::Message(format!(
                "Unknown cron subcommand: {}\n\nUsage: /cron [add|remove|pause|resume|list] [args]",
                unknown
            ))),
        }
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
    fn cron_command_lists_jobs() {
        let cmd = CronCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Scheduled tasks"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn cron_add_shows_usage_without_args() {
        let cmd = CronCommand;
        let mut ctx = test_context();

        let result = cmd.execute("add", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Usage"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn cron_add_returns_pending_with_args() {
        let cmd = CronCommand;
        let mut ctx = test_context();

        let result = cmd.execute("add 0 * * * * run-task", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("pending"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn cron_remove_shows_usage_without_args() {
        let cmd = CronCommand;
        let mut ctx = test_context();

        let result = cmd.execute("remove", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Usage"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn cron_list_subcommand_shows_scheduled_tasks() {
        let cmd = CronCommand;
        let mut ctx = test_context();

        let result = cmd.execute("list", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Scheduled tasks"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn cron_unknown_subcommand_shows_error() {
        let cmd = CronCommand;
        let mut ctx = test_context();

        let result = cmd.execute("frobnicate", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Unknown cron subcommand"));
            }
            _ => panic!("Expected Message result"),
        }
    }
}
