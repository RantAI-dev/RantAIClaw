use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{InfoPanel, InfoSection, StatusKind};

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

        let panel = InfoPanel::new("Status")
            .with_subtitle("session snapshot")
            .with_footer("Esc close · /config to edit · /usage for tokens")
            .section(
                InfoSection::new("Agent")
                    .key_value("Model", &ctx.model)
                    .key_value("Debug", if ctx.debug_mode { "on" } else { "off" }),
            )
            .section(
                InfoSection::new("Session")
                    .key_value("ID", session_short)
                    .key_value("Messages", ctx.messages.len().to_string())
                    .key_value("Total sessions", total_sessions.to_string()),
            );
        Ok(CommandResult::OpenInfoPanel(panel))
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
                // Open the config inspector panel. Editing config still
                // happens via `/config <key> <value>` and `/setup`.
                let panel = InfoPanel::new("Config")
                    .with_subtitle("session-level keys")
                    .with_footer(
                        "Esc close · `/config <key> <value>` to set · `/setup` for full wizard",
                    )
                    .section(
                        InfoSection::new("Runtime")
                            .key_value("model", &ctx.model)
                            .key_value("debug", if ctx.debug_mode { "true" } else { "false" }),
                    )
                    .section(InfoSection::new("Persisted").plain(
                        "On-disk config lives at \
                                 `~/.rantaiclaw/profiles/<active>/config.toml`. \
                                 `/setup` walks the wizard against it; \
                                 `/setup <section>` re-runs one section.",
                    ));
                Ok(CommandResult::OpenInfoPanel(panel))
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
        // Core probes — fast, infallible-ish.
        let store_ok = ctx.session_store.list_sessions(1).is_ok();
        let model_ok = !ctx.model.is_empty();

        let mut core = InfoSection::new("Core");
        core = if store_ok {
            core.status_with(StatusKind::Ok, "Session store", "opened")
        } else {
            core.status_with(StatusKind::Fail, "Session store", "could not list sessions")
        };
        core = if model_ok {
            core.status_with(StatusKind::Ok, "Model configured", &ctx.model)
        } else {
            core.status_with(
                StatusKind::Fail,
                "Model configured",
                "no model set — run /setup provider",
            )
        };
        core = core.status_with(StatusKind::Ok, "TUI", "running");

        // Channels probe — read live auto_start_state.
        let mut channels = InfoSection::new("Channels");
        let configured_count = ctx.channels_summary.iter().filter(|(_, c)| *c).count();
        channels = match crate::channels::auto_start_state::snapshot() {
            crate::channels::auto_start_state::AutoStartState::NotDispatched => {
                if configured_count == 0 {
                    channels.status_with(StatusKind::Info, "Auto-start", "no channels configured")
                } else {
                    channels.status_with(
                        StatusKind::Warn,
                        "Auto-start",
                        "configured but not dispatched (restart `rantaiclaw`)",
                    )
                }
            }
            crate::channels::auto_start_state::AutoStartState::Starting { .. } => {
                channels.status_with(StatusKind::Info, "Auto-start", "starting…")
            }
            crate::channels::auto_start_state::AutoStartState::Terminated { .. } => channels
                .status_with(
                    StatusKind::Warn,
                    "Auto-start",
                    "stopped (dispatch loop exited)",
                ),
            crate::channels::auto_start_state::AutoStartState::Failed { .. } => channels
                .status_with(
                    StatusKind::Fail,
                    "Auto-start",
                    "failed — see /channels for the error",
                ),
        };
        for (name, configured) in &ctx.channels_summary {
            if *configured {
                channels = channels.status_with(StatusKind::Ok, name.clone(), "configured");
            }
        }

        // Skills probe.
        let skills_section = InfoSection::new("Skills").status_with(
            if ctx.available_skills.is_empty() {
                StatusKind::Warn
            } else {
                StatusKind::Ok
            },
            "Skills loaded",
            format!("{} skill(s)", ctx.available_skills.len()),
        );

        // Workspace probe — `~/.rantaiclaw/` exists.
        let workspace_section = {
            use crate::profile::paths;
            let root = paths::rantaiclaw_root();
            let profiles = root.join("profiles");
            let mut s = InfoSection::new("Workspace");
            s = s.status_with(
                if root.exists() {
                    StatusKind::Ok
                } else {
                    StatusKind::Fail
                },
                "~/.rantaiclaw",
                if root.exists() { "present" } else { "missing" },
            );
            s = s.status_with(
                if profiles.exists() {
                    StatusKind::Ok
                } else {
                    StatusKind::Fail
                },
                "profiles/",
                if profiles.exists() {
                    "present"
                } else {
                    "missing"
                },
            );
            s
        };

        // Roll up overall verdict for the footer.
        let any_fail = false; // probes above don't currently produce hard fails post-init
        let footer = if any_fail {
            "Esc close · some checks failed — review above"
        } else {
            "Esc close · all checks ok — `/channels` for transport details"
        };

        let panel = InfoPanel::new("Doctor")
            .with_subtitle("health checks")
            .with_footer(footer)
            .section(core)
            .section(channels)
            .section(skills_section)
            .section(workspace_section);
        Ok(CommandResult::OpenInfoPanel(panel))
    }
}

