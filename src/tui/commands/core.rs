use anyhow::Result;

use super::{CommandHandler, CommandResult, OverlayContent, OverlayTab};
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
        if !args.is_empty() {
            return Ok(CommandResult::Message(format!(
                "Help for /{args}: not yet documented per-command. Use /help to see all categories."
            )));
        }

        // Claude-Code-style modal: title + tab strip + body.
        // `general` pane mirrors their layout — short pitch, then a 3-column
        // shortcuts grid. `commands` lists every registered slash command
        // with its description. The renderer reads these as plain strings
        // and styles them at draw time.
        let general = OverlayTab {
            label: "general".to_string(),
            body: vec![
                "Rantaiclaw understands your workspace, runs tools with your".to_string(),
                "approval, and chats with the agent — right from your terminal.".to_string(),
                String::new(),
                "Shortcuts".to_string(),
                "  /  for commands             Ctrl+Enter to send".to_string(),
                "  ↑/↓ scrolls chat history    Ctrl+C cancels a running turn".to_string(),
                "  Tab completes a command     Ctrl+D quits".to_string(),
                "  Esc closes this overlay     Ctrl+L clears the screen".to_string(),
                String::new(),
                "Tips".to_string(),
                "  • Type / and keep typing — the dropdown filters live.".to_string(),
                "  • /retry re-runs the previous prompt against a fresh model call.".to_string(),
                "  • /sessions resumes any past conversation by id.".to_string(),
                "  • /doctor checks config + provider + channel health.".to_string(),
                String::new(),
                "For more help: https://github.com/RantAI-dev/RantAIClaw".to_string(),
            ],
        };

        let commands = OverlayTab {
            label: "commands".to_string(),
            body: vec![
                "Core".to_string(),
                "  /help               Show this overlay".to_string(),
                "  /quit, /exit        Exit the application".to_string(),
                "  /new, /clear        Start a new session".to_string(),
                String::new(),
                "Model & usage".to_string(),
                "  /model [name]       Change or show current model".to_string(),
                "  /usage              Show token usage statistics".to_string(),
                String::new(),
                "Session".to_string(),
                "  /sessions           List past sessions".to_string(),
                "  /resume <id>        Resume a session".to_string(),
                "  /search <query>     Search message history".to_string(),
                "  /title <name>       Set session title".to_string(),
                "  /insights           Show session analytics".to_string(),
                String::new(),
                "Conversation".to_string(),
                "  /retry              Retry last response".to_string(),
                "  /undo               Remove last exchange".to_string(),
                "  /stop               Cancel streaming".to_string(),
                String::new(),
                "Memory & skills".to_string(),
                "  /memory [action]    Manage persistent memory".to_string(),
                "  /forget <key>       Remove a memory entry".to_string(),
                "  /compress           Compress context".to_string(),
                "  /skills             List available skills".to_string(),
                "  /skill <name>       Run a skill".to_string(),
                "  /personality        Set agent personality".to_string(),
                String::new(),
                "Config & diagnostics".to_string(),
                "  /status             Show system status".to_string(),
                "  /config [k] [v]     View or set config".to_string(),
                "  /debug              Toggle debug mode".to_string(),
                "  /doctor             Run diagnostics".to_string(),
                "  /platforms          Show connected platforms".to_string(),
                "  /cron [action]      Manage scheduled tasks".to_string(),
            ],
        };

        Ok(CommandResult::Overlay(OverlayContent {
            title: format!("Rantaiclaw v{}", env!("CARGO_PKG_VERSION")),
            tabs: vec![general, commands],
            active_tab: 0,
        }))
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
    fn test_context() -> TuiContext {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx
    }

    #[test]
    fn help_command_returns_help_text() {
        let cmd = HelpCommand;
        let mut ctx = test_context();
        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Overlay(content) => {
                // /help now opens a modal overlay with at least two tabs
                // (general + commands). The commands tab must mention the
                // basics so users can grep visually for what they need.
                assert!(content.tabs.len() >= 2, "expected ≥2 tabs");
                let body_text = content
                    .tabs
                    .iter()
                    .flat_map(|t| t.body.iter().cloned())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(body_text.contains("/help"), "body should reference /help");
                assert!(body_text.contains("/quit"), "body should reference /quit");
            }
            _ => panic!("Expected Overlay result, got {result:?}"),
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
