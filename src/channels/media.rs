//! Inbound media: the one place the policy lives.
//!
//! Accepting an attachment means downloading attacker-supplied bytes onto the
//! operator's machine and putting them in the agent's context. The rules — size,
//! type, where bytes land, what happens on failure — are written down in
//! `docs/security/inbound-media-policy.md` and implemented here **once**, so a
//! channel added later inherits them instead of inventing its own answers.

use base64::Engine as _;

/// Image types the agent accepts. Anything else is rejected with a note.
const ACCEPTED: &[(&[u8], &str)] = &[
    (b"\x89PNG\r\n\x1a\n", "image/png"),
    (b"\xff\xd8\xff", "image/jpeg"),
    (b"GIF87a", "image/gif"),
    (b"GIF89a", "image/gif"),
];

/// What the fetch produced: either a `data:` URI ready to embed, or a note for
/// the user. There is no third case — a rejection is never silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaOutcome {
    /// `data:<mime>;base64,…`, to be wrapped in an `[IMAGE:…]` marker.
    Image(String),
    /// Human-readable reason, appended to the forwarded content.
    Rejected(String),
}

impl MediaOutcome {
    /// The text this outcome contributes to the message the agent sees.
    #[must_use]
    pub fn to_marker(&self) -> String {
        match self {
            Self::Image(data_uri) => format!("[IMAGE:{data_uri}]"),
            Self::Rejected(note) => format!("[{note}]"),
        }
    }
}

/// The size ceiling, from `[multimodal].max_image_size_mb` (clamped 1–20 MiB by
/// `effective_limits`).
#[must_use]
pub fn max_bytes(multimodal: &crate::config::MultimodalConfig) -> u64 {
    let (_, max_mb) = multimodal.effective_limits();
    max_mb as u64 * 1024 * 1024
}

/// Whether a platform's *claimed* type is worth downloading at all.
///
/// An early filter only — the claim comes from the sender's client and is
/// attacker-influenced, so it never decides acceptance. Its job is to skip the
/// download for an obvious PDF, not to vouch for a JPEG.
#[must_use]
pub fn claimed_type_is_image(claimed: Option<&str>) -> bool {
    claimed.is_none_or(|c| c.trim().to_ascii_lowercase().starts_with("image/"))
}

