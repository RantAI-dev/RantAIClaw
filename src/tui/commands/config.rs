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

/// /platforms command — show which communication platforms are active
pub struct PlatformsCommand;

impl CommandHandler for PlatformsCommand {
    fn name(&self) -> &str {
        "platforms"
    }

    fn description(&self) -> &str {
        "Show active communication platforms"
    }

    fn execute(&self, _args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        Ok(CommandResult::Message(
            "Active platforms:\n  TUI (terminal user interface) — active".to_string(),
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
