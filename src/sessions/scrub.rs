//! Secret scrubbing on the channel-recording write path.
//!
//! Two scrubbers run on every recorded message — user text and reply —
//! before the `INSERT`. The order is fixed, with `scrub_secret_patterns` first
//! (catches the well-known `sk-…`, `ghp_…`, etc. prefixes providers leak in
//! errors) and `scrub_credentials` second (catches the `api_key =
//! "value"` shape a tool might write into memory or a tool result might
//! carry). The order does not matter for non-overlapping matches; both
//! scrubbers are idempotent so a double-run produces the same string.
//!
//! **Known limit.** Neither scrubber catches a secret written as a plain
//! sentence ("here is my token: mysecretvalue123"). Both rely on shape — a
//! prefix list and a `key=value` pattern — and `mysecretvalue123` carries
//! neither. This is the limit the operator-facing copy states plainly; a
//! general natural-language scrubber would be a different and far larger
//! change.

use crate::agent::loop_::scrub_credentials;
use crate::providers::scrub_secret_patterns;

/// Run both scrubbers on `input` and return the joined result. Kept as a single
/// entry point so the channel recording site has exactly one place to call —
/// no chance of one scrubber getting skipped on a future refactor.
pub fn scrub_channel_message(input: &str) -> String {
    let cleaned = scrub_secret_patterns(input);
    scrub_credentials(&cleaned)
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
}
