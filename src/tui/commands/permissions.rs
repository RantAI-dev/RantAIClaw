//! Slash command for managing the per-role channel permission model from
//! inside the TUI — the same model the `rantaiclaw permissions` CLI edits and
//! the owner-gated chat tool mutates. All three route through
//! [`crate::approval::permissions`] so they behave identically.
//!
//! - `/permissions` — show owners + the non-owner (guest) capability ceiling.
//! - `/permissions add <owner|tool|command> <value>` — widen a list.
//! - `/permissions remove <owner|tool|command> <value>` — narrow a list.
//!
//! Writes go to `config.toml`; the TUI's `config_watcher` reloads the runtime
//! within ~500ms so a live channel session picks up the change.

use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::approval::permissions::{self, Op, Target};
use crate::tui::context::TuiContext;

pub struct PermissionsCommand;

impl CommandHandler for PermissionsCommand {
    fn name(&self) -> &str {
        "permissions"
    }

    fn aliases(&self) -> Vec<&str> {
        vec!["perms", "owners"]
    }

    fn description(&self) -> &str {
        "Manage channel owners + the non-owner (guest) tool/command ceiling"
    }

    fn usage(&self) -> &str {
        "/permissions [add|remove <owner|tool|command> <value>]"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let args = args.trim();

        // No arg → render current state.
        if args.is_empty() {
            return match load_config() {
                Ok(config) => Ok(CommandResult::Message(permissions::render(
                    &config.channels_config,
                    &config.autonomy.auto_approve,
                ))),
                Err(e) => Ok(CommandResult::Message(format!(
                    "✗ Could not load config: {e}"
                ))),
            };
        }

        // `<add|remove> <target> <value...>`
        let mut it = args.splitn(3, char::is_whitespace);
        let op_tok = it.next().unwrap_or("");
        let target_tok = it.next().unwrap_or("");
        let value = it.next().unwrap_or("").trim();

        let Some(op) = Op::parse(op_tok) else {
            return Ok(CommandResult::Message(format!(
                "✗ Unknown action `{op_tok}`. Usage: {}",
                self.usage()
            )));
        };
        let Some(target) = Target::parse(target_tok) else {
            return Ok(CommandResult::Message(format!(
                "✗ Unknown target `{target_tok}`. Expected: owner | tool | command."
            )));
        };
        if value.is_empty() {
            return Ok(CommandResult::Message(format!(
                "✗ Missing value. Usage: {}",
                self.usage()
            )));
        }

        match mutate_and_save(target, op, value) {
            Ok(outcome) => {
                let icon = if outcome.changed { "✅" } else { "ℹ️" };
                let mut msg = format!("{icon} {}", outcome.message);
                if target == Target::Owner && op == Op::Add && value == "*" {
                    msg.push_str(
                        "\n⚠️ `*` makes ANY sender an owner with the full toolset — insecure.",
                    );
                }
                Ok(CommandResult::Message(msg))
            }
            Err(e) => Ok(CommandResult::Message(format!(
                "✗ Failed to update permissions: {e}"
            ))),
        }
    }
}

/// Load the active config synchronously from within the TUI's async runtime
/// (mirrors `/autonomy`'s `block_in_place` bridge).
fn load_config() -> anyhow::Result<crate::config::Config> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| anyhow::anyhow!("/permissions must run inside a tokio runtime"))?;
    tokio::task::block_in_place(|| handle.block_on(crate::config::Config::load_or_init()))
}

/// Load → apply one mutation → save. Returns the outcome for messaging.
fn mutate_and_save(
    target: Target,
    op: Op,
    value: &str,
) -> anyhow::Result<permissions::ChangeOutcome> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| anyhow::anyhow!("/permissions must run inside a tokio runtime"))?;
    tokio::task::block_in_place(|| {
        handle.block_on(async move {
            let mut config = crate::config::Config::load_or_init().await?;
            let outcome = permissions::apply(&mut config.channels_config, target, op, value);
            if outcome.changed {
                config.save().await?;
            }
            Ok(outcome)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> TuiContext {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx
    }

    #[test]
    fn unknown_action_is_friendly() {
        let mut ctx = test_context();
        match PermissionsCommand
            .execute("frobnicate owner x", &mut ctx)
            .unwrap()
        {
            CommandResult::Message(m) => assert!(m.contains("Unknown action")),
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn unknown_target_is_friendly() {
        let mut ctx = test_context();
        match PermissionsCommand
            .execute("add wizard x", &mut ctx)
            .unwrap()
        {
            CommandResult::Message(m) => assert!(m.contains("Unknown target")),
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn add_without_value_is_friendly() {
        let mut ctx = test_context();
        match PermissionsCommand.execute("add owner", &mut ctx).unwrap() {
            CommandResult::Message(m) => assert!(m.contains("Missing value")),
            other => panic!("expected Message, got {other:?}"),
        }
    }
}
