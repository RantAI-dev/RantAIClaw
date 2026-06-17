//! `manage_permissions` — owner-only chat self-setup for the per-role channel
//! permission model.
//!
//! Lets an **owner** configure ownership and the non-owner ("guest") capability
//! ceiling by asking the agent in chat, instead of dropping to the CLI:
//!
//! > "add my colleague @bob as an owner"
//! > "let normal users run `kubectl get` and `kubectl describe`"
//! > "show me the current permissions"
//!
//! ## Why this is safe to expose
//!
//! Mutating who-owns-the-bot from chat is high-risk, so it is gated in depth:
//!
//!   1. **Guests can never call it.** The per-turn [`crate::approval::GuestGate`]
//!      treats `manage_permissions` as owner-only and denies it outright,
//!      *regardless* of `guest_allowed_tools` (see `GuestGate::OWNER_ONLY_TOOLS`).
//!      A non-owner chatting on any multi-user channel cannot reach this tool.
//!   2. **Only owner turns and the local operator reach it.** Owner turns run
//!      with no guest gate; CLI/TUI/console turns are the machine operator. Both
//!      are already fully privileged (they can edit `config.toml` directly), so
//!      letting them edit it through chat grants no new authority.
//!
//! The tool routes every change through [`crate::approval::permissions`] — the
//! same editor the `rantaiclaw permissions` CLI and the `/permissions` TUI
//! command use — then persists with `Config::save`.

use super::traits::{Tool, ToolResult};
use crate::approval::permissions::{self, Op, Target};
use crate::config::Config;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Tool name. Kept in one place because both the registry and the
/// [`crate::approval::GuestGate`] owner-only denylist reference it.
pub const TOOL_NAME: &str = "manage_permissions";

pub struct ManagePermissionsTool {
    /// Carries `config_path` so we persist to the same file the running process
    /// loaded. The actual edit reloads from disk to avoid clobbering concurrent
    /// changes.
    config: Arc<Config>,
}

impl ManagePermissionsTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

fn err(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(msg.into()),
    }
}

