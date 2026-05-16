//! `/kanban …` slash command — invoked from the TUI and from any gateway
//! channel. Reuses the same clap subcommand tree the CLI does so behaviour
//! and flags agree by construction.

use clap::{CommandFactory, FromArgMatches, Parser};

use crate::kanban::cli::{handle_command, KanbanCommand};
use crate::kanban::errors::{KanbanError, Result};

#[derive(Parser, Debug)]
#[command(
    name = "/kanban",
    bin_name = "/kanban",
    no_binary_name = true,
    disable_help_subcommand = true
)]
struct SlashKanban {
    /// `--board <slug>` scopes a single invocation; otherwise the active
    /// board is used.
    #[arg(long)]
    board: Option<String>,
    #[command(subcommand)]
    cmd: KanbanCommand,
}

/// Parse a `/kanban …` invocation (the body after the leading slash word) and
/// dispatch it. Returns the textual output that would otherwise have been
/// printed.
pub fn run_slash(line: &str) -> Result<String> {
    let words = shell_words::split(line)
        .map_err(|e| KanbanError::InvalidStatus(format!("kanban slash parse: {e}")))?;
    let cmd = SlashKanban::command();
    let matches = cmd
        .clone()
        .try_get_matches_from(words)
        .map_err(|e| KanbanError::InvalidStatus(format!("kanban slash: {e}")))?;
    let parsed = SlashKanban::from_arg_matches(&matches)
        .map_err(|e| KanbanError::InvalidStatus(format!("kanban slash: {e}")))?;
    let mut buf = Vec::<u8>::new();
    handle_command(&parsed.cmd, parsed.board.as_deref(), &mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn slash_kanban_clap_tree_builds() {
        SlashKanban::command().debug_assert();
    }

    #[test]
    fn parses_create_with_quoted_title() {
        let words = shell_words::split("create \"hello world\" --priority 3").unwrap();
        let cmd = SlashKanban::command();
        let m = cmd.clone().try_get_matches_from(words);
        assert!(m.is_ok(), "{:?}", m.err());
    }

    #[test]
    fn parses_board_flag_first() {
        let words = shell_words::split("--board atm10 list").unwrap();
        let cmd = SlashKanban::command();
        let m = cmd.clone().try_get_matches_from(words);
        assert!(m.is_ok(), "{:?}", m.err());
    }
}
