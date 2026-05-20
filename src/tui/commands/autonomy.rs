//! Slash command for inspecting and switching the approval-policy preset
//! without leaving the TUI. The same write path is used by Shift+Tab
//! (which cycles) and by `rantaiclaw autonomy <preset>` on the CLI.
//!
//! - `/autonomy` — print the active preset plus the four options.
//! - `/autonomy <preset>` — switch to `manual`, `smart`, `strict`, `off`,
//!   or `full` (alias for `off`).
//!
//! The handler does not own a `Profile` handle, so it routes the write
//! through `ProfileManager::active()` — same source the rest of the
//! runtime uses. Failures surface as `System:` messages rather than
//! bubbling errors, so a typo doesn't tear down the chat session.

use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::approval::policy_writer::{self, PolicyPreset};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

pub struct AutonomyCommand;

impl CommandHandler for AutonomyCommand {
    fn name(&self) -> &str {
        "autonomy"
    }

    fn description(&self) -> &str {
        "Pick or switch the approval-policy preset (Manual / Smart / Strict / Off)"
    }

    fn usage(&self) -> &str {
        "/autonomy [manual|smart|strict|off]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let arg = args.trim();
        let profile = match crate::profile::ProfileManager::active() {
            Ok(p) => p,
            Err(e) => {
                return Ok(CommandResult::Message(format!(
                    "✗ Could not resolve active profile: {e}"
                )));
            }
        };

        // No arg → open the interactive picker. Up/Down to navigate,
        // Enter to select, Esc to dismiss. Selection routes through
        // app.rs::dispatch_list_picker_selection → ListPickerKind::Autonomy.
        if arg.is_empty() {
            let current = policy_writer::read_active_preset(&profile.policy_dir());
            ctx.autonomy_preset = current;
            let items: Vec<ListPickerItem> = PolicyPreset::ALL
                .iter()
                .map(|p| {
                    let is_current = current == Some(*p);
                    let primary = if is_current {
                        format!("{} (current)", p.label())
                    } else {
                        p.label().to_string()
                    };
                    ListPickerItem {
                        key: p.id().to_string(),
                        primary,
                        secondary: preset_blurb(*p).to_string(),
                    }
                })
                .collect();
            let preselect = current.map(|p| p.id().to_string());
            let picker = ListPicker::new(
                ListPickerKind::Autonomy,
                "Select Autonomy Preset",
                items,
                preselect.as_deref(),
                "No presets registered (this is a build-time error).",
            );
            return Ok(CommandResult::OpenListPicker(picker));
        }

        // Explicit arg → apply the preset directly (no picker round-trip).
        let target = match PolicyPreset::from_str_ci(arg) {
            Ok(p) => p,
            Err(e) => {
                return Ok(CommandResult::Message(format!(
                    "✗ {e}\n   Try one of: manual, smart, strict, off (or `full` as alias for off)."
                )));
            }
        };

        let warning = match policy_writer::write_policy_files(&profile, target, true) {
            Ok(w) => w,
            Err(e) => {
                return Ok(CommandResult::Message(format!(
                    "✗ Failed to switch autonomy mode: {e}"
                )));
            }
        };

        // Propagate to config.toml so the runtime `SecurityPolicy`
        // actually reflects the preset. The config_watcher will fire
        // within ~500ms and the TUI's reload_config will rebuild the
        // agent with the new level. Without this step the preset file
        // changes but `is_command_allowed` keeps using the old level.
        if let Err(e) = persist_preset_to_config(target) {
            return Ok(CommandResult::Message(format!(
                "⚠ Preset file written, but updating config.toml failed: {e}\n   \
                The live gate may not reflect the change until restart."
            )));
        }

        ctx.autonomy_preset = Some(target);
        let mut msg = format!(
            "⚙ Autonomy mode → {} (level={:?}). Shift+Tab to cycle.",
            target.label(),
            target.autonomy_level(),
        );
        if let Some(w) = warning {
            msg.push_str("\n\n");
            msg.push_str(w);
        }
        Ok(CommandResult::Message(msg))
    }
}

/// Load config.toml, apply `preset` to `[autonomy].level`, save back.
/// Drives the async `config.save()` via `block_in_place` so it can be
/// called from a sync `CommandHandler::execute`. The config_watcher
/// picks up the file change within ~500ms and the TUI's reload_config
/// rebuilds the agent with the new policy.
fn persist_preset_to_config(preset: PolicyPreset) -> anyhow::Result<()> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| anyhow::anyhow!("/autonomy must run inside a tokio runtime"))?;
    tokio::task::block_in_place(|| {
        handle.block_on(async move {
            let mut config = crate::config::Config::load_or_init().await?;
            policy_writer::apply_preset_to_config(&mut config, preset);
            config.save().await
        })
    })
}

/// Mirrors `crate::tui::app::preset_blurb` so the command module doesn't
/// have to reach into `app.rs` for one string. Kept tiny and deliberately
/// duplicated — four lines isn't worth a shared helper module.
pub(crate) fn preset_blurb(preset: PolicyPreset) -> &'static str {
    match preset {
        PolicyPreset::Manual => "every tool call prompts",
        PolicyPreset::Smart => "read-only auto, writes prompt",
        PolicyPreset::Strict => "deny by default, no prompts",
        PolicyPreset::Off => "no prompts — trusted env only",
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
    fn no_arg_opens_picker_with_all_four_presets() {
        let cmd = AutonomyCommand;
        let mut ctx = test_context();
        let result = cmd.execute("", &mut ctx).unwrap();
        match result {
            CommandResult::OpenListPicker(picker) => {
                assert_eq!(picker.kind, ListPickerKind::Autonomy);
                let keys: Vec<String> = picker
                    .entries()
                    .iter()
                    .filter_map(|e| e.as_item().map(|i| i.key.clone()))
                    .collect();
                assert_eq!(keys, vec!["manual", "smart", "strict", "off"]);
            }
            other => panic!("expected OpenListPicker, got {other:?}"),
        }
    }

    #[test]
    fn invalid_arg_returns_error_message() {
        let cmd = AutonomyCommand;
        let mut ctx = test_context();
        let result = cmd.execute("paranoid", &mut ctx).unwrap();
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("unknown policy preset"));
                assert!(msg.contains("manual, smart, strict, off"));
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }
}