#[async_trait]
impl Tool for ManagePermissionsTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Owner-only. Manage who owns this bot and what NON-owner (\"guest\") users \
         may do on multi-user channels. Owners get the full toolset and may approve \
         tool calls; guests run under a capability ceiling. Use action=show to \
         review, add/remove with target=owner|tool|command to change. \
         target=owner takes a sender identity (e.g. a Telegram numeric user id or a \
         Slack/Discord username); target=tool takes a tool name (e.g. shell, \
         web_search); target=command takes a shell-command glob (e.g. 'kubectl get *')."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["show", "add", "remove"],
                    "description": "show = print current permissions; add/remove = change a list"
                },
                "target": {
                    "type": "string",
                    "enum": ["owner", "tool", "command"],
                    "description": "Which list to change (required for add/remove). owner = full-privilege sender; tool = a tool guests may use; command = a shell-command glob guests may run"
                },
                "value": {
                    "type": "string",
                    "description": "The entry to add/remove: a sender identity (owner), tool name (tool), or shell-command glob (command). Required for add/remove."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        // Load the live config from disk (authoritative; avoids editing a stale
        // in-memory snapshot). load_or_init resolves the same profile/path the
        // running process uses.
        let mut config = match Config::load_or_init().await {
            Ok(c) => c,
            Err(e) => return Ok(err(format!("could not load config: {e}"))),
        };

        if action == "show" {
            let rendered =
                permissions::render(&config.channels_config, &config.autonomy.auto_approve);
            return Ok(ToolResult {
                success: true,
                output: rendered,
                error: None,
            });
        }

        let op = match action.as_str() {
            "add" => Op::Add,
            "remove" => Op::Remove,
            other => {
                return Ok(err(format!(
                    "unknown action `{other}` (expected: show | add | remove)"
                )));
            }
        };

        let target_str = args
            .get("target")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        let Some(target) = Target::parse(target_str) else {
            return Ok(err(format!(
                "unknown or missing target `{target_str}` (expected: owner | tool | command)"
            )));
        };

        let value = args
            .get("value")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        if value.is_empty() {
            return Ok(err("missing `value` (the entry to add/remove)"));
        }

        // Lockout guard: refuse to remove the last owner FROM CHAT. With no
        // owners, every sender (including the requester) becomes a guest, and
        // `manage_permissions` is owner-only — so the chat surface could never
        // re-add an owner. The local CLI/TUI operator can still empty the list
        // deliberately (they have machine access); this guard is chat-only.
        if target == Target::Owner
            && op == Op::Remove
            && config.channels_config.approval_owners.len() == 1
            && config
                .channels_config
                .approval_owners
                .first()
                .map(String::as_str)
                == Some(value)
        {
            return Ok(err(
                "refusing to remove the last owner from chat — that would lock everyone out of \
                 chat-based permission management. Use the `rantaiclaw permissions` CLI if you \
                 really want zero owners.",
            ));
        }

        // Serialize this tool's load→apply→save so two concurrent owner calls
        // can't read-modify-write over each other (last-writer-wins data loss).
        let _save_guard = SAVE_LOCK.lock().await;

        let outcome = permissions::apply(&mut config.channels_config, target, op, value);
        if !outcome.changed {
            // Nothing to persist; report the no-op outcome as success.
            return Ok(ToolResult {
                success: true,
                output: outcome.message,
                error: None,
            });
        }

        let _ = &self.config; // path comes from the freshly-loaded config
        if let Err(e) = config.save().await {
            return Ok(err(format!("change computed but saving failed: {e}")));
        }

        let mut output = outcome.message;
        if target == Target::Owner && op == Op::Add && value == "*" {
            // Surface to the operator log too, not just the chat reply.
            tracing::warn!(
                target: "permissions",
                "approval_owners set to wildcard '*' via chat — ANY sender is now an owner"
            );
            output.push_str(
                "\n⚠️ `*` makes ANY sender an owner with the full toolset — this is insecure.",
            );
        }
        output.push_str("\n(Saved. A running channel/daemon may need a reload/restart to apply.)");

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

/// Serializes `manage_permissions` config writes process-wide so concurrent
/// owner calls don't clobber each other's load→apply→save.
static SAVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// `manage_permissions` resolves its config via `Config::load_or_init`, which
    /// reads `RANTAICLAW_CONFIG_DIR`. Point it at a temp dir so the test never
    /// touches a real profile. Serialized because it mutates a process-global env.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    async fn tool_in(tmp: &TempDir) -> ManagePermissionsTool {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        ManagePermissionsTool::new(Arc::new(config))
    }

    #[tokio::test]
    async fn add_show_remove_roundtrip() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::env::set_var("RANTAICLAW_CONFIG_DIR", tmp.path());
        let tool = tool_in(&tmp).await;

        let added = tool
            .execute(json!({"action": "add", "target": "owner", "value": "123456"}))
            .await
            .unwrap();
        assert!(added.success, "{:?}", added.error);
        assert!(added.output.contains("Added owner"));

        let shown = tool.execute(json!({"action": "show"})).await.unwrap();
        assert!(shown.success);
        assert!(shown.output.contains("123456"));

        // Add a second owner so removing the first isn't the last-owner lockout.
        tool.execute(json!({"action": "add", "target": "owner", "value": "789"}))
            .await
            .unwrap();
        let removed = tool
            .execute(json!({"action": "remove", "target": "owner", "value": "123456"}))
            .await
            .unwrap();
        assert!(removed.success);
        assert!(removed.output.contains("Removed owner"));

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }

    #[tokio::test]
    async fn missing_value_is_rejected() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::env::set_var("RANTAICLAW_CONFIG_DIR", tmp.path());
        let tool = tool_in(&tmp).await;

        let res = tool
            .execute(json!({"action": "add", "target": "tool"}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("missing `value`"));

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }

    #[tokio::test]
    async fn refuses_to_remove_last_owner() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::env::set_var("RANTAICLAW_CONFIG_DIR", tmp.path());
        let tool = tool_in(&tmp).await;

        // One owner present → removing it must be refused.
        tool.execute(json!({"action": "add", "target": "owner", "value": "solo"}))
            .await
            .unwrap();
        let res = tool
            .execute(json!({"action": "remove", "target": "owner", "value": "solo"}))
            .await
            .unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("last owner"));

        // With two owners, removing one is fine.
        tool.execute(json!({"action": "add", "target": "owner", "value": "second"}))
            .await
            .unwrap();
        let ok = tool
            .execute(json!({"action": "remove", "target": "owner", "value": "solo"}))
            .await
            .unwrap();
        assert!(ok.success, "{:?}", ok.error);

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }

    #[tokio::test]
    async fn unknown_action_is_rejected() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        std::env::set_var("RANTAICLAW_CONFIG_DIR", tmp.path());
        let tool = tool_in(&tmp).await;

        let res = tool.execute(json!({"action": "frobnicate"})).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("unknown action"));

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }
}
