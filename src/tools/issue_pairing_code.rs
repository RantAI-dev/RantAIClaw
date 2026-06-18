//! `issue_pairing_code` — owner-only chat tool that mints an on-demand
//! pairing code for a channel (or the gateway), so an owner can invite a new
//! user or device without dropping to the CLI or restarting the daemon.
//!
//! > "give me an invite code for telegram"
//! > "issue a single-use whatsapp code that expires in 5 minutes"
//!
//! The owner forwards the printed code to the recipient, who DMs the bot:
//!   * `/claim <code>` — become an **owner** (only if the code grants it), or
//!   * `/bind <code>`  — become an allowed **chat** user.
//!
//! ## Why this is safe to expose
//!
//! Minting a code is an authority-granting action (a `/claim` code can promote
//! the recipient to owner), so it is gated exactly like
//! [`crate::tools::manage_permissions`]:
//!
//!   1. **Guests can never call it.** The per-turn [`crate::approval::GuestGate`]
//!      lists `issue_pairing_code` in `OWNER_ONLY_TOOLS` and denies it outright,
//!      regardless of `guest_allowed_tools`.
//!   2. **Only owner turns and the local operator reach it.** Those callers can
//!      already run `rantaiclaw channels pair` directly, so this grants no new
//!      authority — just a chat-side convenience.
//!
//! The minted code lands in the shared on-disk store
//! ([`crate::security::pairing_store`]); a running daemon validates it on the
//! next `/bind`/`/claim` message with no restart.

use super::traits::{Tool, ToolResult};
use crate::security::pairing_store;
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

/// Tool name. Kept in one place because both the registry and the
/// [`crate::approval::GuestGate`] owner-only denylist reference it.
pub const TOOL_NAME: &str = "issue_pairing_code";

/// Default validity window, in minutes, when the caller omits `ttl_minutes`.
const DEFAULT_TTL_MINUTES: i64 = 15;

pub struct IssuePairingCodeTool;

impl IssuePairingCodeTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for IssuePairingCodeTool {
    fn default() -> Self {
        Self::new()
    }
}

fn err(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(msg.into()),
    }
}

/// Current unix time in seconds (the clock the store windows codes against).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl Tool for IssuePairingCodeTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Owner-only. Mint an on-demand pairing code so a new user or device can \
         self-onboard onto a channel (or the gateway) without a daemon restart. \
         Forward the returned code to the recipient: they DM the bot `/claim \
         <code>` to become an owner (if the code grants it) or `/bind <code>` to \
         become an allowed chat user. `channel` is the surface (e.g. telegram, \
         whatsapp, discord, slack, or gateway). `ttl_minutes` sets the validity \
         window (default 15). `max_uses` bounds claims (omit for unlimited within \
         the window). `owner` (default true) controls whether `/claim` may \
         promote the recipient to owner; set false for a chat-only invite."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "The surface this code is scoped to (e.g. telegram, whatsapp, discord, slack, gateway). A code minted for one channel cannot be claimed on another."
                },
                "ttl_minutes": {
                    "type": "integer",
                    "description": "Validity window in minutes (default 15)."
                },
                "max_uses": {
                    "type": "integer",
                    "description": "Maximum number of successful claims. Omit for unlimited within the window."
                },
                "owner": {
                    "type": "boolean",
                    "description": "Whether `/claim` may promote the recipient to owner (default true). Set false for a chat-only invite."
                }
            },
            "required": ["channel"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let channel = args
            .get("channel")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if channel.is_empty() {
            return Ok(err(
                "missing `channel` (the surface to mint a code for, e.g. telegram, whatsapp, gateway)",
            ));
        }

        let ttl_minutes = args
            .get("ttl_minutes")
            .and_then(serde_json::Value::as_i64)
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_TTL_MINUTES);

        let max_uses = args
            .get("max_uses")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0);

        let grant_owner = args
            .get("owner")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        // Resolve the active profile root the running process uses; the store
        // lives under it. Same resolution as the `rantaiclaw channels pair` CLI.
        let root = match crate::profile::ProfileManager::active() {
            Ok(p) => p.root,
            Err(e) => return Ok(err(format!("could not resolve active profile: {e}"))),
        };

        let code = match pairing_store::mint(
            &root,
            &channel,
            ttl_minutes.saturating_mul(60),
            max_uses,
            grant_owner,
            now_unix(),
        ) {
            Ok(c) => c,
            Err(e) => return Ok(err(format!("could not mint pairing code: {e}"))),
        };

        let uses = match max_uses {
            Some(1) => "single-use".to_string(),
            Some(n) => format!("up to {n} uses"),
            None => "multi-use".to_string(),
        };

        let mut output =
            format!("🔐 Pairing code for {channel}: {code}   (valid {ttl_minutes} min, {uses})\n");
        if grant_owner {
            let _ = write!(
                output,
                "Forward it to the recipient. They DM the bot:\n  /claim {code}   → become an owner\n  /bind {code}    → become an allowed chat user\n"
            );
        } else {
            let _ = write!(
                output,
                "Forward it to the recipient. They DM the bot:\n  /bind {code}    → become an allowed chat user\n(This code does not grant owner — `/claim` will only bind them as a chat user.)\n"
            );
        }
        output.push_str("No daemon restart needed — a running channel picks this up on the next pairing message.");

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `issue_pairing_code` resolves the store root via `ProfileManager::active`,
    /// which reads `HOME` (→ `~/.rantaiclaw/profiles/<name>`). Point `HOME` at a
    /// temp dir so the test never touches a real profile. Serialized (async
    /// mutex, held across awaits) because it mutates a process-global env var.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn mint_returns_code_in_output() {
        let _g = ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        let tool = IssuePairingCodeTool::new();
        let res = tool
            .execute(json!({"channel": "telegram", "ttl_minutes": 5}))
            .await
            .unwrap();

        assert!(res.success, "{:?}", res.error);
        // A dash-grouped code appears in the output, plus the claim/bind hints.
        assert!(
            res.output.contains('-'),
            "output should carry a code: {}",
            res.output
        );
        assert!(res.output.contains("/claim"), "{}", res.output);
        assert!(res.output.contains("/bind"), "{}", res.output);
        assert!(res.output.contains("telegram"), "{}", res.output);

        std::env::remove_var("HOME");
    }

    #[tokio::test]
    async fn no_owner_flag_omits_claim_owner_line() {
        let _g = ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        let tool = IssuePairingCodeTool::new();
        let res = tool
            .execute(json!({"channel": "whatsapp", "owner": false}))
            .await
            .unwrap();

        assert!(res.success, "{:?}", res.error);
        assert!(res.output.contains("/bind"), "{}", res.output);
        assert!(
            res.output.contains("does not grant owner"),
            "chat-only invite should be flagged: {}",
            res.output
        );

        std::env::remove_var("HOME");
    }

    #[tokio::test]
    async fn missing_channel_is_rejected() {
        let _g = ENV_LOCK.lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        let tool = IssuePairingCodeTool::new();
        let res = tool.execute(json!({"ttl_minutes": 10})).await.unwrap();
        assert!(!res.success);
        assert!(res.error.unwrap_or_default().contains("missing `channel`"));

        std::env::remove_var("HOME");
    }
}
