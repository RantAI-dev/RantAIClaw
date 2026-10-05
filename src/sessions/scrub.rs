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
//! plain sentence ("here is my token: mysecretvalue123"). Both rely on shape
//! — a prefix list and a `key=value` pattern — and `mysecretvalue123` carries
//! neither. This is the limit the operator-facing copy states plainly; a
//! general natural-language scrubber would be a different and far larger
//! change.

use crate::agent::loop_::scrub_credentials;
use crate::providers::scrub_secret_patterns;
use regex::Regex;
use std::sync::LazyLock;

/// Match any of the five marker kinds followed by a body that opens with
/// `data:` (a base64 data URI). The non-greedy `.+?` plus the explicit `]`
/// stop at the first closing bracket on the same marker, so a nested bracket
/// in the body would only confuse the parser in pathological cases (none of
/// the live marker kinds accept one in practice).
///
/// Captures the kind tag (`IMAGE`, `DOCUMENT`, `VIDEO`, `AUDIO`, `VOICE`)
/// without the trailing colon, so the replacement can rebuild the marker.
static PAYLOAD_MARKER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<open>\[(?:IMAGE|DOCUMENT|VIDEO|AUDIO|VOICE):)data:[^\]]+\]").unwrap()
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
/// kept verbatim: a path is not a payload. The body of a marker that has no
/// closing `]` on the same line is also left alone — the channel parser is
/// the only thing that recovers those, and the recovered text is a path the
/// model already saw.
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
}
