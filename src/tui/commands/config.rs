use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

/// /status command — show a summary of the current TUI session state
pub struct StatusCommand;

impl CommandHandler for StatusCommand {
    fn name(&self) -> &str {
        "status"
    }

    fn description(&self) -> &str {
        "Show current session and agent status"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let session_short = &ctx.session_id[..ctx.session_id.len().min(8)];
        let total_sessions = ctx.session_store.list_sessions(1000)?.len();

        Ok(CommandResult::Message(format!(
            "Status:\n  Model: {}\n  Session: {}\n  Messages: {}\n  Total sessions: {}\n  Debug mode: {}",
            ctx.model,
            session_short,
            ctx.messages.len(),
            total_sessions,
            ctx.debug_mode,
        )))
    }
}

/// /debug command — toggle debug mode on or off
pub struct DebugCommand;

impl CommandHandler for DebugCommand {
    fn name(&self) -> &str {
        "debug"
    }

    fn description(&self) -> &str {
        "Toggle debug mode"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        ctx.debug_mode = !ctx.debug_mode;
        let state = if ctx.debug_mode { "on" } else { "off" };
        Ok(CommandResult::Message(format!("Debug mode: {}", state)))
    }
}

/// /config command — inspect or set runtime configuration keys
pub struct ConfigCommand;

impl CommandHandler for ConfigCommand {
    fn name(&self) -> &str {
        "config"
    }

    fn description(&self) -> &str {
        "Inspect or set configuration values"
    }

    fn usage(&self) -> &str {
        "/config [key] [value]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let parts: Vec<&str> = args.split_whitespace().collect();

        match parts.len() {
            0 => {
                // Show all known config keys
                Ok(CommandResult::Message(format!(
                    "Config:\n  model = {}\n  debug = {}",
                    ctx.model, ctx.debug_mode
                )))
            }
            1 => {
                // Show a specific key
                let key = parts[0];
                match key {
                    "model" => Ok(CommandResult::Message(format!("model = {}", ctx.model))),
                    "debug" => Ok(CommandResult::Message(format!(
                        "debug = {}",
                        ctx.debug_mode
                    ))),
                    _ => Ok(CommandResult::Message(format!(
                        "Unknown config key: {}",
                        key
                    ))),
                }
            }
            _ => {
                // Set key = value (everything after the key is the value)
                let key = parts[0];
                let value = parts[1..].join(" ");
                match key {
                    "model" => {
                        ctx.model = value.clone();
                        Ok(CommandResult::Message(format!("model set to: {}", value)))
                    }
                    "debug" => {
                        let enabled = matches!(value.as_str(), "true" | "1" | "on");
                        ctx.debug_mode = enabled;
                        Ok(CommandResult::Message(format!(
                            "debug set to: {}",
                            ctx.debug_mode
                        )))
                    }
                    _ => Ok(CommandResult::Message(format!(
                        "Unknown config key: {}",
                        key
                    ))),
                }
            }
        }
    }
}

/// /doctor command — run basic health checks on the TUI environment
pub struct DoctorCommand;

impl CommandHandler for DoctorCommand {
    fn name(&self) -> &str {
        "doctor"
    }

    fn description(&self) -> &str {
        "Run diagnostics and health checks"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let mut checks: Vec<(String, bool)> = Vec::new();

        // Check session store
        let store_ok = ctx.session_store.list_sessions(1).is_ok();
        checks.push(("Session store".to_string(), store_ok));

        // Check model configured
        let model_ok = !ctx.model.is_empty();
        checks.push(("Model configured".to_string(), model_ok));

        // Check TUI running (trivially true here since we are executing)
        checks.push(("TUI running".to_string(), true));

        let mut lines = vec!["Doctor checks:".to_string()];
        let mut all_ok = true;
        for (label, ok) in &checks {
            let icon = if *ok { "OK" } else { "FAIL" };
            lines.push(format!("  [{}] {}", icon, label));
            if !ok {
                all_ok = false;
            }
        }

        if all_ok {
            lines.push("All checks passed.".to_string());
        } else {
            lines.push("Some checks failed. Review the output above.".to_string());
        }

        Ok(CommandResult::Message(lines.join("\n")))
    }
}

/// /channels command — show installed channels and whether they're being
/// polled by this process. Pre-v0.6.4 the TUI did not auto-start channel
/// listeners, so a configured Telegram bot would never reply unless the
/// user separately ran `rantaiclaw daemon`. v0.6.4 spawns listeners
/// alongside the TUI; this command makes that visible.
pub struct ChannelsCommand;

