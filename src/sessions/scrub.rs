//! Secret scrubbing on the channel-recording write path.
//!
//! Three scrubbers run on every recorded message — user text and reply —
//! before the `INSERT`. The order is fixed, with `scrub_secret_patterns` first
//! (catches the well-known `sk-…`, `ghp_…`, etc. prefixes providers leak in
//! errors) and `scrub_credentials` second (catches the `api_key =
//! "value"` shape a tool might write into memory or a tool result might
//! carry). The third pass, [`redact_media_payloads`], replaces attachment
//! markers whose body is a base64 data URI with a short placeholder, so an
//! image the agent already saw once does not sit on disk for thirty days. The
//! three are idempotent so a double-run produces the same string.
//!
//! **Known limit.** Neither secret scrubber catches a secret written as a
//! plain sentence ("the token string is abcdefghijklmno"). Both rely on
//! shape — a prefix list and a `key=value` pattern — and the prose form
//! carries neither. `Authorization: Bearer …` headers, bot tokens whose
//! prefix is not on `scrub_secret_patterns`'s list, and JWTs are not caught.
//! This is the limit the operator-facing copy states plainly; a general
//! natural-language scrubber would be a different and far larger change.

use crate::agent::loop_::scrub_credentials;
use crate::providers::scrub_secret_patterns;
use regex::Regex;
use std::sync::LazyLock;

/// Match any of the five marker kinds followed by a body that opens with
/// `data:` (a base64 data URI). The `[^\]\n]+` body class stops at the
/// first closing bracket *or* the first newline — a marker that opens with
/// `data:` and never closes on its line is left alone, because the only way a
/// channel parser could give us back a path that lives on the next line is
/// through a marker it could not close, and that path is what the model
/// already saw.
///
/// Captures the kind tag (`IMAGE`, `DOCUMENT`, `VIDEO`, `AUDIO`, `VOICE`)
/// without the trailing colon, so the replacement can rebuild the marker.
static PAYLOAD_MARKER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<open>\[(?:IMAGE|DOCUMENT|VIDEO|AUDIO|VOICE):)data:[^\]\n]+\]").unwrap()
});

/// Run both secret scrubbers on `input` and replace any attachment marker
/// whose body is a base64 data URI with a short placeholder, then return the
/// joined result. Kept as a single entry point so the channel recording site
/// has exactly one place to call — no chance of one scrubber getting skipped
/// on a future refactor.
pub fn scrub_channel_message(input: &str) -> String {
    let cleaned = scrub_secret_patterns(input);
    let scrubbed = scrub_credentials(&cleaned);
    redact_media_payloads(&scrubbed)
}

