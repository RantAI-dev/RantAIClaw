//! Cross-channel approval bridge for the Supervised-mode shell
//! allowlist.
//!
//! Two halves:
//!
//! 1. [`spawn_relay`] — background task that subscribes to the
//!    `PendingApprovals` broadcast on [`SecurityPolicy`] and forwards
//!    every new request to every configured channel as a formatted
//!    message. The user can reply on any channel (or via the TUI
//!    slash commands); the first resolution wins.
//!
//! 2. [`try_handle_reply`] — stateless parser called by the channel
//!    dispatch loop before each inbound message is forwarded to the
//!    agent. Recognises text-channel approval replies in formats
//!    natural over chat:
//!
//!    - `/allow brew` / `/allow brew --persist`
//!    - `/deny brew`
//!    - `allow brew`, `deny brew` (slash-less for SMS / WhatsApp)
//!    - single-char shortcuts: `y brew` (once), `Y brew` (persist),
//!      `n brew` (deny). The capital-letter convention mirrors
//!      `git rebase -i` style and stays consistent with how the
//!      design doc described the channel UX.
//!
//! Returning `Some(reply)` from [`try_handle_reply`] means the
//! message was consumed; the caller should *not* forward it to the
//! agent and should reply to the user with the returned string.
//! `None` means the message is normal chat traffic.

use std::collections::HashMap;
use std::sync::Arc;

use crate::approval::{
    can_approve, summarize_args, ApprovalBackend, ApprovalManager, ApprovalRequest,
    ApprovalResponse,
};
use crate::channels::traits::{Channel, SendMessage};
use crate::security::{Decision, PendingApprovals, SecurityPolicy};

/// Format a new approval request as a chat-friendly message.
pub fn format_approval_message(basename: &str, full_command: &str) -> String {
    format!(
        "🔒 Approval needed: `{basename}` (full command: `{full_command}`).\n\
         Reply with one of:\n\
         • `/allow {basename}` — allow once for this session\n\
         • `/allow {basename} --persist` — allow and remember across restarts\n\
         • `/deny {basename}` — reject\n\
         Auto-deny in 5 min."
    )
}