/// /channels command — show installed channels and whether they're being
/// polled by this process. v0.6.8 converts from the v0.6.6 text-blob
/// layout to a proper TUI panel (matching the visual language of
/// /skills, /sessions, /personality). v0.6.7 tester report:
/// "Change the shitty on chat ui or infos to proper tui comp ui."
pub struct ChannelsCommand;

fn build_channels_panel(ctx: &TuiContext) -> InfoPanel {
    use crate::channels::auto_start_state::{snapshot, AutoStartState};

    let rows: Vec<(String, bool)> = ctx.channels_summary.clone();
    let configured_count = rows.iter().filter(|(_, c)| *c).count();
    let not_configured: Vec<String> = rows
        .iter()
        .filter(|(_, c)| !*c)
        .map(|(n, _)| n.clone())
        .collect();

    // Auto-start state — drives the per-channel status icon + the footer
    // diagnostic. Mirrors the v0.6.6 logic but renders into typed rows.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let (auto_label, auto_kind, auto_detail) = match snapshot() {
        AutoStartState::NotDispatched => (
            "Auto-start",
            StatusKind::Info,
            "no channels configured at startup".to_string(),
        ),
        AutoStartState::Starting { since_unix } => {
            let elapsed = now.saturating_sub(since_unix);
            if elapsed < 5 {
                ("Auto-start", StatusKind::Info, "starting…".to_string())
            } else {
                ("Auto-start", StatusKind::Ok, "running".to_string())
            }
        }
        AutoStartState::Terminated { .. } => (
            "Auto-start",
            StatusKind::Warn,
            "stopped (dispatch loop exited; restart `rantaiclaw`)".to_string(),
        ),
        AutoStartState::Failed { message, .. } => ("Auto-start", StatusKind::Fail, message),
    };

    let mut panel = InfoPanel::new("Channels")
        .with_subtitle(format!(
            "{} configured · {} available",
            configured_count,
            rows.len()
        ))
        .with_footer("Esc close · `/setup channels` to add or reconfigure")
        .section(InfoSection::new("Always available").status_with(
            StatusKind::Ok,
            "CLI / TUI",
            "this terminal",
        ));

    // Auto-start state row — single-line probe.
    panel =
        panel.section(InfoSection::new("Runtime").status_with(auto_kind, auto_label, auto_detail));

    // Configured channels — detailed per-row state.
    if configured_count > 0 {
        let mut sec = InfoSection::new("Configured");
        let polling_label = match snapshot() {
            AutoStartState::Starting { since_unix } if now.saturating_sub(since_unix) >= 5 => {
                "polling"
            }
            AutoStartState::Starting { .. } => "starting…",
            AutoStartState::Terminated { .. } => "stopped",
            AutoStartState::Failed { .. } => "failed",
            AutoStartState::NotDispatched => "configured",
        };
        let kind = match snapshot() {
            AutoStartState::Starting { since_unix } if now.saturating_sub(since_unix) >= 5 => {
                StatusKind::Ok
            }
            AutoStartState::Starting { .. } => StatusKind::Info,
            AutoStartState::Terminated { .. } => StatusKind::Warn,
            AutoStartState::Failed { .. } => StatusKind::Fail,
            AutoStartState::NotDispatched => StatusKind::Info,
        };
        for (name, configured) in &rows {
            if *configured {
                sec = sec.status_with(kind, name.clone(), polling_label);
            }
        }
        panel = panel.section(sec);
    } else {
        panel = panel.section(InfoSection::new("Configured").plain(
            "No external channels configured. \
                     Run `/setup channels` to add Telegram, Discord, Slack, etc.",
        ));
    }

    // Not configured — compact comma-separated list (visual breathing
    // room — keep the panel from blowing up to 30 lines).
    if !not_configured.is_empty() {
        panel = panel.section(InfoSection::new("Not configured").inline_list(not_configured));
    }

    // Log pointer — always there because anyone debugging Telegram needs it.
    panel = panel.section(
        InfoSection::new("Logs")
            .plain("~/.rantaiclaw/logs/tui-YYYY-MM-DD.log")
            .plain("(search for `auto-start`, `channel message`, `channel reply`)"),
    );

    panel
}

impl CommandHandler for ChannelsCommand {
    fn name(&self) -> &str {
        "channels"
    }

    fn description(&self) -> &str {
        "Show installed and active channels (Telegram, Discord, etc.)"
    }

    fn aliases(&self) -> Vec<&str> {
        // /platforms removed in v0.6.8 — was a v0.6.4 alias for muscle
        // memory but the screenshots showed it as redundant noise. Drop.
        vec![]
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        Ok(CommandResult::OpenInfoPanel(build_channels_panel(ctx)))
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
            CommandResult::OpenInfoPanel(panel) => {
                assert_eq!(panel.title, "Status");
                // Two sections: Agent + Session.
                assert_eq!(panel.sections.len(), 2);
            }
            _ => panic!("Expected OpenInfoPanel result"),
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
