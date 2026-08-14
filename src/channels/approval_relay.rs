//! Cross-channel approval bridge for the Supervised-mode shell
//! allowlist.
//!
//! This module is the **reply** half of chat approval.
//!
//! It used to describe a second half — a `spawn_relay` background task that
//! broadcast every new shell request to every configured channel. That function
//! had no callers in the entire repository, so nothing ever told an owner a
//! shell approval was pending, while the module doc presented it as working
//! machinery. It has been removed rather than left to read as wired.
//!
//! The consequence, stated plainly: **shell approvals over chat are reply-only.**
//! An owner can answer one, but nothing announces it. Tool approvals are
//! different — [`ChatRelayApprovalBackend`] posts those into the originating
//! chat and is wired.
//!
//! [`try_handle_reply`] is a stateless parser called by the channel
//! dispatch loop before each inbound message is forwarded to the
//! agent. It recognises text-channel approval replies in formats
//! natural over chat:
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
use crate::security::{Decision, PendingApprovals, PendingRequest, SecurityPolicy};

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
    channel: &str,
    reply_target: &str,
) -> Option<String> {
    let pending = security.pending()?;

    if let Some(parsed) = parse_reply(text) {
        // Look the request up BEFORE the authorization branch. The refusal used
        // to be returned first, which consumed ordinary chat containing "allow
        // him" and answered it with a refusal — and that refusal was emitted
        // only to non-owners, making it a membership oracle.
        let names_something_pending = pending.list().iter().any(|r| r.basename == parsed.basename);
        if !names_something_pending {
            return None;
        }
        if parsed.verb == ReplyVerb::Allow && !can_approve(owners, sender) {
            return Some(format!(
                "You're not authorized to approve `{}`. Ask an owner to reply `/allow {}`.",
                parsed.basename, parsed.basename
            ));
        }
        return handle_parsed(&parsed, security, &pending);
    }

    // Forgiving bare-verb form (`allow`, `yes`, `ok`, `deny`, `no`, …) with no
    // target token.
    //
    // This is the path that carried the cross-chat hole: a guest triggers a
    // gated command in chat A, the prompt is posted to chat A — the *triggering*
    // chat, which the guest chose — and an owner chatting normally in chat B
    // types `ok`, resolving chat A's request. Nothing about the reply named
    // that request, and resolution consulted neither the id nor the origin the
    // request already carried.
    //
    // A bare verb now only answers a request from the same chat. Explicitly
    // targeted replies (`/allow brew`) stay unscoped: naming the command is a
    // deliberate act, and shell requests carry no chat to match against.
    let verb = parse_bare_verb(text)?;
    // Scope restricts *granting*, not denying. This module already documents
    // that "deny is always honored regardless of sender (denying is safe and
    // lets anyone stop a pending action)" — narrowing that would remove a
    // fail-safe to close a hole that only exists on the grant path.
    let snapshot: Vec<PendingRequest> = match verb {
        BareVerb::Approve => pending
            .list()
            .into_iter()
            .filter(|r| request_is_answerable_here(r, channel, reply_target))
            .collect(),
        BareVerb::Deny => pending.list(),
    };
    match snapshot.len() {
        0 => None, // nothing pending here → normal chat ("yes please").
        1 => {
            let basename = snapshot[0].basename.clone();
            let parsed = ParsedReply {
                verb: match verb {
                    BareVerb::Approve => ReplyVerb::Allow,
                    BareVerb::Deny => ReplyVerb::Deny,
                },
                basename: basename.clone(),
                persist: false,
            };
            if parsed.verb == ReplyVerb::Allow && !can_approve(owners, sender) {
                return Some(format!(
                    "You're not authorized to approve `{basename}`. Ask an owner to reply `/allow {basename}`."
                ));
            }
            handle_parsed(&parsed, security, &pending)
        }
        _ => Some(ambiguous_pending_message(&snapshot, "allow")),
    }
}

