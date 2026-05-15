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
pub fn try_handle_reply(text: &str, security: &SecurityPolicy) -> Option<String> {
    let parsed = parse_reply(text)?;
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
        let pending = Arc::new(PendingApprovals::new(Duration::from_secs(10)));
        security.set_pending(pending.clone());

        // Producer side: shell tool would call this.
        let pending2 = pending.clone();
        let task = tokio::spawn(async move {
            pending2
                .request_decision("brew", "brew --version", "telegram")
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let ack = try_handle_reply("/allow brew", &security).expect("recognised");
        assert!(ack.contains("session"));
        assert!(ack.contains("retry"));

        let decision = task.await.unwrap();
        assert_eq!(decision, Decision::Session);
        assert!(security
            .runtime_allowlist_snapshot()
            .contains(&"brew".to_string()));
    }

    #[tokio::test]
    async fn try_handle_reply_deny_keeps_allowlist_clean() {
        let security = supervised_only_echo();
        let pending = Arc::new(PendingApprovals::new(Duration::from_secs(10)));
        security.set_pending(pending.clone());

        let pending2 = pending.clone();
        let task = tokio::spawn(async move {
            pending2
                .request_decision("brew", "brew --version", "telegram")
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let ack = try_handle_reply("n brew", &security).expect("recognised");
        assert!(ack.contains("Denied"));
        assert_eq!(task.await.unwrap(), Decision::Deny);
        assert!(!security
            .runtime_allowlist_snapshot()
            .contains(&"brew".to_string()));
    }

    #[test]
    fn try_handle_reply_returns_none_for_chat_messages() {
        let security = supervised_only_echo();
        assert!(try_handle_reply("hello", &security).is_none());
        assert!(try_handle_reply("can you find me a recipe", &security).is_none());
    }
}
