//! In-chat tool approval for gateway chat channels (WhatsApp, Linq,
//! Nextcloud Talk).
//!
//! Channels are non-interactive: there is no terminal to show a Y/N/A
//! prompt, so by default the gateway auto-denies any tool that needs
//! approval at the current autonomy level. This module adds a turn-based
//! approval flow so a person chatting the bot can approve a tool by
//! replying in the chat:
//!
//! ```text
//! bot:  🔧 To do that I need to run `vm.create`. Reply Y / A / N.
//! user: y            (allow once)  ·  a (always, this sender)  ·  n (skip)
//! ```
//!
//! It is **turn-based** (not a blocking prompt) so it fits the webhook
//! request/response model: the bot asks, ACKs the webhook, and the user's
//! next message carries the decision. On Y/A the original request is
//! re-run with the tool allow-listed — side-effect-safe because in
//! Supervised mode the first (fully-denied) turn executed no tools.
//!
//! Bypass entirely with `[channels_config] autonomous_tools = true`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How a chat reply to an approval prompt was interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalReply {
    /// Allow the pending tool(s) once, for this request only.
    Once,
    /// Allow the pending tool(s) and remember them for this sender.
    Always,
    /// Deny — do not run the pending tool(s).
    Deny,
}

/// Interpret a free-text chat message as a reply to a pending approval
/// prompt. Returns `None` for ordinary chat so the caller treats it as a
/// new request. Deliberately strict — only short, unambiguous tokens
/// count, so "yes please tell me more about X" is NOT an approval.
pub fn parse_approval_reply(text: &str) -> Option<ApprovalReply> {
    let t = text
        .trim()
        .trim_end_matches(['.', '!'])
        .trim()
        .to_ascii_lowercase();
    match t.as_str() {
        "y" | "yes" | "ok" | "okay" | "allow" | "approve" | "go" | "do it" => {
            Some(ApprovalReply::Once)
        }
        "a" | "always" | "yes always" | "allow always" => Some(ApprovalReply::Always),
        "n" | "no" | "deny" | "cancel" | "stop" | "skip" | "nope" => Some(ApprovalReply::Deny),
        _ => None,
    }
}

/// Format the Y/A/N prompt sent to the channel.
pub fn format_prompt(tools: &[String]) -> String {
    let list = tools
        .iter()
        .map(|t| format!("`{t}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "🔧 To do that I need to run {list}.\n\
         Reply *Y* to allow once, *A* to always allow, or *N* to skip."
    )
}

/// A tool call awaiting the user's Y/A/N decision.
#[derive(Debug, Clone)]
struct PendingApproval {
    /// The user's original message, replayed verbatim once approved.
    original_message: String,
    /// Tools the model wanted that were denied pending approval.
    tools: Vec<String>,
    created_at: Instant,
}

/// Per-`(channel, sender)` approval state for gateway chat channels.
pub struct ChannelApprovalStore {
    pending: Mutex<HashMap<String, PendingApproval>>,
    /// Tools a sender has chosen "Always" for, scoped to this process.
    allowlist: Mutex<HashMap<String, HashSet<String>>>,
    ttl: Duration,
}

impl Default for ChannelApprovalStore {
    fn default() -> Self {
        // A pending prompt expires after 10 minutes so a stale "y" much
        // later can't trigger an action the user has forgotten about.
        Self::new(Duration::from_mins(10))
    }
}

impl ChannelApprovalStore {
    pub fn new(ttl: Duration) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            allowlist: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Tools this sender has permanently allowed (for seeding auto-approve).
    pub fn allowlisted(&self, key: &str) -> Vec<String> {
        self.allowlist
            .lock()
            .get(key)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Record a pending approval, replacing any prior one for the key.
    pub fn set_pending(&self, key: &str, original_message: String, tools: Vec<String>) {
        self.pending.lock().insert(
            key.to_string(),
            PendingApproval {
                original_message,
                tools,
                created_at: Instant::now(),
            },
        );
    }

    /// Take (remove) a non-expired pending approval for the key, if any.
    pub fn take_pending(&self, key: &str) -> Option<(String, Vec<String>)> {
        let mut guard = self.pending.lock();
        let expired = guard.get(key).map(|p| p.created_at.elapsed() > self.ttl)?;
        let p = guard.remove(key)?;
        if expired {
            return None;
        }
        Some((p.original_message, p.tools))
    }

    /// Remember tools as always-allowed for this sender.
    pub fn remember_always(&self, key: &str, tools: &[String]) {
        let mut guard = self.allowlist.lock();
        guard
            .entry(key.to_string())
            .or_default()
            .extend(tools.iter().cloned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_affirmatives_as_once() {
        for s in [
            "y", "Y", "yes", "ok", "Okay", "allow", "approve", "go", "do it",
        ] {
            assert_eq!(parse_approval_reply(s), Some(ApprovalReply::Once), "{s}");
        }
    }

    #[test]
    fn parses_always_and_deny() {
        for s in ["a", "always", "Allow always"] {
            assert_eq!(parse_approval_reply(s), Some(ApprovalReply::Always), "{s}");
        }
        for s in ["n", "no", "deny", "cancel", "stop", "skip"] {
            assert_eq!(parse_approval_reply(s), Some(ApprovalReply::Deny), "{s}");
        }
    }

    #[test]
    fn ordinary_chat_is_not_an_approval() {
        for s in [
            "yes please tell me more",
            "what is nqrust microvm",
            "no idea, can you explain",
            "",
            "allocate a vm",
        ] {
            assert_eq!(parse_approval_reply(s), None, "{s}");
        }
    }

    #[test]
    fn pending_round_trips_and_expires() {
        let store = ChannelApprovalStore::new(Duration::from_mins(10));
        let key = "whatsapp:+15551234";
        store.set_pending(
            key,
            "create a vm named web".into(),
            vec!["vm.create".into()],
        );
        let (msg, tools) = store.take_pending(key).expect("pending present");
        assert_eq!(msg, "create a vm named web");
        assert_eq!(tools, vec!["vm.create".to_string()]);
        // taken — second take is empty
        assert!(store.take_pending(key).is_none());

        // expired pending is dropped
        let store = ChannelApprovalStore::new(Duration::from_millis(0));
        store.set_pending(key, "x".into(), vec!["t".into()]);
        std::thread::sleep(Duration::from_millis(5));
        assert!(store.take_pending(key).is_none());
    }

    #[test]
    fn always_allowlist_accumulates_per_sender() {
        let store = ChannelApprovalStore::default();
        let key = "linq:alice";
        store.remember_always(key, &["vm.create".into()]);
        store.remember_always(key, &["vm.start".into()]);
        let mut got = store.allowlisted(key);
        got.sort();
        assert_eq!(got, vec!["vm.create".to_string(), "vm.start".to_string()]);
        // other senders are unaffected
        assert!(store.allowlisted("linq:bob").is_empty());
    }
}