/// The real type, from the leading bytes. RIFF/WebP needs the container check,
/// which is why this is not a flat prefix table.
#[must_use]
pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    for (magic, mime) in ACCEPTED {
        if bytes.starts_with(magic) {
            return Some(mime);
        }
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Apply the policy to bytes that have already been read.
///
/// Split from the fetch so the rules are testable without a network: the
/// caller's only job is to hand over at most `max_bytes + 1` bytes, and the
/// extra byte is how an oversized body is detected after a bounded read.
#[must_use]
pub fn accept_bytes(bytes: &[u8], claimed: Option<&str>, max_bytes: u64) -> MediaOutcome {
    if bytes.len() as u64 > max_bytes {
        return MediaOutcome::Rejected(format!(
            "Attachment rejected: image too large (over {} MiB limit)",
            max_bytes / (1024 * 1024)
        ));
    }
    if bytes.is_empty() {
        return MediaOutcome::Rejected(
            "Attachment unavailable: media fetch returned no data".into(),
        );
    }

    match sniff_image_mime(bytes) {
        Some(mime) => {
            // The claim is checked against the bytes rather than trusted: a
            // mismatch is a signal, not a formatting quirk.
            if let Some(claimed) = claimed {
                let claimed = claimed.trim().to_ascii_lowercase();
                if !claimed.is_empty() && !claimed.starts_with("image/") {
                    return MediaOutcome::Rejected(format!(
                        "Attachment rejected: type mismatch (sender claimed {claimed}, bytes are {mime})"
                    ));
                }
            }
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            MediaOutcome::Image(format!("data:{mime};base64,{encoded}"))
        }
        None => MediaOutcome::Rejected(
            "Attachment rejected: unsupported type (not a PNG, JPEG, GIF or WebP)".into(),
        ),
    }
}

/// Fetch media and apply the policy.
///
/// The body is read **bounded** at `max_bytes + 1`: a server that streams
/// forever cannot exhaust memory while we wait to learn how big it is, and the
/// extra byte makes "exactly at the limit" distinguishable from "over".
/// `Content-Length` is an early exit only — it is advisory and can lie.
pub async fn fetch_image(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    claimed: Option<&str>,
    max_bytes: u64,
) -> MediaOutcome {
    if !claimed_type_is_image(claimed) {
        return MediaOutcome::Rejected(format!(
            "Attachment rejected: unsupported type ({})",
            claimed.unwrap_or("unknown")
        ));
    }

    let mut request = client.get(url);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let Ok(response) = request.send().await else {
        return MediaOutcome::Rejected("Attachment unavailable: media fetch failed".into());
    };
    if !response.status().is_success() {
        return MediaOutcome::Rejected(format!(
            "Attachment unavailable: media fetch failed (HTTP {})",
            response.status().as_u16()
        ));
    }
    if let Some(len) = response.content_length() {
        if len > max_bytes {
            return MediaOutcome::Rejected(format!(
                "Attachment rejected: image too large ({:.1} MiB, limit {} MiB)",
                len as f64 / (1024.0 * 1024.0),
                max_bytes / (1024 * 1024)
            ));
        }
    }

    let mut collected: Vec<u8> = Vec::new();
    let mut stream = response;
    loop {
        match stream.chunk().await {
            Ok(Some(chunk)) => {
                collected.extend_from_slice(&chunk);
                if collected.len() as u64 > max_bytes {
                    // Stop reading: past this point the answer cannot change.
                    collected.truncate(max_bytes as usize + 1);
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                return MediaOutcome::Rejected(
                    "Attachment unavailable: media fetch failed mid-download".into(),
                )
            }
        }
    }

    accept_bytes(&collected, claimed, max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(padding: usize) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend(std::iter::repeat_n(0u8, padding));
        bytes
    }

    #[test]
    fn oversized_media_is_rejected_with_a_note() {
        let limit = 1024;
        let outcome = accept_bytes(&png(4096), Some("image/png"), limit);
        match outcome {
            MediaOutcome::Rejected(note) => {
                assert!(note.contains("too large"), "note was: {note}");
                assert!(note.starts_with("Attachment rejected"));
            }
            MediaOutcome::Image(_) => panic!("an oversized image must not be accepted"),
        }
        // Control on the same shape: under the cap it IS accepted, so this
        // cannot pass because the fixture was malformed.
        assert!(matches!(
            accept_bytes(&png(16), Some("image/png"), limit),
            MediaOutcome::Image(_)
        ));
    }

    #[test]
    fn unaccepted_mime_is_rejected_with_a_note() {
        // Bytes that are not any accepted image.
        let outcome = accept_bytes(b"%PDF-1.7 not an image", Some("image/png"), 1024);
        let MediaOutcome::Rejected(note) = outcome else {
            panic!("a PDF must not be accepted as an image")
        };
        assert!(note.contains("unsupported type"), "note was: {note}");
    }

    #[test]
    fn a_claimed_type_that_contradicts_the_bytes_is_rejected() {
        // The platform reports what the SENDER's client declared, so a claim
        // that disagrees with the bytes is a signal, not a quirk.
        let outcome = accept_bytes(&png(16), Some("application/pdf"), 1024);
        let MediaOutcome::Rejected(note) = outcome else {
            panic!("a contradicted claim must not be accepted")
        };
        assert!(note.contains("type mismatch"), "note was: {note}");

        // And the claim alone never accepts: PDF bytes claiming to be a PNG
        // are still rejected.
        assert!(matches!(
            accept_bytes(b"%PDF-1.7", Some("image/png"), 1024),
            MediaOutcome::Rejected(_)
        ));
    }

    #[test]
    fn every_accepted_type_is_sniffed_from_its_own_bytes() {
        assert_eq!(
            sniff_image_mime(b"\x89PNG\r\n\x1a\n\x00"),
            Some("image/png")
        );
        assert_eq!(sniff_image_mime(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(sniff_image_mime(b"GIF89a\x00"), Some("image/gif"));
        assert_eq!(
            sniff_image_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(sniff_image_mime(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff_image_mime(b""), None);
    }

    #[test]
    fn a_rejection_reaches_the_content_as_a_note() {
        let marker =
            MediaOutcome::Rejected("Attachment unavailable: media fetch failed".into()).to_marker();
        assert_eq!(marker, "[Attachment unavailable: media fetch failed]");
        // An accepted image becomes the marker the multimodal path parses.
        let marker = MediaOutcome::Image("data:image/png;base64,AAA".into()).to_marker();
        assert_eq!(marker, "[IMAGE:data:image/png;base64,AAA]");
    }

    #[tokio::test]
    async fn fetch_failure_is_reported_not_silent() {
        let client = reqwest::Client::new();
        // Port 1 on loopback: nothing listens, so the request fails fast.
        let outcome = fetch_image(
            &client,
            "http://127.0.0.1:1/media",
            None,
            Some("image/png"),
            1024,
        )
        .await;
        let MediaOutcome::Rejected(note) = outcome else {
            panic!("a failed fetch must not look like an accepted image")
        };
        assert!(note.contains("media fetch failed"), "note was: {note}");
    }

    #[tokio::test]
    async fn an_oversized_body_stops_the_download() {
        use axum::body::Bytes;

        async fn big() -> (axum::http::HeaderMap, Bytes) {
            // No Content-Length: the header is advisory and the bounded read is
            // what actually enforces the cap.
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("content-type", "image/png".parse().expect("header"));
            let mut body = b"\x89PNG\r\n\x1a\n".to_vec();
            body.extend(std::iter::repeat_n(0u8, 8192));
            (headers, Bytes::from(body))
        }

        let app = axum::Router::new().route("/media", axum::routing::get(big));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let outcome = fetch_image(
            &client,
            &format!("http://{addr}/media"),
            None,
            Some("image/png"),
            1024,
        )
        .await;
        assert!(
            matches!(outcome, MediaOutcome::Rejected(ref note) if note.contains("too large")),
            "got: {outcome:?}"
        );

        // Control: the same server under the cap is accepted, so the assertion
        // above cannot pass because the server was broken.
        let outcome = fetch_image(
            &client,
            &format!("http://{addr}/media"),
            None,
            Some("image/png"),
            65536,
        )
        .await;
        assert!(
            matches!(outcome, MediaOutcome::Image(_)),
            "got: {outcome:?}"
        );
    }
}
