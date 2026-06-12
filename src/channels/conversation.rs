//! Conversation identity across surfaces.
//!
//! Every surface defines "a conversation" differently — a Telegram DM, a
//! Discord thread, a Slack thread, a web session. The agent runtime needs a
//! single, stable id per conversation so memory and per-conversation history
//! scope correctly without leaking across chats.
//!
//! [`ConversationKey::resolve`] is the one place that turns the raw
//! `(surface, sender, thread)` triple into that id, using the deterministic
//! `surface:sender[:thread]` scheme (mirrors Hermes' `build_session_key`).
//! It replaces ad-hoc `format!("{channel}:{sender}")` call sites so the format
//! lives in exactly one tested place and gains thread-awareness for free —
//! Discord/Slack threads resolve to their own conversation instead of being
//! merged into the parent channel.
//!
//! This is the PR4 foundation of `docs/unified-agent-runtime-plan.md`. Agent
//! *capability* is unified across surfaces; conversation *identity* stays
//! surface-scoped, and this is where that scoping is defined.

/// The inputs needed to resolve a stable conversation id for one message.
///
/// `surface` is the channel name (`"telegram"`, `"discord"`, …) or `"webhook"`
/// / `"cli"`. `sender` is the per-surface user/chat id. `thread` is an optional
/// finer-grained scope (forum topic, Discord/Slack thread) — `None`/empty means
/// the conversation is the whole DM/channel.
#[derive(Debug, Clone, Copy)]
pub struct ConversationKey<'a> {
    pub surface: &'a str,
    pub sender: &'a str,
    pub thread: Option<&'a str>,
}

impl<'a> ConversationKey<'a> {
    /// A whole-DM/channel conversation (no thread sub-scope).
    pub fn new(surface: &'a str, sender: &'a str) -> Self {
        Self {
            surface,
            sender,
            thread: None,
        }
    }

    /// Attach a thread/topic sub-scope so it resolves to its own conversation.
    pub fn in_thread(mut self, thread: Option<&'a str>) -> Self {
        self.thread = thread.filter(|t| !t.is_empty());
        self
    }

    /// The stable conversation id: `surface:sender[:thread]`.
    ///
    /// Deterministic and collision-free across surfaces because the surface
    /// name prefixes the id. Behaviour-preserving for existing call sites that
    /// used `format!("{surface}:{sender}")` when no thread is set.
    pub fn resolve(&self) -> String {
        match self.thread {
            Some(thread) if !thread.is_empty() => {
                format!("{}:{}:{}", self.surface, self.sender, thread)
            }
            _ => format!("{}:{}", self.surface, self.sender),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_channel_id_is_surface_and_sender() {
        let key = ConversationKey::new("telegram", "12345");
        assert_eq!(key.resolve(), "telegram:12345");
    }

    #[test]
    fn backward_compatible_with_old_format() {
        // Existing gateway key was `format!("{channel_name}:{sender}")`.
        let surface = "webhook";
        let sender = "+15551234";
        assert_eq!(
            ConversationKey::new(surface, sender).resolve(),
            format!("{surface}:{sender}")
        );
    }

    #[test]
    fn thread_scopes_to_its_own_conversation() {
        let parent = ConversationKey::new("discord", "chan99").resolve();
        let thread = ConversationKey::new("discord", "chan99")
            .in_thread(Some("thread42"))
            .resolve();
        assert_eq!(thread, "discord:chan99:thread42");
        assert_ne!(parent, thread, "a thread is a distinct conversation");
    }

    #[test]
    fn empty_thread_is_treated_as_no_thread() {
        let a = ConversationKey::new("slack", "u1")
            .in_thread(Some(""))
            .resolve();
        let b = ConversationKey::new("slack", "u1").resolve();
        assert_eq!(a, b);
    }
}
