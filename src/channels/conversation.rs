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

/// Reverse of [`encode_component`]. The encoder writes `%` before `:`, so the
/// inverse reads `%3A` before `%25` — undoing a percent sign otherwise leaves a
/// `3A` next to a colon and the join round-trips wrong. Inverse is part of the
/// same tested module so the encode/decode pair cannot drift.
pub fn decode_component(value: &str) -> String {
    value.replace("%3A", ":").replace("%25", "%")
}

/// Parse a place string produced by [`ConversationKey::resolve`] back into the
/// three parts the key was built from. The place is `surface:sender[:thread]`
/// with `:` and `%` percent-encoded inside the components, so a literal `:`
/// always means a separator.
///
/// Returns an error on a string that does not start with `surface:` — every
/// resolved place has at least one separator, so the absence is a misconfigured
/// input the caller (cron origin / dispatch key test) should refuse rather than
/// silently widen.
pub fn parse_place(place: &str) -> Result<(String, String, Option<String>), String> {
    let (surface, rest) = place.split_once(':').ok_or_else(|| {
        format!("cannot parse view place as cron origin: no surface separator in '{place}'")
    })?;
    // A literal `:` in `rest` is the sender/thread separator. `%3A` inside an
    // encoded component is not a separator, so `split_once` only catches it.
    let (sender, thread) = match rest.split_once(':') {
        Some((s, t)) => (s.to_string(), Some(decode_component(t))),
        None => (decode_component(rest), None),
    };
    Ok((surface.to_string(), sender, thread))
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

    // ── parse_place: inverse of resolve ───────────────────────────────
    //
    // The cron scheduler rebuilds the per-job view from origin columns; the
    // columns come from `cron_origin_for_view`, which in turn comes from the
    // dispatcher-built place string. If `parse_place` does not undo
    // `resolve`, the job reads a place nothing writes to and the chat's
    // notes are gone. These tests pin the round trip for every shape the
    // dispatcher actually produces.

    /// A plain chat: the place is `surface:sender`. Parse returns the two
    /// parts and no thread.
    #[test]
    fn parse_place_for_a_plain_chat() {
        let place = ConversationKey::new("telegram", "12345").resolve();
        let (surface, sender, thread) = parse_place(&place).expect("plain chat parses");
        assert_eq!(surface, "telegram");
        assert_eq!(sender, "12345");
        assert_eq!(thread, None);
    }

    /// A threaded chat: the place is `surface:sender:thread`. Parse splits at
    /// the second literal `:` and decodes the thread.
    #[test]
    fn parse_place_for_a_threaded_chat() {
        let place = ConversationKey::new("slack", "C0CHAN")
            .in_thread(Some("1700000000.000500"))
            .resolve();
        let (surface, sender, thread) = parse_place(&place).expect("threaded chat parses");
        assert_eq!(surface, "slack");
        assert_eq!(sender, "C0CHAN");
        assert_eq!(thread.as_deref(), Some("1700000000.000500"));
    }

    /// A sender containing `:` (Matrix `@bob:example.org`, or a Telegram forum
    /// topic that the channel packs into reply_target) is percent-encoded by
    /// `resolve`, so the only literal `:` is the surface separator. Parse must
    /// decode the sender, not split it.
    #[test]
    fn parse_place_for_a_sender_with_colon() {
        let place = ConversationKey::new("matrix", "@bob:example.org").resolve();
        let (surface, sender, thread) = parse_place(&place).expect("encoded sender parses");
        assert_eq!(surface, "matrix");
        assert_eq!(sender, "@bob:example.org");
        assert_eq!(thread, None);
    }

    /// A sender containing `%` (e.g. `100%`) is encoded with `%25`, and parse
    /// must undo that without confusing it with the encoded form of a colon.
    #[test]
    fn parse_place_for_a_sender_with_percent() {
        let place = ConversationKey::new("telegram", "100%").resolve();
        let (surface, sender, thread) = parse_place(&place).expect("percent sender parses");
        assert_eq!(surface, "telegram");
        assert_eq!(sender, "100%");
        assert_eq!(thread, None);
    }

    /// A thread containing `%` round-trips too.
    #[test]
    fn parse_place_for_a_thread_with_percent() {
        let place = ConversationKey::new("slack", "C")
            .in_thread(Some("100%"))
            .resolve();
        let (_, sender, thread) = parse_place(&place).expect("percent thread parses");
        assert_eq!(sender, "C");
        assert_eq!(thread.as_deref(), Some("100%"));
    }

    /// A thread containing `:` round-trips too — the encoded form is `%3A`,
    /// not a literal `:` that would be mistaken for a separator.
    #[test]
    fn parse_place_for_a_thread_with_colon() {
        let place = ConversationKey::new("slack", "C")
            .in_thread(Some("a:b"))
            .resolve();
        let (_, sender, thread) = parse_place(&place).expect("colon thread parses");
        assert_eq!(sender, "C");
        assert_eq!(thread.as_deref(), Some("a:b"));
    }

    /// A string with no `:` is not a valid place. Parse refuses rather than
    /// silently widening into `("", input, None)`.
    #[test]
    fn parse_place_refuses_a_string_without_a_separator() {
        let result = parse_place("noseparator");
        assert!(result.is_err(), "a string without `:` must be refused");
    }

    /// The whole point of the inverse: rebuild from the parsed parts and the
    /// resolved place is exactly the original. Pinned for every shape the
    /// dispatcher produces, so a regression in either side of the round trip
    /// fails here.
    #[test]
    fn parse_place_round_trips_through_resolve() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("telegram", "12345", None),
            ("slack", "C0CHAN", Some("1700000000.000500")),
            ("matrix", "@bob:example.org", None),
            ("telegram", "100%", None),
            ("discord", "chan99", Some("100%")),
            ("slack", "C", Some("a:b")),
            ("telegram", "-100123456:77", None), // Telegram forum topic
        ];
        for (surface, sender, thread) in cases {
            let place = ConversationKey::new(surface, sender)
                .in_thread(*thread)
                .resolve();
            let (s2, sender2, thread2) =
                parse_place(&place).unwrap_or_else(|e| panic!("place {place:?} must parse, {e}"));
            assert_eq!(s2, *surface, "surface round-trip for {place:?}");
            assert_eq!(sender2, *sender, "sender round-trip for {place:?}");
            assert_eq!(
                thread2.as_deref(),
                *thread,
                "thread round-trip for {place:?}"
            );
            // And rebuilding via the parsed parts gives the same string.
            let rebuilt = ConversationKey::new(&s2, &sender2)
                .in_thread(thread2.as_deref())
                .resolve();
            assert_eq!(rebuilt, place, "rebuild round-trip for {place:?}");
        }
    }
}
