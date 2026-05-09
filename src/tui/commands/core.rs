use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

/// /help command — open an interactive command picker. With no args,
/// shows every registered command (filterable, paginated). Selecting a
/// command pre-fills `/<name> ` into the input buffer so the user can
/// type args and submit. With a name arg, shows a one-line description
/// of that specific command (or a "no such command" message).
pub struct HelpCommand;

impl CommandHandler for HelpCommand {
    fn name(&self) -> &str {
        "help"
    }
    fn description(&self) -> &str {
        "Browse commands"
    }
    fn usage(&self) -> &str {
        "/help [command]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let needle = args.trim().trim_start_matches('/');
        if !needle.is_empty() {
            // Show details for a specific command.
            let found = ctx
                .available_commands
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(needle));
            return match found {
                Some((name, desc)) => Ok(CommandResult::Message(format!("/{name} — {desc}"))),
                None => Ok(CommandResult::Message(format!(
                    "No command named '/{needle}'. Run /help to see all commands."
                ))),
            };
        }

        // No args → open the interactive picker over all registered
        // commands. The startup snapshot in `available_commands` keeps
        // command-handler trait signatures unchanged (no need to pass
        // the registry in here).
        let items: Vec<ListPickerItem> = ctx
            .available_commands
            .iter()
            .map(|(name, desc)| ListPickerItem {
                key: name.clone(),
                primary: format!("/{name}"),
                secondary: desc.clone(),
            })
            .collect();
        let picker = ListPicker::new(
            ListPickerKind::Help,
            "Commands",
            items,
            None,
            "No commands registered.",
        );
        Ok(CommandResult::OpenListPicker(picker))
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
        Ok(CommandResult::ClearTerminal(
            "Started new session".to_string(),
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
    fn help_command_opens_command_picker() {
        let cmd = HelpCommand;
        let mut ctx = test_context();
        // Seed the snapshot the picker reads from.
        ctx.available_commands = vec![
            ("help".to_string(), "Browse commands".to_string()),
            ("quit".to_string(), "Exit the application".to_string()),
            (
                "model".to_string(),
                "Pick or change the active model".to_string(),
            ),
        ];

        let result = cmd.execute("", &mut ctx).unwrap();
        match result {
            CommandResult::OpenListPicker(picker) => {
                assert_eq!(picker.kind, crate::tui::widgets::ListPickerKind::Help);
                assert_eq!(picker.entries().len(), 3);
                assert!(picker
                    .entries()
                    .iter()
                    .any(|e| matches!(e, ListPickerEntry::Item(i) if i.key == "quit")));
                assert!(picker
                    .entries()
                    .iter()
                    .any(|e| matches!(e, ListPickerEntry::Item(i) if i.primary == "/quit")));
            }
            other => panic!("Expected OpenListPicker, got {other:?}"),
        }
    }

    #[test]
    fn help_command_with_arg_shows_one_line_description() {
        let cmd = HelpCommand;
        let mut ctx = test_context();
        ctx.available_commands = vec![(
            "model".to_string(),
            "Pick or change the active model".to_string(),
        )];

        let result = cmd.execute("model", &mut ctx).unwrap();
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("/model"));
                assert!(msg.contains("Pick or change"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn help_command_with_unknown_arg_returns_not_found() {
        let cmd = HelpCommand;
        let mut ctx = test_context();
        let result = cmd.execute("nonexistent", &mut ctx).unwrap();
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.to_lowercase().contains("no command"));
                assert!(msg.contains("nonexistent"));
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
    fn new_command_creates_new_session_and_signals_terminal_clear() {
        let cmd = NewCommand;
        let mut ctx = test_context();
        let old_id = ctx.session_id.clone();

        let result = cmd.execute("", &mut ctx).unwrap();

        assert_ne!(ctx.session_id, old_id);
        assert!(ctx.messages.is_empty());
        match result {
            CommandResult::ClearTerminal(msg) => {
                assert!(msg.to_lowercase().contains("new session"));
            }
            other => panic!("Expected ClearTerminal, got {other:?}"),
        }
    }
}