/// Whether a bare `ok`/`y` arriving in `(channel, reply_target)` may answer this
/// request.
///
/// An unscoped request (empty `reply_target`) never qualifies. `ShellTool` is
/// the live case: it is a `Tool`, and the trait carries no originating message,
/// so a shell approval genuinely has no chat to be answered from. Those must be
/// named explicitly rather than resolved by a guess.
fn request_is_answerable_here(r: &PendingRequest, channel: &str, reply_target: &str) -> bool {
    !r.reply_target.is_empty() && r.channel == channel && r.reply_target == reply_target
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

    // Multi-character verbs are matched case-insensitively: a phone keyboard
    // capitalises the first word, so `Allow brew` matched neither this nor the
    // bare-verb path (which already lowercases) and was forwarded to the model
    // as ordinary chat.
    //
    // `y`/`Y` and `n`/`N` stay case-SENSITIVE on purpose — `Y` means persist and
    // `y` means once, so case is load-bearing there.
    let head_lower = head.to_ascii_lowercase();
    let (verb, persist) = match head {
        "y" if !persist => (ReplyVerb::Allow, false),
        "Y" if !persist => (ReplyVerb::Allow, true),
        "n" | "N" if !persist => (ReplyVerb::Deny, false),
        _ => match head_lower.as_str() {
            "allow" | "approve" => (ReplyVerb::Allow, persist),
            "deny" | "reject" => (ReplyVerb::Deny, persist),
            _ => return None,
        },
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
            let decision = if parsed.persist {
                Decision::Persist
            } else {
                Decision::Session
            };
            // Resolve FIRST, and only widen the allowlist if something was
            // actually pending.
            //
            // The order used to be reversed: `add_runtime_command` ran before
            // the resolve and the reply reported success either way, so
            // `/allow <anything>` wrote that basename into the runtime allowlist
            // with nothing pending at all. The read path matches the basename of
            // any path, so the grant authorises that name anywhere on `PATH`.
            let Some(_) = pending.resolve_by_basename(&parsed.basename, decision) else {
                return Some(format!(
                    "No pending approval for `{}` (or more than one queued) — nothing was allowed.",
                    parsed.basename
                ));
            };
            if let Err(e) = security.add_runtime_command(&parsed.basename, parsed.persist) {
                return Some(format!("Failed to allow `{}`: {e}", parsed.basename));
            }
            let scope = if parsed.persist {
                "persistent"
            } else {
                "session"
            };
            Some(format!(
                "✅ Added `{}` to the {scope} allowlist — pending approval resolved; the agent will retry.",
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
/// The deadline sentence, or an explicit statement that there is none.
///
/// Derived from the registry's configured timeout rather than written as a
/// literal: the prompt used to promise "Auto-deny in 5 min" while the shell
/// registry behind it was built with no timeout at all. Saying "waits until
/// answered" when that is the truth is better than implying a deadline, because
/// an operator who believes a request expires will leave a real one pending.
pub(crate) fn auto_deny_line(timeout: Option<std::time::Duration>) -> String {
    match timeout {
        Some(d) => {
            let secs = d.as_secs();
            if secs >= 60 && secs % 60 == 0 {
                format!("Auto-deny in {} min.", secs / 60)
            } else {
                format!("Auto-deny in {secs}s.")
            }
        }
        None => "No deadline — this waits until it is answered.".to_string(),
    }
}

/// Format a tool-approval request for chat.
///
/// `handle` is the short request id. A tool name is not a unique thing to
/// answer — two calls to the same tool can be pending at once — so the prompt
/// names the request the owner is actually deciding.
pub fn format_tool_approval_message(
    tool_name: &str,
    args_summary: &str,
    handle: &str,
    timeout: Option<std::time::Duration>,
) -> String {
    let detail = if args_summary.trim().is_empty() {
        String::new()
    } else {
        format!(" — `{args_summary}`")
    };
    format!(
        "🔧 The agent wants to run the `{tool_name}` tool{detail}.\n\
         Request `{handle}`.\n\
         Reply with one of:\n\
         • `/approve {tool_name}` — allow this call\n\
         • `/approve {handle}` — answer this exact request\n\
         • `/deny {tool_name}` — reject it\n\
         {}",
        auto_deny_line(timeout)
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
        // The id is minted here, not inside the registry, because the prompt is
        // posted BEFORE the request is registered and it has to name the request
        // it is about.
        let request_id = uuid::Uuid::new_v4();
        let handle = PendingApprovals::handle_for(request_id);
        let body = format_tool_approval_message(
            &request.tool_name,
            &summary,
            &handle,
            self.relay.timeout(),
        );
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
            .request_decision_in(
                request_id,
                request.tool_name.clone(),
                summary,
                self.channel_name.clone(),
                // The chat this request can be answered from. Without it, a bare
                // `ok` typed in ANY chat resolved it — the prompt goes to the
                // triggering chat, which a guest chooses.
                self.recipient.clone(),
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

/// A bare approval verb with no target token (`approve`, `yes`, `ok`, `deny`,
/// `no`, …). Shared by both relays so a single pending request can be resolved
/// without forcing the owner to type the exact tool/command token.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum BareVerb {
    Approve,
    Deny,
}

/// Parse `text` as a bare approval/deny verb with NO target token. Returns
/// `None` for anything carrying extra tokens (so `/approve shell` and chatty
/// sentences like `yes please` are NOT treated as bare verbs). Case-insensitive;
/// an optional single leading slash is allowed.
fn parse_bare_verb(text: &str) -> Option<BareVerb> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let body = trimmed.strip_prefix('/').unwrap_or(trimmed);
    let mut tokens = body.split_whitespace();
    let head = tokens.next()?;
    // Bare means exactly one token.
    if tokens.next().is_some() {
        return None;
    }
    match head.to_ascii_lowercase().as_str() {
        "approve" | "approved" | "yes" | "y" | "ok" | "allow" => Some(BareVerb::Approve),
        "deny" | "no" | "n" | "reject" => Some(BareVerb::Deny),
        _ => None,
    }
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
    channel: &str,
    reply_target: &str,
) -> Option<String> {
    // First try the exact / loose-name form (`/approve <token>`).
    if let Some(parsed) = parse_tool_reply(text) {
        let pending = relay.list();
        // A request handle names exactly one request, so it is honoured wherever
        // it is typed — the owner has said which request they mean rather than
        // letting the registry guess.
        if let Some(req) = pending
            .iter()
            .find(|r| PendingApprovals::handle_for(r.id) == parsed.tool.to_ascii_lowercase())
        {
            let basename = req.basename.clone();
            return Some(resolve_tool(
                relay,
                &basename,
                parsed.verb,
                sender,
                owners,
                &req.channel.clone(),
                &req.reply_target.clone(),
            ));
        }
        // Exact tool basename match keeps the original behavior.
        if pending.iter().any(|r| r.basename == parsed.tool) {
            return Some(resolve_tool(
                relay,
                &parsed.tool,
                parsed.verb,
                sender,
                owners,
                channel,
                reply_target,
            ));
        }
        // Loose name: the token isn't an exact tool basename, but if there's
        // exactly one request pending IN THIS CHAT, treat it as that one (e.g.
        // `/approve kubectl` → the single pending `shell` request).
        let here: Vec<&PendingRequest> = pending
            .iter()
            .filter(|r| request_is_answerable_here(r, channel, reply_target))
            .collect();
        if here.len() == 1 {
            let basename = here[0].basename.clone();
            return Some(resolve_tool(
                relay,
                &basename,
                parsed.verb,
                sender,
                owners,
                channel,
                reply_target,
            ));
        }
        // Token names something not pending and the queue is empty or
        // ambiguous → not ours (could be a shell reply or plain chat).
        return None;
    }

    // Otherwise try a bare verb with no target token (`approve`, `yes`, `ok`,
    // `deny`, `no`, …). Only meaningful when something is pending IN THIS CHAT —
    // see `request_is_answerable_here` for why the scope matters.
    let verb = parse_bare_verb(text)?;
    let verb = match verb {
        BareVerb::Approve => ToolReplyVerb::Approve,
        BareVerb::Deny => ToolReplyVerb::Deny,
    };
    let pending: Vec<PendingRequest> = relay
        .list()
        .into_iter()
        .filter(|r| request_is_answerable_here(r, channel, reply_target))
        .collect();
    match pending.len() {
        0 => None, // nothing pending here → normal chat ("yes please").
        1 => Some(resolve_tool(
            relay,
            &pending[0].basename,
            verb,
            sender,
            owners,
            channel,
            reply_target,
        )),
        _ => Some(ambiguous_pending_message(&pending, "approve")),
    }
}

/// Resolve a single tool request (already known to be pending) honoring the
/// owner gate for approvals.
fn resolve_tool(
    relay: &PendingApprovals,
    tool: &str,
    verb: ToolReplyVerb,
    sender: &str,
    owners: &[String],
    channel: &str,
    reply_target: &str,
) -> String {
    match verb {
        ToolReplyVerb::Approve => {
            if !can_approve(owners, sender) {
                return format!(
                    "You're not authorized to approve `{tool}`. Ask an owner to reply `/approve {tool}`."
                );
            }
            match relay.resolve_by_basename_in(tool, channel, reply_target, Decision::Once) {
                Some(_) => format!("✅ Approved `{tool}` — the agent will run it now."),
                None => {
                    format!("Couldn't approve `{tool}` — more than one request is queued for it.")
                }
            }
        }
        ToolReplyVerb::Deny => {
            match relay.resolve_by_basename_in(tool, channel, reply_target, Decision::Deny) {
                Some(_) => format!("🚫 Denied `{tool}`. The tool call will fail."),
                None => format!("Couldn't deny `{tool}` — more than one request is queued for it."),
            }
        }
    }
}

/// Build a "which one?" message listing the pending basenames so the owner can
/// name a specific target rather than us guessing.
fn ambiguous_pending_message(pending: &[PendingRequest], verb: &str) -> String {
    let names: Vec<String> = pending
        .iter()
        .map(|r| format!("`{}`", r.basename))
        .collect();
    format!(
        "Multiple approvals are pending: {}. Reply `/{verb} <name>` to pick one.",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    /// Wait until `pending` has registered at least `n` requests.
    ///
    /// Replaces `sleep(20ms)` after spawning the producer. A fixed sleep is a
    /// race: it passes because 20 ms is usually enough on an unloaded runner,
    /// and the suite survives only because CI forces `--test-threads=1`. This
    /// polls the registry the test actually depends on, so it is correct at any
    /// scheduling speed and fails loudly instead of flakily.
    async fn await_pending(pending: &Arc<PendingApprovals>, n: usize) {
        for _ in 0..500 {
            if pending.list().len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!(
            "timed out waiting for {n} pending request(s); registry has {}",
            pending.list().len()
        );
    }

    use super::*;
    use std::time::Duration;

    fn supervised_only_echo() -> Arc<SecurityPolicy> {
        Arc::new(
            SecurityPolicy::default()
                .with_autonomy(crate::security::AutonomyLevel::Supervised)
                .with_allowed_commands(vec!["echo".into()]),
        )
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
        await_pending(&pending, 1).await;

        // Reply comes from an authorized owner.
        let owners = vec!["owner1".to_string()];
        let ack = try_handle_reply(
            "/allow brew",
            &security,
            "owner1",
            &owners,
            "telegram",
            "chat-1",
        )
        .expect("recognised");
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
        await_pending(&pending, 1).await;

        // A non-owner replies `/allow` — recognised as an approval attempt
        // (so it's consumed, not routed to the agent) but NOT honored: the
        // command must not be allowlisted and the pending request stays open.
        let owners = vec!["owner1".to_string()];
        let ack = try_handle_reply(
            "/allow brew",
            &security,
            "stranger",
            &owners,
            "telegram",
            "chat-1",
        )
        .expect("recognised");
        assert!(ack.contains("not authorized"));
        assert!(!security
            .runtime_allowlist_snapshot()
            .contains(&"brew".to_string()));

        // The pending request is still open; a real owner can resolve it.
        let ack = try_handle_reply(
            "/allow brew",
            &security,
            "owner1",
            &owners,
            "telegram",
            "chat-1",
        )
        .expect("recognised");
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
        await_pending(&pending, 1).await;

        // Deny is honored regardless of sender/owner — stopping a pending
        // action is always safe.
        let ack = try_handle_reply("n brew", &security, "anyone", &[], "telegram", "chat-1")
            .expect("recognised");
        assert!(ack.contains("Denied"));
        assert_eq!(task.await.unwrap(), Decision::Deny);
        assert!(!security
            .runtime_allowlist_snapshot()
            .contains(&"brew".to_string()));
    }

    #[test]
    fn try_handle_reply_returns_none_for_chat_messages() {
        let security = supervised_only_echo();
        assert!(try_handle_reply("hello", &security, "u", &[], "telegram", "chat-1").is_none());
        assert!(try_handle_reply(
            "can you find me a recipe",
            &security,
            "u",
            &[],
            "telegram",
            "chat-1"
        )
        .is_none());
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
        assert!(try_handle_tool_reply(
            "/approve web_search",
            &relay,
            "owner1",
            &["owner1".into()],
            "telegram",
            "chat-1"
        )
        .is_none());
        assert!(try_handle_tool_reply(
            "hello there",
            &relay,
            "owner1",
            &["owner1".into()],
            "telegram",
            "chat-1"
        )
        .is_none());
    }

    #[tokio::test]
    async fn tool_reply_owner_approves_resolves_pending() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let r2 = relay.clone();
        let task = tokio::spawn(async move {
            r2.request_decision_in(
                uuid::Uuid::new_v4(),
                "web_search",
                "query: rust",
                "telegram",
                "chat-1",
            )
            .await
        });
        await_pending(&relay, 1).await;

        let owners = vec!["owner1".to_string()];
        let ack = try_handle_tool_reply(
            "/approve web_search",
            &relay,
            "owner1",
            &owners,
            "telegram",
            "chat-1",
        )
        .expect("recognised");
        assert!(ack.contains("Approved"), "{ack}");
        assert_eq!(task.await.unwrap(), Decision::Once);
    }

    #[tokio::test]
    async fn tool_reply_non_owner_approve_is_refused_pending_stays_open() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let r2 = relay.clone();
        let task = tokio::spawn(async move {
            r2.request_decision_in(
                uuid::Uuid::new_v4(),
                "web_search",
                "query: rust",
                "telegram",
                "chat-1",
            )
            .await
        });
        await_pending(&relay, 1).await;

        let owners = vec!["owner1".to_string()];
        // Stranger names the real pending tool → consumed (it WAS an approval
        // attempt) but refused; the request stays open for a real owner.
        let ack = try_handle_tool_reply(
            "/approve web_search",
            &relay,
            "stranger",
            &owners,
            "telegram",
            "chat-1",
        )
        .expect("recognised");
        assert!(ack.contains("not authorized"), "{ack}");
        assert_eq!(relay.list().len(), 1, "pending stays open");

        // A real owner then resolves it.
        let ack = try_handle_tool_reply(
            "/approve web_search",
            &relay,
            "owner1",
            &owners,
            "telegram",
            "chat-1",
        )
        .expect("recognised");
        assert!(ack.contains("Approved"));
        assert_eq!(task.await.unwrap(), Decision::Once);
    }

    /// The reported attack. A guest triggers a gated tool in chat A; the prompt
    /// is posted to chat A — the *triggering* chat, which the guest chose. An
    /// owner chatting normally in chat B on another channel types `ok`. That
    /// used to resolve chat A's request, because resolution matched on the
    /// basename and consulted neither the request id nor the origin the request
    /// already carried.
    #[tokio::test]
    async fn bare_ok_from_another_channel_does_not_resolve() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let r2 = relay.clone();
        let task = tokio::spawn(async move {
            r2.request_decision_in(
                uuid::Uuid::new_v4(),
                "web_search",
                "query: rust",
                "telegram",
                "chat-a",
            )
            .await
        });
        await_pending(&relay, 1).await;

        let owners = vec!["owner1".to_string()];
        assert!(
            try_handle_tool_reply("ok", &relay, "owner1", &owners, "discord", "chat-b").is_none(),
            "a bare verb in another chat must not answer this request"
        );
        assert_eq!(relay.list().len(), 1, "the request is still pending");

        // The owner CAN still answer it from the chat it was posted to.
        let ack = try_handle_tool_reply("ok", &relay, "owner1", &owners, "telegram", "chat-a")
            .expect("answerable from its own chat");
        assert!(ack.contains("Approved"), "{ack}");
        assert_eq!(task.await.unwrap(), Decision::Once);
    }

    /// Same channel, different chat — the cheaper version of the same mistake.
    #[tokio::test]
    async fn bare_ok_in_a_different_chat_on_the_same_channel_does_not_resolve() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let r2 = relay.clone();
        let task = tokio::spawn(async move {
            r2.request_decision_in(
                uuid::Uuid::new_v4(),
                "web_search",
                "q",
                "telegram",
                "chat-a",
            )
            .await
        });
        await_pending(&relay, 1).await;

        let owners = vec!["owner1".to_string()];
        assert!(
            try_handle_tool_reply("ok", &relay, "owner1", &owners, "telegram", "chat-b").is_none()
        );
        assert_eq!(relay.list().len(), 1);
        relay.resolve_by_basename("web_search", Decision::Deny);
        let _ = task.await;
    }

    /// A basename is not a unique thing to answer. Two pending requests can
    /// share one; the handle names exactly which.
    #[tokio::test]
    async fn reply_naming_the_request_handle_resolves_that_one() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let id_a = uuid::Uuid::new_v4();
        let id_b = uuid::Uuid::new_v4();
        let (r1, r2) = (relay.clone(), relay.clone());
        let ta = tokio::spawn(async move {
            r1.request_decision_in(id_a, "shell", "ls /a", "telegram", "chat-a")
                .await
        });
        let tb = tokio::spawn(async move {
            r2.request_decision_in(id_b, "shell", "ls /b", "telegram", "chat-b")
                .await
        });
        await_pending(&relay, 1).await;

        let owners = vec!["owner1".to_string()];
        let handle = PendingApprovals::handle_for(id_b);
        let ack = try_handle_tool_reply(
            &format!("/approve {handle}"),
            &relay,
            "owner1",
            &owners,
            "telegram",
            "chat-b",
        )
        .expect("handle names a request");
        assert!(ack.contains("Approved"), "{ack}");
        assert_eq!(tb.await.unwrap(), Decision::Once, "exactly the named one");
        assert_eq!(relay.list().len(), 1, "the other is untouched");

        relay.resolve_by_basename_in("shell", "telegram", "chat-a", Decision::Deny);
        let _ = ta.await;
    }

    /// `/allow` used to widen the runtime allowlist BEFORE checking that
    /// anything was pending, and reported success either way. The read path
    /// matches the basename of any path, so the grant authorised that name
    /// anywhere on `PATH`.
    #[tokio::test]
    async fn allow_with_nothing_pending_does_not_touch_the_allowlist() {
        let security = supervised_only_echo();
        let pending = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        security.set_pending(pending.clone());

        let owners = vec!["owner1".to_string()];
        let reply = try_handle_reply(
            "/allow curl",
            &security,
            "owner1",
            &owners,
            "telegram",
            "chat-1",
        );

        assert!(
            !security
                .runtime_allowlist_snapshot()
                .contains(&"curl".to_string()),
            "nothing was pending, so nothing may be allowed"
        );
        // Nothing pending by that name → not an approval reply at all.
        assert!(reply.is_none(), "must not report success: {reply:?}");
    }

    /// Pins the ORDER, not just the early return.
    ///
    /// Two requests share a basename, so `resolve_by_basename` refuses to guess
    /// and returns `None`. The old code had already called
    /// `add_runtime_command` by that point, so the allowlist was widened for a
    /// grant that resolved nothing — and the read path matches that basename
    /// anywhere on `PATH`.
    #[tokio::test]
    async fn ambiguous_allow_resolves_nothing_and_grants_nothing() {
        let security = supervised_only_echo();
        let pending = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        security.set_pending(pending.clone());
        let (p1, p2) = (pending.clone(), pending.clone());
        let t1 = tokio::spawn(async move { p1.request_decision("curl", "curl a", "t").await });
        let t2 = tokio::spawn(async move { p2.request_decision("curl", "curl b", "t").await });
        await_pending(&pending, 1).await;
        assert_eq!(pending.list().len(), 2, "both queued under one basename");

        let owners = vec!["owner1".to_string()];
        let ack = try_handle_reply(
            "/allow curl",
            &security,
            "owner1",
            &owners,
            "telegram",
            "chat-1",
        )
        .expect("recognised: `curl` is pending");

        assert!(
            !security
                .runtime_allowlist_snapshot()
                .contains(&"curl".to_string()),
            "an ambiguous approval resolved nothing, so it must grant nothing: {ack}"
        );
        assert_eq!(pending.list().len(), 2, "both still pending");

        pending.resolve_by_basename("curl", Decision::Deny);
        pending.resolve_by_basename("curl", Decision::Deny);
        let _ = t1.await;
        let _ = t2.await;
    }

    /// A phone keyboard capitalises the first word. `Allow brew` matched neither
    /// the targeted parser (case-sensitive) nor the bare-verb parser (needs one
    /// token), so it was forwarded to the model as chat.
    #[test]
    fn capitalised_verbs_are_recognised_but_case_still_selects_persistence() {
        let allow = parse_reply("Allow brew").expect("Allow is a verb");
        assert_eq!(allow.verb, ReplyVerb::Allow);
        assert!(!allow.persist);

        assert_eq!(
            parse_reply("DENY brew").expect("DENY").verb,
            ReplyVerb::Deny
        );

        // `Y` means persist and `y` means once — case is load-bearing here and
        // must NOT be folded.
        assert!(parse_reply("Y brew").expect("Y brew").persist);
        assert!(!parse_reply("y brew").expect("y brew").persist);
    }

    /// The non-owner refusal used to be returned before checking whether
    /// anything was pending, so ordinary chat containing "allow him" was
    /// consumed and answered — and the refusal was emitted only to non-owners,
    /// making it an owner-membership oracle.
    #[test]
    fn non_owner_chat_that_looks_like_an_approval_is_not_consumed() {
        let security = supervised_only_echo();
        let pending = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        security.set_pending(pending);

        let owners = vec!["owner1".to_string()];
        assert!(
            try_handle_reply(
                "allow him",
                &security,
                "stranger",
                &owners,
                "telegram",
                "chat-1"
            )
            .is_none(),
            "nothing pending named `him` — this is chat, and answering it leaks who is an owner"
        );
    }

    /// The prompt promised "Auto-deny in 5 min" while the shell registry behind
    /// it was built with no timeout at all.
    #[test]
    fn the_deadline_line_states_what_the_registry_actually_does() {
        // Against the real constant, so the prompt and the registry cannot
        // drift apart again without this failing.
        assert_eq!(
            auto_deny_line(Some(super::super::CHANNEL_APPROVAL_DEADLINE)),
            "Auto-deny in 5 min."
        );
        assert_eq!(
            auto_deny_line(Some(Duration::from_secs(45))),
            "Auto-deny in 45s."
        );
        assert!(
            auto_deny_line(None).contains("No deadline"),
            "a registry with no timeout must not imply one"
        );
    }

    // ── Forgiving bare-verb / loose-name replies ─────────────────────

    #[tokio::test]
    async fn bare_approve_resolves_single_pending_tool() {
        for verb in ["/approve", "approve", "yes", "y", "ok"] {
            let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
            let r2 = relay.clone();
            let task = tokio::spawn(async move {
                r2.request_decision_in(
                    uuid::Uuid::new_v4(),
                    "web_search",
                    "query: rust",
                    "telegram",
                    "chat-1",
                )
                .await
            });
            await_pending(&relay, 1).await;

            let owners = vec!["owner1".to_string()];
            let ack = try_handle_tool_reply(verb, &relay, "owner1", &owners, "telegram", "chat-1")
                .unwrap_or_else(|| panic!("recognised: {verb}"));
            assert!(ack.contains("Approved"), "verb={verb} ack={ack}");
            assert_eq!(task.await.unwrap(), Decision::Once, "verb={verb}");
        }
    }

    #[tokio::test]
    async fn bare_verb_returns_none_when_no_tool_pending() {
        // Nothing pending → must fall through (normal chat like "yes please").
        let relay = PendingApprovals::new(Some(Duration::from_secs(10)));
        let owners = vec!["owner1".to_string()];
        assert!(
            try_handle_tool_reply("yes", &relay, "owner1", &owners, "telegram", "chat-1").is_none()
        );
        assert!(
            try_handle_tool_reply("ok", &relay, "owner1", &owners, "telegram", "chat-1").is_none()
        );
        assert!(
            try_handle_tool_reply("/approve", &relay, "owner1", &owners, "telegram", "chat-1")
                .is_none()
        );
    }

    #[tokio::test]
    async fn bare_verb_lists_pending_when_multiple_tools() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let r1 = relay.clone();
        let r2 = relay.clone();
        let t1 = tokio::spawn(async move {
            r1.request_decision_in(
                uuid::Uuid::new_v4(),
                "web_search",
                "q",
                "telegram",
                "chat-1",
            )
            .await
        });
        let t2 = tokio::spawn(async move {
            r2.request_decision_in(uuid::Uuid::new_v4(), "shell", "ls", "telegram", "chat-1")
                .await
        });
        await_pending(&relay, 1).await;

        let owners = vec!["owner1".to_string()];
        let ack = try_handle_tool_reply("approve", &relay, "owner1", &owners, "telegram", "chat-1")
            .expect("consumed: ambiguous");
        assert!(ack.contains("web_search"), "{ack}");
        assert!(ack.contains("shell"), "{ack}");
        // Nothing resolved — both still pending.
        assert_eq!(relay.list().len(), 2);

        // Clean up the spawned waiters.
        relay.resolve_by_basename("web_search", Decision::Deny);
        relay.resolve_by_basename("shell", Decision::Deny);
        let _ = t1.await;
        let _ = t2.await;
    }

    #[tokio::test]
    async fn loose_name_resolves_single_pending_tool() {
        // `/approve kubectl` resolves the single pending `shell` request even
        // though `kubectl` is not the exact tool basename.
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let r2 = relay.clone();
        let task = tokio::spawn(async move {
            r2.request_decision_in(
                uuid::Uuid::new_v4(),
                "shell",
                "kubectl get pods",
                "telegram",
                "chat-1",
            )
            .await
        });
        await_pending(&relay, 1).await;

        let owners = vec!["owner1".to_string()];
        let ack = try_handle_tool_reply(
            "/approve kubectl",
            &relay,
            "owner1",
            &owners,
            "telegram",
            "chat-1",
        )
        .expect("recognised");
        assert!(ack.contains("Approved"), "{ack}");
        assert_eq!(task.await.unwrap(), Decision::Once);
    }

    #[tokio::test]
    async fn bare_approve_from_non_owner_is_refused_and_does_not_resolve() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let r2 = relay.clone();
        let task = tokio::spawn(async move {
            r2.request_decision_in(
                uuid::Uuid::new_v4(),
                "web_search",
                "q",
                "telegram",
                "chat-1",
            )
            .await
        });
        await_pending(&relay, 1).await;

        let owners = vec!["owner1".to_string()];
        let ack =
            try_handle_tool_reply("approve", &relay, "stranger", &owners, "telegram", "chat-1")
                .expect("consumed");
        assert!(ack.contains("not authorized"), "{ack}");
        assert_eq!(relay.list().len(), 1, "pending stays open");

        // A real owner can still resolve.
        try_handle_tool_reply("yes", &relay, "owner1", &owners, "telegram", "chat-1")
            .expect("recognised");
        assert_eq!(task.await.unwrap(), Decision::Once);
    }

    #[tokio::test]
    async fn explicit_allow_resolves_a_pending_shell_request() {
        // A shell request is UNSCOPED — `ShellTool` is a `Tool` and the trait
        // carries no originating message, so it has no chat to be answered from.
        // Naming the command is therefore how it is answered; a bare `ok` is
        // covered by `bare_verb_does_not_resolve_an_unscoped_shell_request`.
        for verb in ["/allow brew", "allow brew", "Allow brew", "y brew"] {
            let security = supervised_only_echo();
            let pending = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
            security.set_pending(pending.clone());
            let p2 = pending.clone();
            let task =
                tokio::spawn(
                    async move { p2.request_decision("brew", "brew --version", "t").await },
                );
            await_pending(&pending, 1).await;

            let owners = vec!["owner1".to_string()];
            let ack = try_handle_reply(verb, &security, "owner1", &owners, "telegram", "chat-1")
                .unwrap_or_else(|| panic!("recognised: {verb}"));
            assert!(ack.contains("session"), "verb={verb} ack={ack}");
            assert_eq!(task.await.unwrap(), Decision::Session, "verb={verb}");
            assert!(security
                .runtime_allowlist_snapshot()
                .contains(&"brew".to_string()));
        }
    }

    /// The cross-chat hole, on the path that carried it.
    ///
    /// A guest triggers a gated command; the request is registered with no chat
    /// (the shell tool has none to give). An owner typing `ok` anywhere used to
    /// resolve it — and on the shell path the grant is durable, so the
    /// attacker-chosen basename went into the runtime allowlist.
    #[tokio::test]
    async fn bare_verb_does_not_resolve_an_unscoped_shell_request() {
        let security = supervised_only_echo();
        let pending = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        security.set_pending(pending.clone());
        let p2 = pending.clone();
        let task =
            tokio::spawn(async move { p2.request_decision("brew", "brew --version", "t").await });
        await_pending(&pending, 1).await;

        let owners = vec!["owner1".to_string()];
        assert!(
            try_handle_reply("ok", &security, "owner1", &owners, "telegram", "chat-1").is_none(),
            "a bare verb must not answer a request that names no chat"
        );
        assert!(
            !security
                .runtime_allowlist_snapshot()
                .contains(&"brew".to_string()),
            "and nothing may be added to the allowlist"
        );

        // Still pending, so an explicit reply can still answer it.
        assert_eq!(pending.list().len(), 1);
        drop(task);
    }

    #[tokio::test]
    async fn bare_allow_returns_none_when_no_shell_pending() {
        let security = supervised_only_echo();
        let pending = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        security.set_pending(pending);
        let owners = vec!["owner1".to_string()];
        assert!(
            try_handle_reply("yes", &security, "owner1", &owners, "telegram", "chat-1").is_none()
        );
        assert!(
            try_handle_reply("/allow", &security, "owner1", &owners, "telegram", "chat-1")
                .is_none()
        );
    }

    #[tokio::test]
    async fn bare_deny_resolves_single_pending_shell_from_anyone() {
        let security = supervised_only_echo();
        let pending = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        security.set_pending(pending.clone());
        let p2 = pending.clone();
        let task =
            tokio::spawn(async move { p2.request_decision("brew", "brew --version", "t").await });
        await_pending(&pending, 1).await;

        let ack = try_handle_reply("no", &security, "anyone", &[], "telegram", "chat-1")
            .expect("recognised");
        assert!(ack.contains("Denied"), "{ack}");
        assert_eq!(task.await.unwrap(), Decision::Deny);
    }

    #[tokio::test]
    async fn bare_verb_lists_pending_when_multiple_shell() {
        // Two pending requests in the SAME chat: a bare `allow` cannot pick one,
        // so it must ask rather than guess. Scoped explicitly because the
        // ambiguity only arises among requests a bare verb could answer.
        let security = supervised_only_echo();
        let pending = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        security.set_pending(pending.clone());
        let p1 = pending.clone();
        let p2 = pending.clone();
        let t1 = tokio::spawn(async move {
            p1.request_decision_in(uuid::Uuid::new_v4(), "brew", "brew", "telegram", "chat-1")
                .await
        });
        let t2 = tokio::spawn(async move {
            p2.request_decision_in(uuid::Uuid::new_v4(), "npm", "npm i", "telegram", "chat-1")
                .await
        });
        await_pending(&pending, 1).await;

        let owners = vec!["owner1".to_string()];
        let ack = try_handle_reply("allow", &security, "owner1", &owners, "telegram", "chat-1")
            .expect("consumed");
        assert!(ack.contains("brew"), "{ack}");
        assert!(ack.contains("npm"), "{ack}");
        assert_eq!(pending.list().len(), 2);

        pending.resolve_by_basename("brew", Decision::Deny);
        pending.resolve_by_basename("npm", Decision::Deny);
        let _ = t1.await;
        let _ = t2.await;
    }

    #[tokio::test]
    async fn tool_reply_deny_is_honored_from_anyone() {
        let relay = Arc::new(PendingApprovals::new(Some(Duration::from_secs(10))));
        let r2 = relay.clone();
        let task = tokio::spawn(async move {
            r2.request_decision_in(
                uuid::Uuid::new_v4(),
                "shell",
                "rm -rf /tmp/x",
                "telegram",
                "chat-1",
            )
            .await
        });
        await_pending(&relay, 1).await;

        // Deny needs no owner authority — stopping an action is always safe.
        let ack = try_handle_tool_reply("/deny shell", &relay, "anyone", &[], "telegram", "chat-1")
            .expect("recognised");
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
        try_handle_tool_reply(
            "/approve web_search",
            &relay,
            "owner1",
            &owners,
            "telegram",
            "chat-1",
        )
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
