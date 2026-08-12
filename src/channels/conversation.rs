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

    /// The stable conversation id: `surface:sender[:thread]`, with `:` inside
    /// `sender` and `thread` percent-encoded as `%3A` (and a literal `%` as
    /// `%25`, so the encoding is itself unambiguous).
    ///
    /// The encoding is not cosmetic. Matrix senders are `@localpart:homeserver`
    /// (`src/channels/matrix.rs`), and Telegram forum targets are
    /// `chat_id:thread_id`, so a plain join made two different conversations
    /// resolve to one id: `("matrix", "@bob", Some("example.org"))` and
    /// `("matrix", "@bob:example.org", None)` both produced
    /// `matrix:@bob:example.org`. The previous docstring claimed this function
    /// was collision-free, which invited callers to rely on a property it did
    /// not have.
    ///
    /// Ids for senders and threads containing no `:` or `%` are unchanged, so
    /// existing call sites keep their current values.
    pub fn resolve(&self) -> String {
        match self.thread {
            Some(thread) if !thread.is_empty() => format!(
                "{}:{}:{}",
                self.surface,
                encode_component(self.sender),
                encode_component(thread)
            ),
            _ => format!("{}:{}", self.surface, encode_component(self.sender)),
        }
    }
}

/// Percent-encode the two characters that would otherwise make the joined id
/// ambiguous. `%` first, so an input already containing `%3A` cannot be
/// confused with an encoded colon.
fn encode_component(value: &str) -> String {
    if !value.contains(':') && !value.contains('%') {
        return value.to_string();
    }
    value.replace('%', "%25").replace(':', "%3A")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reported shape of the leak, at the id level: one person's DM and a
    /// group they share with the bot must not resolve to one conversation.
    #[test]
    fn different_chats_with_the_same_person_are_different_conversations() {
        let dm = ConversationKey::new("telegram", "12345").resolve();
        let group = ConversationKey::new("telegram", "-100999").resolve();
        assert_ne!(dm, group);
    }

    /// Matrix senders are `@localpart:homeserver` and Telegram forum targets are
    /// `chat_id:thread_id`, so a plain `:` join made two different conversations
    /// produce one id. The docstring used to claim this was collision-free.
    #[test]
    fn a_colon_in_the_sender_cannot_forge_a_thread_scope() {
        let threaded = ConversationKey::new("matrix", "@bob")
            .in_thread(Some("example.org"))
            .resolve();
        let plain = ConversationKey::new("matrix", "@bob:example.org").resolve();
        assert_ne!(
            threaded, plain,
            "a colon inside the sender must not read as the thread separator"
        );
    }

    /// The encoding must itself be unambiguous, or it just moves the collision.
    #[test]
    fn an_encoded_colon_in_the_input_is_not_confused_with_a_real_one() {
        let literal = ConversationKey::new("telegram", "a%3Ab").resolve();
        let actual = ConversationKey::new("telegram", "a:b").resolve();
        assert_ne!(literal, actual);
    }

    /// Ids for ordinary senders keep their existing value, so this is not a
    /// silent re-keying of every conversation.
    #[test]
    fn ordinary_ids_are_unchanged_by_the_encoding() {
        assert_eq!(
            ConversationKey::new("discord", "C123").resolve(),
            "discord:C123"
        );
    }

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
