use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

/// `/kanban …` — full parity with `rantaiclaw kanban …` on the CLI. The verb
/// stream is parsed by `crate::kanban::run_slash`, which routes through the
/// same clap subcommand tree the binary uses.
pub struct KanbanCommand;

impl CommandHandler for KanbanCommand {
    fn name(&self) -> &str {
        "kanban"
    }

    fn description(&self) -> &str {
        "Multi-agent kanban board (durable SQLite, parity with hermes kanban)"
    }

    fn usage(&self) -> &str {
        "/kanban <list|show|create|complete|block|unblock|comment|specify|...|boards <sub>>"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return Ok(CommandResult::Message(
                "Usage: /kanban <verb> [args]. Try /kanban list, /kanban show <id>, /kanban create \"<title>\" --assignee <profile>, /kanban boards list. See `rantaiclaw kanban --help` for the full surface.".to_string(),
            ));
        }
        match crate::kanban::run_slash(trimmed) {
            Ok(output) => Ok(CommandResult::Message(output.trim_end().to_string())),
            Err(e) => Ok(CommandResult::Message(format!("kanban: {e}"))),
        }
    }
}