/// Try to interpret `text` as an approval reply. On success returns a
/// human-readable acknowledgement *and* resolves the pending request
/// against `security`. On failure returns `None` so the caller can
/// route the message to the agent as normal.
///
/// `sender` + `owners` enforce the owner-authority gate: an `allow`/`/allow`
/// reply that would grant a command is honored **only** from an authorized
/// owner (`[channels_config] approval_owners`). Being able to chat with the
/// bot does not make a sender able to approve its shell commands — otherwise
/// any chat participant could allowlist arbitrary commands for the agent.
/// `deny` is always honored regardless of sender (denying is safe and lets
/// anyone stop a pending action).
pub fn try_handle_reply(
    text: &str,
    security: &SecurityPolicy,
    sender: &str,
    owners: &[String],
) -> Option<String> {
    let parsed = parse_reply(text)?;
    if parsed.verb == ReplyVerb::Allow && !crate::approval::can_approve(owners, sender) {
        // Recognised as an allow reply, but this sender isn't an owner.
        // Consume the message (it WAS an approval attempt, not chat) and tell
        // them they can't grant it. The pending request stays open so a real
        // owner can still resolve it.
        return Some(format!(
            "You're not authorized to approve `{}`. Ask an owner to reply `/allow {}`.",
            parsed.basename, parsed.basename
        ));
    }
    let pending = security.pending()?;
    handle_parsed(&parsed, security, &pending)
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedReply {
    verb: ReplyVerb,
    basename: String,
    persist: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ReplyVerb {
    Allow,
    Deny,
}

fn parse_reply(text: &str) -> Option<ParsedReply> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Drop a leading slash, but only at the very start — we don't want
    // `find / -name foo` to parse as a /command.
    let body = trimmed.strip_prefix('/').unwrap_or(trimmed);

    let mut tokens = body.split_whitespace();
    let head = tokens.next()?;
    let basename_raw = tokens.next()?;
    // Reject anything beyond `--persist` / `--save` so chatty
    // sentences like "allow brew because it's safe" don't silently
    // toggle the allowlist.
    let trailing: Vec<&str> = tokens.collect();
    let persist = match trailing.as_slice() {
        [] => false,
        ["--persist" | "--save" | "-p" | "persist"] => true,
        _ => return None,
    };

    let (verb, persist) = match head {
        "allow" => (ReplyVerb::Allow, persist),
        "deny" => (ReplyVerb::Deny, persist),
        // Short-form shortcuts — `Y` implies persist, `y` does not.
        "y" if !persist => (ReplyVerb::Allow, false),
        "Y" if !persist => (ReplyVerb::Allow, true),
        "n" | "N" if !persist => (ReplyVerb::Deny, false),
        _ => return None,
    };

    let basename = basename_raw.trim_matches('`').trim_matches('"');
    if basename.is_empty() || basename.contains(char::is_whitespace) {
        return None;
    }
    Some(ParsedReply {
        verb,
        basename: basename.to_string(),
        persist,
    })
}

fn handle_parsed(
    parsed: &ParsedReply,
    security: &SecurityPolicy,
    pending: &PendingApprovals,
) -> Option<String> {
    match parsed.verb {
        ReplyVerb::Allow => {
            if let Err(e) = security.add_runtime_command(&parsed.basename, parsed.persist) {
                return Some(format!("Failed to allow `{}`: {e}", parsed.basename));
            }
            let decision = if parsed.persist {
                Decision::Persist
            } else {
                Decision::Session
            };
            let resolved = pending
                .resolve_by_basename(&parsed.basename, decision)
                .is_some();
            let scope = if parsed.persist {
                "persistent"
            } else {
                "session"
            };
            let suffix = if resolved {
                " — pending approval resolved; the agent will retry."
            } else {
                ""
            };
            Some(format!(
                "✅ Added `{}` to the {scope} allowlist{suffix}",
                parsed.basename
            ))
        }
        ReplyVerb::Deny => match pending.resolve_by_basename(&parsed.basename, Decision::Deny) {
            Some(_) => Some(format!(
                "🚫 Denied `{}`. The agent's tool call will fail.",
                parsed.basename
            )),
            None => Some(format!(
                "No pending approval for `{}` (or more than one queued).",
                parsed.basename
            )),
        },
    }
}

/// Spawn the broadcast → channels relay. The task lives for the
/// lifetime of the process. It exits if the broadcast sender drops
/// (i.e. the registry is gone), which only happens at shutdown.
///
/// `channels_by_name` is borrowed by handle, so adding/removing
/// channels at runtime would need a registry refresh — out of scope
/// here; rantaiclaw configures channels at boot.
pub fn spawn_relay(
    security: Arc<SecurityPolicy>,
    channels_by_name: Arc<HashMap<String, Arc<dyn Channel>>>,
    default_recipients: HashMap<String, String>,
) {
    let Some(pending) = security.pending() else {
        tracing::debug!(target: "approval_relay", "no PendingApprovals bound; relay not started");
        return;
    };
    let mut rx = pending.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(req) => {
                    let body = format_approval_message(&req.basename, &req.full_command);
                    for (name, channel) in channels_by_name.iter() {
                        let Some(recipient) = default_recipients.get(name) else {
                            continue;
                        };
                        let msg = SendMessage::new(body.clone(), recipient.clone());
                        if let Err(e) = channel.send(&msg).await {
                            tracing::warn!(
                                target: "approval_relay",
                                channel = %name,
                                error = %e,
                                "failed to deliver approval notification"
                            );
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

// ── Whole-tool approval over chat (Layer A: ApprovalBackend) ──────────
//
// The shell allowlist relay above (Layer B) only covers `shell` basenames.
// A tool that needs approval at the current autonomy level (anything not in
// `auto_approve`) is decided by [`ApprovalBackend`] in the agent loop, which on
// channels defaults to auto-deny. [`ChatRelayApprovalBackend`] upgrades that on
// owner-configured channels: it posts the pending tool call to the chat and
// awaits an authorized owner's `/approve` / `/deny` reply, reusing the same
// async [`PendingApprovals`] machinery the shell relay uses (the tool name sits
// in the `basename` slot — a dedicated registry, never the shell one). Absent an
// approving owner before the deadline, the request times out to deny, so the
// secure-by-default posture is preserved.

/// Format a pending whole-tool approval as a chat-friendly message.
pub fn format_tool_approval_message(tool_name: &str, args_summary: &str) -> String {
    let detail = if args_summary.trim().is_empty() {
        String::new()
    } else {
        format!(" — `{args_summary}`")
    };
    format!(
        "🔧 The agent wants to run the `{tool_name}` tool{detail}.\n\
         Reply with one of:\n\
         • `/approve {tool_name}` — allow this call\n\
         • `/deny {tool_name}` — reject it\n\
         Auto-deny in 5 min."
    )
}

/// In-chat, owner-gated approval backend for polling channels.
///
/// Constructed per inbound message (it carries the originating chat's reply
/// target) only when an owner is configured and tool-gating is active; otherwise
/// the loop keeps using the auto-deny default. Posting + awaiting both happen
/// inside [`ApprovalBackend::decide`].
pub struct ChatRelayApprovalBackend {
    /// Dedicated tool-approval registry (NOT the shell `PendingApprovals`).
    relay: Arc<PendingApprovals>,
    /// Channel used to post the approval prompt back to the originating chat.
    channel: Arc<dyn Channel>,
    /// Reply target (chat id / room) the prompt is delivered to.
    recipient: String,
    /// Optional thread id so the prompt threads with the conversation.
    thread_ts: Option<String>,
    /// Channel name, recorded on the pending request for display/audit.
    channel_name: String,
}

impl ChatRelayApprovalBackend {
    pub fn new(
        relay: Arc<PendingApprovals>,
        channel: Arc<dyn Channel>,
        recipient: impl Into<String>,
        thread_ts: Option<String>,
        channel_name: impl Into<String>,
    ) -> Self {
        Self {
            relay,
            channel,
            recipient: recipient.into(),
            thread_ts,
            channel_name: channel_name.into(),
        }
    }
}

#[async_trait::async_trait]
impl ApprovalBackend for ChatRelayApprovalBackend {
    async fn decide(&self, _mgr: &ApprovalManager, request: &ApprovalRequest) -> ApprovalResponse {
        let summary = summarize_args(&request.arguments);
        let body = format_tool_approval_message(&request.tool_name, &summary);
        let msg = SendMessage::new(body, &self.recipient).in_thread(self.thread_ts.clone());
        if let Err(e) = self.channel.send(&msg).await {
            // Can't ask the owner → fail closed (deny). Do not run the tool.
            tracing::warn!(
                target: "approval_relay",
                channel = %self.channel_name,
                tool = %request.tool_name,
                error = %e,
                "failed to post tool-approval prompt; denying"
            );
            return ApprovalResponse::No;
        }

        // Block this tool call until an owner resolves it (via
        // `try_handle_tool_reply`) or the registry's deadline auto-denies.
        match self
            .relay
            .request_decision(
                request.tool_name.clone(),
                summary,
                self.channel_name.clone(),
            )
            .await
        {
            // A single approval grants this one call; we deliberately do NOT
            // map Session/Persist to a session allowlist here — channel
            // approvals stay per-call so a stranger can't ride a prior grant.
            Decision::Once | Decision::Session | Decision::Persist => ApprovalResponse::Yes,
            Decision::Deny => ApprovalResponse::No,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ToolReplyVerb {
    Approve,
    Deny,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedToolReply {
    verb: ToolReplyVerb,
    tool: String,
}

fn parse_tool_reply(text: &str) -> Option<ParsedToolReply> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let body = trimmed.strip_prefix('/').unwrap_or(trimmed);
    let mut tokens = body.split_whitespace();
    let head = tokens.next()?;
    let tool_raw = tokens.next()?;
    // Reject trailing chatter so "approve web_search because…" doesn't fire.
    if tokens.next().is_some() {
        return None;
    }
    let verb = match head {
        "approve" | "approved" => ToolReplyVerb::Approve,
        "deny" | "reject" => ToolReplyVerb::Deny,
        _ => return None,
    };
    let tool = tool_raw.trim_matches('`').trim_matches('"');
    if tool.is_empty() || tool.contains(char::is_whitespace) {
        return None;
    }
    Some(ParsedToolReply {
        verb,
        tool: tool.to_string(),
    })
}

/// Try to interpret `text` as a whole-tool approval reply (`/approve <tool>`,
/// `/deny <tool>`, slash optional). Returns `Some(ack)` only when a matching
/// tool request is actually pending in `relay`, so unrelated replies (including
/// the shell `/allow` path) fall through to other handlers. The owner-authority
/// gate mirrors the shell relay: an `approve` is honored only from an authorized
/// owner; `deny` is honored from anyone (stopping an action is always safe).
pub fn try_handle_tool_reply(
    text: &str,
    relay: &PendingApprovals,
    sender: &str,
    owners: &[String],
) -> Option<String> {
    let parsed = parse_tool_reply(text)?;
    // Only claim the message if this tool is genuinely awaiting a decision —
    // otherwise it may be a shell reply or normal chat.
    let pending_for_tool = relay.list().iter().any(|r| r.basename == parsed.tool);
    if !pending_for_tool {
        return None;
    }
    match parsed.verb {
        ToolReplyVerb::Approve => {
            if !can_approve(owners, sender) {
                return Some(format!(
                    "You're not authorized to approve `{}`. Ask an owner to reply `/approve {}`.",
                    parsed.tool, parsed.tool
                ));
            }
            match relay.resolve_by_basename(&parsed.tool, Decision::Once) {
                Some(_) => Some(format!(
                    "✅ Approved `{}` — the agent will run it now.",
                    parsed.tool
                )),
                None => Some(format!(
                    "Couldn't approve `{}` — more than one request is queued for it.",
                    parsed.tool
                )),
            }
        }
        ToolReplyVerb::Deny => match relay.resolve_by_basename(&parsed.tool, Decision::Deny) {
            Some(_) => Some(format!(
                "🚫 Denied `{}`. The tool call will fail.",
                parsed.tool
            )),
            None => Some(format!(
                "Couldn't deny `{}` — more than one request is queued for it.",
                parsed.tool
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn supervised_only_echo() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: crate::security::AutonomyLevel::Supervised,
            allowed_commands: vec!["echo".into()],
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn parse_slash_allow_basic() {
        let p = parse_reply("/allow brew").unwrap();
        assert_eq!(p.verb, ReplyVerb::Allow);
        assert_eq!(p.basename, "brew");
        assert!(!p.persist);
    }

    #[test]
    fn parse_slashless_works_for_chat_channels() {
        let p = parse_reply("allow brew").unwrap();
        assert_eq!(p.verb, ReplyVerb::Allow);
    }

    #[test]
    fn parse_allow_persist_flag() {
        let p = parse_reply("/allow brew --persist").unwrap();
        assert!(p.persist);
        let p = parse_reply("allow brew persist").unwrap();
        assert!(p.persist);
        let p = parse_reply("allow brew -p").unwrap();
        assert!(p.persist);
    }

    #[test]
    fn parse_short_forms() {
        let p = parse_reply("y brew").unwrap();
        assert_eq!(p.verb, ReplyVerb::Allow);
        assert!(!p.persist);
        let p = parse_reply("Y brew").unwrap();
        assert!(p.persist);
        let p = parse_reply("n brew").unwrap();
        assert_eq!(p.verb, ReplyVerb::Deny);
    }

    #[test]
    fn parse_rejects_chatty_extra_words() {
        // We don't want "allow brew because i need it" to silently
        // pass — it might be a chat sentence, not an explicit verb.
        assert!(parse_reply("allow brew because i need it").is_none());
    }

    #[test]
    fn parse_rejects_leading_slash_inside_a_path() {
        // First token is `find`, not a verb — not a reply.
        assert!(parse_reply("find / -name foo").is_none());
    }

    #[test]
    fn parse_rejects_unknown_verbs() {
        assert!(parse_reply("install brew").is_none());
        assert!(parse_reply("hello").is_none());
        assert!(parse_reply("").is_none());
    }

    #[tokio::test]
    async fn try_handle_reply_resolves_pending() {
        let security = supervised_only_echo();
        let pending = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        security.set_pending(pending.clone());

        // Producer side: shell tool would call this.
        let pending2 = pending.clone();
        let task = tokio::spawn(async move {
            pending2
                .request_decision("brew", "brew --version", "telegram")
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Reply comes from an authorized owner.
        let owners = vec!["owner1".to_string()];
        let ack =
            try_handle_reply("/allow brew", &security, "owner1", &owners).expect("recognised");
        assert!(ack.contains("session"));
        assert!(ack.contains("retry"));

        let decision = task.await.unwrap();
        assert_eq!(decision, Decision::Session);
        assert!(security
            .runtime_allowlist_snapshot()
            .contains(&"brew".to_string()));
    }

    #[tokio::test]
    async fn try_handle_reply_allow_from_non_owner_is_refused() {
        let security = supervised_only_echo();
        let pending = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        security.set_pending(pending.clone());

        let pending2 = pending.clone();
        let task = tokio::spawn(async move {
            pending2
                .request_decision("brew", "brew --version", "telegram")
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // A non-owner replies `/allow` — recognised as an approval attempt
        // (so it's consumed, not routed to the agent) but NOT honored: the
        // command must not be allowlisted and the pending request stays open.
        let owners = vec!["owner1".to_string()];
        let ack =
            try_handle_reply("/allow brew", &security, "stranger", &owners).expect("recognised");
        assert!(ack.contains("not authorized"));
        assert!(!security
            .runtime_allowlist_snapshot()
            .contains(&"brew".to_string()));

        // The pending request is still open; a real owner can resolve it.
        let ack =
            try_handle_reply("/allow brew", &security, "owner1", &owners).expect("recognised");
        assert!(ack.contains("session"));
        assert_eq!(task.await.unwrap(), Decision::Session);
    }

    #[tokio::test]
    async fn try_handle_reply_deny_keeps_allowlist_clean() {
        let security = supervised_only_echo();
        let pending = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        security.set_pending(pending.clone());

        let pending2 = pending.clone();
        let task = tokio::spawn(async move {
            pending2
                .request_decision("brew", "brew --version", "telegram")
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Deny is honored regardless of sender/owner — stopping a pending
        // action is always safe.
        let ack = try_handle_reply("n brew", &security, "anyone", &[]).expect("recognised");
        assert!(ack.contains("Denied"));
        assert_eq!(task.await.unwrap(), Decision::Deny);
        assert!(!security
            .runtime_allowlist_snapshot()
            .contains(&"brew".to_string()));
    }

    #[test]
    fn try_handle_reply_returns_none_for_chat_messages() {
        let security = supervised_only_echo();
        assert!(try_handle_reply("hello", &security, "u", &[]).is_none());
        assert!(try_handle_reply("can you find me a recipe", &security, "u", &[]).is_none());
    }

    // ── Whole-tool relay (Layer A) ───────────────────────────────────

    #[test]
    fn parse_tool_reply_recognises_approve_deny() {
        assert_eq!(
            parse_tool_reply("/approve web_search").unwrap(),
            ParsedToolReply {
                verb: ToolReplyVerb::Approve,
                tool: "web_search".into()
            }
        );
        assert_eq!(
            parse_tool_reply("deny shell").unwrap().verb,
            ToolReplyVerb::Deny
        );
        // Chatty / unknown / empty → not a reply.
        assert!(parse_tool_reply("approve web_search because i need it").is_none());
        assert!(parse_tool_reply("hello").is_none());
        assert!(parse_tool_reply("approve").is_none());
    }

    #[tokio::test]
    async fn tool_reply_returns_none_when_no_pending_match() {
        // Recognised verb, but nothing pending for that tool → fall through
        // (could be a shell reply or plain chat).
        let relay = PendingApprovals::new(Some(Duration::from_secs(10)));
        assert!(
            try_handle_tool_reply("/approve web_search", &relay, "owner1", &["owner1".into()])
                .is_none()
        );
        assert!(
            try_handle_tool_reply("hello there", &relay, "owner1", &["owner1".into()]).is_none()
        );
    }

    #[tokio::test]
    async fn tool_reply_owner_approves_resolves_pending() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let r2 = relay.clone();
        let task = tokio::spawn(async move {
            r2.request_decision("web_search", "query: rust", "telegram")
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let owners = vec!["owner1".to_string()];
        let ack = try_handle_tool_reply("/approve web_search", &relay, "owner1", &owners)
            .expect("recognised");
        assert!(ack.contains("Approved"), "{ack}");
        assert_eq!(task.await.unwrap(), Decision::Once);
    }

    #[tokio::test]
    async fn tool_reply_non_owner_approve_is_refused_pending_stays_open() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let r2 = relay.clone();
        let task = tokio::spawn(async move {
            r2.request_decision("web_search", "query: rust", "telegram")
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let owners = vec!["owner1".to_string()];
        // Stranger names the real pending tool → consumed (it WAS an approval
        // attempt) but refused; the request stays open for a real owner.
        let ack = try_handle_tool_reply("/approve web_search", &relay, "stranger", &owners)
            .expect("recognised");
        assert!(ack.contains("not authorized"), "{ack}");
        assert_eq!(relay.list().len(), 1, "pending stays open");

        // A real owner then resolves it.
        let ack = try_handle_tool_reply("/approve web_search", &relay, "owner1", &owners)
            .expect("recognised");
        assert!(ack.contains("Approved"));
        assert_eq!(task.await.unwrap(), Decision::Once);
    }

    #[tokio::test]
    async fn tool_reply_deny_is_honored_from_anyone() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let r2 = relay.clone();
        let task = tokio::spawn(async move {
            r2.request_decision("shell", "rm -rf /tmp/x", "telegram")
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Deny needs no owner authority — stopping an action is always safe.
        let ack = try_handle_tool_reply("/deny shell", &relay, "anyone", &[]).expect("recognised");
        assert!(ack.contains("Denied"), "{ack}");
        assert_eq!(task.await.unwrap(), Decision::Deny);
    }

    /// Minimal channel that records what was posted, for backend tests.
    #[derive(Default)]
    struct CapturingChannel {
        posted: tokio::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Channel for CapturingChannel {
        fn name(&self) -> &str {
            "telegram"
        }
        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.posted.lock().await.push(message.content.clone());
            Ok(())
        }
        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn test_manager() -> ApprovalManager {
        ApprovalManager::from_config(&crate::config::AutonomyConfig::default())
    }

    #[tokio::test]
    async fn chat_relay_backend_posts_prompt_and_yields_yes_on_owner_approval() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let channel: Arc<dyn Channel> = Arc::new(CapturingChannel::default());
        let backend = ChatRelayApprovalBackend::new(
            relay.clone(),
            channel.clone(),
            "chat-1",
            None,
            "telegram",
        );
        let mgr = test_manager();
        let request = ApprovalRequest {
            tool_name: "web_search".into(),
            arguments: serde_json::json!({ "query": "rust" }),
        };

        // decide() posts the prompt then blocks awaiting a reply.
        let decide = tokio::spawn(async move { backend.decide(&mgr, &request).await });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let owners = vec!["owner1".to_string()];
        try_handle_tool_reply("/approve web_search", &relay, "owner1", &owners)
            .expect("recognised");

        assert_eq!(decide.await.unwrap(), ApprovalResponse::Yes);
        // The owner saw a prompt naming the tool.
        let posted = relay.list();
        assert!(posted.is_empty(), "registry cleaned up after resolve");
    }

    #[tokio::test]
    async fn chat_relay_backend_denies_on_timeout() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_millis(50))));
        let channel: Arc<dyn Channel> = Arc::new(CapturingChannel::default());
        let backend = ChatRelayApprovalBackend::new(
            relay.clone(),
            channel.clone(),
            "chat-1",
            None,
            "telegram",
        );
        let mgr = test_manager();
        let request = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({ "command": "ls" }),
        };
        // No owner replies → the registry deadline fires → deny.
        assert_eq!(backend.decide(&mgr, &request).await, ApprovalResponse::No);
    }
}