/// Replace any attachment marker whose body is a base64 data URI with a short
/// placeholder. The kind is preserved (`[IMAGE:…]` stays `[IMAGE:…]`) and a
/// hint that the payload was withheld goes inside the brackets, so a future
/// `session_search` hit still tells the operator an image was attached and
/// they can look at the channel history for the file.
///
/// Path-only markers (`[IMAGE:/w/foo.png]`, `[DOCUMENT:notes/menu.txt]`) are
/// kept verbatim: a path is not a payload. The body of a marker that opens
/// with `data:` but has no closing `]` before the next line is also left
/// alone — the channel parser is the only thing that recovers those, and
/// the recovered text is a path the model already saw.
fn redact_media_payloads(input: &str) -> String {
    PAYLOAD_MARKER_REGEX
        .replace_all(input, |caps: &regex::Captures<'_>| {
            // `caps.name("open")` is always present in the regex above and
            // covers `[KIND:` with the trailing colon. The replacement
            // appends `payload withheld]` so the marker still says what kind
            // was attached.
            let open = caps.name("open").map(|m| m.as_str()).unwrap_or("[");
            format!("{open}payload withheld]")
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `sk-` token in user text is scrubbed before storage.
    #[test]
    fn scrub_channel_message_redacts_a_token_prefix() {
        let out = scrub_channel_message("please save sk-abcdef1234567890XYZ for later");
        assert!(!out.contains("abcdef1234567890XYZ"), "got: {out}");
        assert!(out.contains("REDACTED"));
    }

    /// A `key=value` style secret in a reply is scrubbed before storage.
    #[test]
    fn scrub_channel_message_redacts_a_kv_pair() {
        let out = scrub_channel_message(r#"saved with api_key: "supersecretvalue1234""#);
        assert!(!out.contains("supersecretvalue1234"), "got: {out}");
        assert!(out.contains("REDACTED"));
    }

    /// Ordinary words that contain a token prefix (`task-`, `risk-`) are
    /// stored as written.
    #[test]
    fn scrub_channel_message_keeps_words_that_contain_a_token_prefix() {
        let msg = "the risk-assessment task-force plan";
        assert_eq!(scrub_channel_message(msg), msg);
    }

    /// An unquoted base64 value is taken whole, `/`, `+` and `=` included, and
    /// a second pass leaves the stored text as it is.
    #[test]
    fn scrub_channel_message_takes_a_whole_unquoted_base64_value() {
        let once = scrub_channel_message("password=abcdEFGH/ijklMNOP+qrst== ok");
        assert_eq!(once, "password=abcd*[REDACTED] ok");
        assert_eq!(scrub_channel_message(&once), once);
    }

    /// The token-prefix pass runs first and leaves `[REDACTED]` as the value;
    /// the `key=value` pass must not split that marker into a second redaction.
    #[test]
    fn scrub_channel_message_redacts_a_prefixed_key_once() {
        let once = scrub_channel_message("OPENAI_API_KEY=sk-abcdef1234567890XYZ");
        assert_eq!(once, "OPENAI_API_KEY=[REDACTED]");
        assert_eq!(scrub_channel_message(&once), once);

        let once = scrub_channel_message("token: ghp_abcdefghij0123456789XYZ");
        assert_eq!(once, "token: [REDACTED]");
        assert_eq!(scrub_channel_message(&once), once);
    }

    /// A marker left by the first pass at the start of a longer unquoted value
    /// is part of that value: the whole value goes, and the four kept
    /// characters can fall inside the marker.
    #[test]
    fn scrub_channel_message_takes_the_whole_value_that_starts_with_a_marker() {
        let once = scrub_channel_message("OPENAI_API_KEY=sk-abcdef1234567890XYZ/more+stuff");
        assert_eq!(once, "OPENAI_API_KEY=[RED*[REDACTED]");
        assert!(
            !once.contains("more") && !once.contains("stuff"),
            "got: {once}"
        );
        assert_eq!(scrub_channel_message(&once), once);
    }

    /// Known limit: a tail of fewer than seven characters after a prefixed key
    /// leaves the value under eight units (the marker counts as one), so the
    /// marker and the tail are stored as they are.
    #[test]
    fn scrub_channel_message_keeps_a_short_tail_after_a_prefixed_key() {
        let once = scrub_channel_message("password=ghp_abcdefghij0123456789XYZ/abc12");
        assert_eq!(once, "password=[REDACTED]/abc12");
        assert_eq!(scrub_channel_message(&once), once);
    }

    /// A marker in the middle of an unquoted value does not end the value.
    #[test]
    fn scrub_channel_message_takes_the_whole_value_with_a_marker_in_the_middle() {
        let once = scrub_channel_message("password=ab[REDACTED]cdefgh");
        assert_eq!(once, "password=ab[R*[REDACTED]");
        assert!(!once.contains("cdefgh"), "got: {once}");
        assert_eq!(scrub_channel_message(&once), once);
    }

    /// A quoted key goes through both passes: the first leaves a marker inside
    /// the quotes, the second keeps four characters of it. The result is
    /// stable and holds no part of the key.
    #[test]
    fn scrub_channel_message_stores_a_quoted_prefixed_key_stably() {
        let once = scrub_channel_message(r#"api_key: "sk-abcdefghij0123456789XYZ""#);
        assert_eq!(once, r#""api_key": "[RED*[REDACTED]""#);
        assert!(!once.contains("abcdefghij"), "got: {once}");
        assert_eq!(scrub_channel_message(&once), once);
    }

    /// A key behind a slash after `=` is still taken, and the result is stable.
    #[test]
    fn scrub_channel_message_redacts_a_key_after_a_slash_in_a_value() {
        let once = scrub_channel_message("password=/sk-abcdefghij0123456789XYZ");
        assert!(!once.contains("abcdefghij0123456789XYZ"), "got: {once}");
        assert_eq!(once, "password=/[REDACTED]");
        assert_eq!(scrub_channel_message(&once), once);
    }

    /// Idempotent: scrubbing twice produces the same string.
    #[test]
    fn scrub_channel_message_is_idempotent() {
        let once = scrub_channel_message("sk-abcdef1234567890XYZ");
        let twice = scrub_channel_message(&once);
        assert_eq!(once, twice);
    }

    /// Plain text with no secret shape survives unchanged.
    #[test]
    fn scrub_channel_message_leaves_normal_text_alone() {
        let msg = "the fox jumps over the lazy dog";
        assert_eq!(scrub_channel_message(msg), msg);
    }

    /// A `[IMAGE:data:image/png;base64,…]` marker is recorded with the
    /// payload replaced by a placeholder. The kind is kept so the operator
    /// still knows an image was attached.
    #[test]
    fn scrub_channel_message_replaces_a_base64_image_marker() {
        let payload = "A".repeat(64);
        let msg = format!("see this [IMAGE:data:image/png;base64,{payload}] ok");
        let out = scrub_channel_message(&msg);
        assert!(!out.contains(&payload), "payload must not survive: {out}");
        assert!(!out.contains("base64"), "the data-URI hint must go: {out}");
        assert!(
            out.contains("[IMAGE:payload withheld]"),
            "the placeholder must name the kind: {out}"
        );
        assert!(
            out.contains("see this ") && out.contains(" ok"),
            "the surrounding text must survive: {out}"
        );
    }

    /// Path-only markers (the common case for `[IMAGE:/w/foo.png]`,
    /// `[DOCUMENT:notes/menu.txt]`, etc.) are left untouched — there is no
    /// payload to withhold.
    #[test]
    fn scrub_channel_message_keeps_path_only_markers() {
        let msg = "see [IMAGE:/w/foo.png] and [DOCUMENT:notes/menu.txt]";
        assert_eq!(scrub_channel_message(msg), msg);
    }

    /// Every one of the five marker kinds is enumerated: a payload marker
    /// for any of them is replaced with the same `[KIND:payload withheld]`
    /// shape. None of the kinds can carry a data URI today, but the recording
    /// scrubber treats them all the same so a future kind carrying a payload
    /// does not silently land on disk.
    #[test]
    fn scrub_channel_message_redacts_a_payload_marker_for_every_kind() {
        for kind in ["IMAGE", "DOCUMENT", "VIDEO", "AUDIO", "VOICE"] {
            let payload = "B".repeat(32);
            let msg = format!("[{kind}:data:application/octet-stream;base64,{payload}]");
            let out = scrub_channel_message(&msg);
            assert!(
                !out.contains(&payload),
                "{kind} payload must not survive: {out}"
            );
            assert!(
                out.contains(&format!("[{kind}:payload withheld]")),
                "{kind} placeholder must name the kind: {out}"
            );
        }
    }

    /// Pin test for the docs (`docs/reference/channels.md` and the module
    /// doc above): a plain sentence that mentions a key like `token` but
    /// uses prose instead of a `key: value` shape is left untouched. The
    /// regex requires the separator `[:=]` between key and value, so a
    /// value attached by prose (`is`, `was`, `equals`) does not match. The
    /// prose example must stay unredacted: it is the docs' illustration of
    /// a sentence the scrubber is not meant to catch.
    #[test]
    fn scrub_channel_message_leaves_a_prose_token_unchanged() {
        // No `:` or `=` between the key and the value, so the regex skips it.
        let msg = "the token string is abcdefghijklmno";
        let out = scrub_channel_message(msg);
        assert_eq!(
            out, msg,
            "a prose sentence with no key:value separator must not be redacted: {out}"
        );
        assert!(
            !out.contains("REDACTED"),
            "the doc example must remain unredacted: {out}"
        );
    }

    /// An unclosed attachment marker on one line, followed by another marker
    /// shape later in the same input, must not be matched across the newline:
    /// the regex stops at the first line break. Without that, an
    /// `[IMAGE:data:abc\ntext [note] more` would consume the second marker as
    /// part of the payload and rewrite the model-visible text. The recorded
    /// path keeps the unclosed fragment; the channel parser is the only
    /// thing that recovers the original path.
    #[test]
    fn scrub_channel_message_leaves_an_unclosed_marker_on_one_line_alone() {
        let msg = "[IMAGE:data:abc\ntext [note] more";
        let out = scrub_channel_message(msg);
        assert_eq!(
            out, msg,
            "an unclosed marker followed by a newline must not cross lines: {out}"
        );
        assert!(
            !out.contains("payload withheld"),
            "the unclosed marker must not be rewritten: {out}"
        );
    }
}