fn render_channels(ctx: &TuiContext) -> String {
    use crate::channels::auto_start_state::{snapshot, AutoStartState};

    let rows: Vec<(&str, bool)> = ctx
        .channels_summary
        .iter()
        .map(|(n, c)| (n.as_str(), *c))
        .collect();

    let mut out = String::new();
    out.push_str("Channels (transports the agent can speak on):\n\n");
    out.push_str("  ✅ CLI / TUI — always available (this terminal)\n\n");

    // Surface the actual auto-start state, not just "we dispatched".
    // Pre-v0.6.6 the table reported "polling" purely because spawn was
    // dispatched, even when start_channels errored mid-build. The user
    // saw "polling" and assumed everything was wired; in reality the
    // listener never made a single getUpdates call.
    let (status_label, footer_hint) = match snapshot() {
        AutoStartState::NotDispatched => (
            "configured · not started in this process",
            Some(
                "TUI launched without auto-start (no channels were configured at startup, \
                 or this build pre-dates v0.6.4). Restart `rantaiclaw` to pick up the \
                 channel-start path.",
            ),
        ),
        AutoStartState::Starting { since_unix } => {
            let elapsed = now_unix().saturating_sub(since_unix);
            if elapsed < 5 {
                ("starting…", None)
            } else {
                ("running", None)
            }
        }
        AutoStartState::Terminated { .. } => (
            "stopped (dispatch loop exited)",
            Some(
                "The channel runtime exited cleanly. This usually means a graceful \
                 shutdown was requested. Restart `rantaiclaw` to bring it back.",
            ),
        ),
        AutoStartState::Failed { ref message, .. } => (
            "FAILED — see error below",
            Some(message.as_str()),
        ),
    };

    let configured: Vec<_> = rows.iter().filter(|(_, c)| *c).collect();
    let not_configured: Vec<_> = rows.iter().filter(|(_, c)| !*c).collect();

    if configured.is_empty() {
        out.push_str(
            "  No external channels configured.\n  \
             Run `/setup channels` to add Telegram, Discord, Slack, etc.\n",
        );
    } else {
        out.push_str("  Configured:\n");
        for (name, _) in &configured {
            out.push_str(&format!("    · {name:<16} {status_label}\n"));
        }
    }

    if !not_configured.is_empty() {
        out.push_str("\n  Not configured:\n    ");
        let names: Vec<&str> = not_configured.iter().map(|(n, _)| *n).collect();
        out.push_str(&names.join(", "));
        out.push('\n');
    }

    if let Some(hint) = footer_hint {
        out.push_str("\nNote:\n");
        for line in hint.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }

    out.push_str("\nLogs: `~/.rantaiclaw/logs/tui-YYYY-MM-DD.log` (search for `auto-start` and `Compiling`).\n");
    out.push_str("Use `/setup channels` to add or reconfigure.\n");
    out
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl CommandHandler for ChannelsCommand {
    fn name(&self) -> &str {
        "channels"
    }

    fn description(&self) -> &str {
        "Show installed and active channels (Telegram, Discord, etc.)"
    }

    fn aliases(&self) -> Vec<&str> {
        vec!["platforms"]
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        Ok(CommandResult::Message(render_channels(ctx)))
    }
}

/// /platforms command — kept as an alias for /channels so existing
/// muscle memory works; the command registry also exposes /platforms
/// via ChannelsCommand::aliases().
pub struct PlatformsCommand;

impl CommandHandler for PlatformsCommand {
    fn name(&self) -> &str {
        "platforms"
    }

    fn description(&self) -> &str {
        "Alias for /channels — show installed and active channels"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        ChannelsCommand.execute(args, ctx)
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
    fn status_command_shows_info() {
        let cmd = StatusCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Model"));
                assert!(msg.contains("Session"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn debug_command_toggles_mode() {
        let cmd = DebugCommand;
        let mut ctx = test_context();

        assert!(!ctx.debug_mode);

        cmd.execute("", &mut ctx).unwrap();
        assert!(ctx.debug_mode);

        cmd.execute("", &mut ctx).unwrap();
        assert!(!ctx.debug_mode);
    }

    #[test]
    fn config_command_sets_values() {
        let cmd = ConfigCommand;
        let mut ctx = test_context();

        let result = cmd.execute("model new-model", &mut ctx).unwrap();

        assert_eq!(ctx.model, "new-model");
        assert!(matches!(result, CommandResult::Message(_)));
    }
}
