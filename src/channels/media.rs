//! Inbound media: the one place the policy lives.
//!
//! Accepting an attachment means downloading attacker-supplied bytes onto the
//! operator's machine and putting them in the agent's context. The rules — size,
//! type, where bytes land, what happens on failure — are written down in
//! `docs/security/inbound-media-policy.md` and implemented here **once**, so a
//! channel added later inherits them instead of inventing its own answers.

use base64::Engine as _;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Images one sender may have accepted per [`BUDGET_WINDOW`].
///
/// Deliberately a constant and not a config key: a key means a schema version
/// bump and a drift snapshot, and there is no operator asking for a different
/// number yet. Raise it here if one does.
pub(crate) const BUDGET_IMAGES: u32 = 20;

/// The window [`BUDGET_IMAGES`] is counted over. Fixed, not sliding — a sender
/// who exhausts it waits out the remainder of the window, which is cheaper to
/// reason about than a rolling count and errs toward the sender's benefit at
/// the boundary.
const BUDGET_WINDOW: Duration = Duration::from_mins(10);

/// Window start and images charged in it, per sender key.
///
/// Process-global on purpose: the limit is per *sender*, and one sender can be
/// talking to several channels at once. Entries whose window has closed are
/// dropped on the next charge, so this holds only senders active in the last
/// [`BUDGET_WINDOW`].
static BUDGET: LazyLock<Mutex<HashMap<String, (Instant, u32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Charge one inbound image to `sender_key`, or refuse with the note the user
/// should see.
///
/// `sender_key` must be channel-qualified — `"discord:<id>"`, `"email:<addr>"`
/// — so one identifier reused on two platforms does not share an allowance.
///
/// Called **before** the download, so an exhausted sender costs no bandwidth.
/// Inbound media is otherwise an unmetered cost lever for anyone the allowlist
/// admits, and on a group channel that is a wider set than the operator
/// pictures.
///
/// # Errors
///
/// Returns the rejection note when the sender has spent the window's budget.
pub fn charge(sender_key: &str) -> Result<(), String> {
    let now = Instant::now();
    let mut budget = match BUDGET.lock() {
        Ok(budget) => budget,
        // A poisoned lock means some other thread panicked mid-charge. Failing
        // open here would hand an attacker an unmetered path by crashing one
        // request, so the budget refuses instead.
        Err(_) => return Err("Attachment rejected: media budget unavailable".into()),
    };

    budget.retain(|_, (started, _)| now.duration_since(*started) < BUDGET_WINDOW);

    let entry = budget.entry(sender_key.to_string()).or_insert((now, 0));
    if entry.1 >= BUDGET_IMAGES {
        let left = BUDGET_WINDOW.saturating_sub(now.duration_since(entry.0));
        return Err(format!(
            "Attachment rejected: media budget spent ({BUDGET_IMAGES} images per {} minutes); \
             try again in {} minute(s)",
            BUDGET_WINDOW.as_secs() / 60,
            left.as_secs().div_ceil(60)
        ));
    }
    entry.1 += 1;
    Ok(())
}

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

/// Accepted bytes with the type the **bytes** say they are, or the note the
/// user should see. Callers that need the raw image (Telegram resizes before
/// embedding) take this; callers that just want a marker take
/// [`accept_bytes`].
#[derive(Debug)]
pub enum ImageBytes {
    Ok { mime: &'static str, bytes: Vec<u8> },
    Rejected(String),
}

/// Apply the policy to bytes that have already been read.
///
/// Split from the fetch so the rules are testable without a network: the
/// caller's only job is to hand over at most `max_bytes + 1` bytes, and the
/// extra byte is how an oversized body is detected after a bounded read.
#[must_use]
pub fn accept_bytes(bytes: &[u8], claimed: Option<&str>, max_bytes: u64) -> MediaOutcome {
    match accept_image_bytes(bytes, claimed, max_bytes) {
        ImageBytes::Ok { mime, bytes } => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            MediaOutcome::Image(format!("data:{mime};base64,{encoded}"))
        }
        ImageBytes::Rejected(note) => MediaOutcome::Rejected(note),
    }
}

/// The policy itself. [`accept_bytes`] is this plus base64.
#[must_use]
pub fn accept_image_bytes(bytes: &[u8], claimed: Option<&str>, max_bytes: u64) -> ImageBytes {
    if bytes.len() as u64 > max_bytes {
        return ImageBytes::Rejected(format!(
            "Attachment rejected: image too large (over {} MiB limit)",
            max_bytes / (1024 * 1024)
        ));
    }
    if bytes.is_empty() {
        return ImageBytes::Rejected("Attachment unavailable: media fetch returned no data".into());
    }

    match sniff_image_mime(bytes) {
        Some(mime) => {
            // The claim is checked against the bytes rather than trusted: a
            // mismatch is a signal, not a formatting quirk.
            if let Some(claimed) = claimed {
                let claimed = claimed.trim().to_ascii_lowercase();
                if !claimed.is_empty() && !claimed.starts_with("image/") {
                    return ImageBytes::Rejected(format!(
                        "Attachment rejected: type mismatch (sender claimed {claimed}, bytes are {mime})"
                    ));
                }
            }
            ImageBytes::Ok {
                mime,
                bytes: bytes.to_vec(),
            }
        }
        None => ImageBytes::Rejected(
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
    sender_key: &str,
) -> MediaOutcome {
    match fetch_image_bytes(client, url, bearer, claimed, max_bytes, sender_key).await {
        ImageBytes::Ok { mime, bytes } => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            MediaOutcome::Image(format!("data:{mime};base64,{encoded}"))
        }
        ImageBytes::Rejected(note) => MediaOutcome::Rejected(note),
    }
}

/// [`fetch_image`] without the base64 step, for callers that transform the
/// image first (Telegram thumbnails it to fit the model's context).
pub async fn fetch_image_bytes(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    claimed: Option<&str>,
    max_bytes: u64,
    sender_key: &str,
) -> ImageBytes {
    if !claimed_type_is_image(claimed) {
        return ImageBytes::Rejected(format!(
            "Attachment rejected: unsupported type ({})",
            claimed.unwrap_or("unknown")
        ));
    }

    // After the type filter, before the request: the budget meters downloads
    // actually performed, and a declared non-image costs none.
    if let Err(note) = charge(sender_key) {
        return ImageBytes::Rejected(note);
    }

    let mut request = client.get(url);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let Ok(response) = request.send().await else {
        return ImageBytes::Rejected("Attachment unavailable: media fetch failed".into());
    };
    if !response.status().is_success() {
        return ImageBytes::Rejected(format!(
            "Attachment unavailable: media fetch failed (HTTP {})",
            response.status().as_u16()
        ));
    }
    if let Some(len) = response.content_length() {
        if len > max_bytes {
            return ImageBytes::Rejected(format!(
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
                // `usize::try_from` rather than `as`: on a 32-bit target a
                // cap above 4 GiB would wrap, and the clamp below keeps the
                // "one byte over" signal `accept_bytes` reads.
                let ceiling = usize::try_from(max_bytes).unwrap_or(usize::MAX);
                if collected.len() > ceiling {
                    // Stop reading: past this point the answer cannot change.
                    collected.truncate(ceiling.saturating_add(1));
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                return ImageBytes::Rejected(
                    "Attachment unavailable: media fetch failed mid-download".into(),
                )
            }
        }
    }

    accept_image_bytes(&collected, claimed, max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget is process-global, so every test that charges it uses a key
    /// of its own. Sharing one would make a result depend on test ordering.
    #[test]
    fn a_sender_is_cut_off_after_spending_the_window_budget() {
        let key = "test:budget_exhaustion";
        for i in 0..BUDGET_IMAGES {
            assert!(charge(key).is_ok(), "image {i} should be within budget");
        }

        let note = charge(key).expect_err("image past the budget must be refused");
        assert!(note.contains("media budget spent"), "{note}");
        // The note tells the user when they can try again, not just that they
        // failed — a rejection the sender cannot act on is barely better than
        // silence.
        assert!(note.contains("try again in"), "{note}");
    }

    #[test]
    fn one_sender_exhausting_the_budget_does_not_block_another() {
        let loud = "test:budget_isolation_loud";
        for _ in 0..BUDGET_IMAGES {
            assert!(charge(loud).is_ok());
        }
        assert!(charge(loud).is_err());

        // The whole point of keying by sender: a group channel's other members
        // keep working while one member is over their allowance.
        assert!(charge("test:budget_isolation_quiet").is_ok());
    }

    /// The keys callers build are channel-qualified, so the same identifier on
    /// two platforms does not share one allowance.
    #[test]
    fn the_same_identifier_on_two_channels_gets_two_allowances() {
        for _ in 0..BUDGET_IMAGES {
            assert!(charge("discord:test_budget_shared_id").is_ok());
        }
        assert!(charge("discord:test_budget_shared_id").is_err());
        assert!(charge("telegram:test_budget_shared_id").is_ok());
    }

    /// The reason the budget lives here and not in the dispatch loop: an
    /// exhausted sender must cost no bandwidth, so the refusal has to land
    /// before the request is sent.
    #[tokio::test]
    async fn an_exhausted_sender_never_reaches_the_server() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static HITS: AtomicUsize = AtomicUsize::new(0);

        async fn counted() -> (axum::http::HeaderMap, axum::body::Bytes) {
            HITS.fetch_add(1, Ordering::SeqCst);
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("content-type", "image/png".parse().expect("header"));
            let mut body = b"\x89PNG\r\n\x1a\n".to_vec();
            body.extend(std::iter::repeat_n(0u8, 32));
            (headers, axum::body::Bytes::from(body))
        }

        let app = axum::Router::new().route("/media", axum::routing::get(counted));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/media");
        let key = "test:budget_no_download";

        // Control first: the same call succeeds and does reach the server, so
        // the assertion below cannot pass because the server was unreachable.
        let outcome = fetch_image(&client, &url, None, Some("image/png"), 65536, key).await;
        assert!(matches!(outcome, MediaOutcome::Image(_)), "{outcome:?}");
        let after_control = HITS.load(Ordering::SeqCst);
        assert_eq!(
            after_control, 1,
            "the control request must reach the server"
        );

        for _ in 1..BUDGET_IMAGES {
            assert!(charge(key).is_ok());
        }

        let outcome = fetch_image(&client, &url, None, Some("image/png"), 65536, key).await;
        assert!(
            matches!(outcome, MediaOutcome::Rejected(ref note) if note.contains("media budget spent")),
            "{outcome:?}"
        );
        assert_eq!(
            HITS.load(Ordering::SeqCst),
            after_control,
            "the refused fetch still hit the network — the budget is being \
             charged after the download instead of before it"
        );
    }

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
            "test:fetch_failure",
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
            "test:oversized",
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
            "test:oversized_control",
        )
        .await;
        assert!(
            matches!(outcome, MediaOutcome::Image(_)),
            "got: {outcome:?}"
        );
    }
}
