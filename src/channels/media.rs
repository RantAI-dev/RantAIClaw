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
    apply_budget(sender_key, true)
}

/// Whether [`charge`] would refuse `sender_key`, **without** consuming a slot.
///
/// For a channel whose media path costs a request *before* the download:
/// Telegram resolves a `file_id` through `getFile`, WhatsApp Cloud resolves a
/// media id to a URL, and both are authenticated round trips that an exhausted
/// sender should not be able to make either. `charge` sits inside the fetch, so
/// without this the budget saved those channels the download but not the
/// lookup.
///
/// Peek-then-charge is deliberately not atomic: two attachments racing for the
/// last slot means one wasted lookup, and the fetch still refuses. Making the
/// pre-check consume would double-charge every image on these two channels,
/// which is the worse error.
///
/// # Errors
///
/// Returns the same note [`charge`] would, when the sender has no budget left.
pub fn peek(sender_key: &str) -> Result<(), String> {
    apply_budget(sender_key, false)
}

fn apply_budget(sender_key: &str, consume: bool) -> Result<(), String> {
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
    if consume {
        entry.1 += 1;
    }
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

    // ── Outbound attachment paths (plan 354) ────────────────

    /// A workspace holding one file, and that file's absolute path.
    fn workspace_with_file(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let file = workspace.join(name);
        std::fs::write(&file, b"halo").expect("write file");
        file
    }

    /// The only form that worked before plan 354.
    #[test]
    fn an_absolute_path_inside_the_workspace_resolves() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file = workspace_with_file(dir.path(), "catatan.txt");
        let workspace = dir.path().join("workspace");

        let resolved = resolve_attachment_path("Slack", file.to_str().expect("utf-8"), &workspace)
            .expect("an absolute path inside the workspace is allowed");

        assert_eq!(
            resolved.canonicalize().expect("canonical"),
            file.canonicalize().expect("canonical")
        );
    }

    /// The form Telegram failed on at 06:38:48: a bare file name, which must
    /// resolve against the workspace and not the daemon's working directory.
    #[test]
    fn a_bare_name_resolves_against_the_workspace() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file = workspace_with_file(dir.path(), "catatan.txt");
        let workspace = dir.path().join("workspace");

        let resolved = resolve_attachment_path("Telegram", "catatan.txt", &workspace)
            .expect("a bare name resolves against the workspace");

        assert_eq!(
            resolved.canonicalize().expect("canonical"),
            file.canonicalize().expect("canonical")
        );
    }

    /// The form Slack failed on at 06:45:29. `$HOME` is a tempdir here: the
    /// developer's own home is never read.
    #[tokio::test]
    async fn a_tilde_path_resolves_against_home() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let home = tempfile::TempDir::new().expect("tempdir");
        let _home_guard = crate::test_env::HomeGuard::set(home.path());

        let profile = home.path().join(".rantaiclaw/profiles/default");
        let file = workspace_with_file(&profile, "catatan.txt");
        let workspace = profile.join("workspace");

        let resolved = resolve_attachment_path(
            "Slack",
            "~/.rantaiclaw/profiles/default/workspace/catatan.txt",
            &workspace,
        )
        .expect("a ~ path resolves against HOME");

        assert_eq!(
            resolved.canonicalize().expect("canonical"),
            file.canonicalize().expect("canonical")
        );
    }

    /// Accepting more path forms must not widen what is allowed: a relative path
    /// that climbs out of the workspace is still refused.
    #[test]
    fn a_relative_path_that_climbs_out_is_refused() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        workspace_with_file(dir.path(), "catatan.txt");
        let workspace = dir.path().join("workspace");
        std::fs::write(dir.path().join("config.toml"), b"secret").expect("write outside file");

        let refused = resolve_attachment_path("Discord", "../config.toml", &workspace)
            .expect_err("a path climbing out of the workspace must be refused");

        assert!(
            refused.to_string().contains("outside the workspace"),
            "{refused}"
        );
    }

    #[test]
    fn an_absolute_path_outside_the_workspace_is_refused() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        workspace_with_file(dir.path(), "catatan.txt");
        let workspace = dir.path().join("workspace");
        let secret = dir.path().join("config.toml");
        std::fs::write(&secret, b"secret").expect("write outside file");

        let refused =
            resolve_attachment_path("WhatsApp", secret.to_str().expect("utf-8"), &workspace)
                .expect_err("an absolute path outside the workspace must be refused");

        assert!(
            refused.to_string().contains("outside the workspace"),
            "{refused}"
        );
    }

    /// Plan 356: the instruction has to carry the promises, not just the
    /// syntax. Pinned as promises so the wording can be rewritten freely: the
    /// workspace path is named, no tool and no approval are needed, the file
    /// must exist first, and all five markers are still listed.
    #[test]
    fn the_instruction_tells_the_model_what_actually_works() {
        let text = delivery_instructions_for("Telegram", std::path::Path::new("/ws/rantaiclaw"));
        let lowered = text.to_lowercase();

        assert!(
            text.contains("/ws/rantaiclaw"),
            "the one path form that works is unusable unless the workspace is named: {text}"
        );
        assert!(
            lowered.contains("no tool call") && lowered.contains("no approval"),
            "Telegram refused on 2026-09-12 because nothing said a marker is enough: {text}"
        );
        assert!(
            lowered.contains("absolute"),
            "Slack guessed `~/…` without this: {text}"
        );
        assert!(
            lowered.contains("exist"),
            "a marker for a file not yet written cannot be delivered: {text}"
        );
        for marker in ["[IMAGE:", "[DOCUMENT:", "[VIDEO:", "[AUDIO:", "[VOICE:"] {
            assert!(text.contains(marker), "missing {marker}: {text}");
        }
    }

    /// Plan 356, finding F-13: a Slack reply ended `[DOCUMENT:/abs/path` with no
    /// bracket, and the reader saw the raw marker. When the file is really
    /// there, the person gets the file the model meant.
    #[test]
    fn an_unclosed_marker_naming_a_real_file_still_delivers_it() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file = dir.path().join("catatan.txt");
        std::fs::write(&file, b"isi").expect("write the file");

        let reply = format!("ini berkasnya [DOCUMENT:{}", file.display());
        let (text, attachments) = parse_attachment_markers(&reply);

        assert_eq!(attachments.len(), 1, "{attachments:?}");
        assert_eq!(attachments[0].kind, AttachmentKind::Document);
        assert_eq!(attachments[0].target, file.display().to_string());
        assert!(
            !text.contains("[DOCUMENT:"),
            "the raw marker must not reach the reader: {text}"
        );
    }

    /// The other half: nothing is lost when the fragment names no file. The text
    /// stays exactly as it was, and plan 355's notice explains the rest.
    #[test]
    fn an_unclosed_marker_naming_nothing_stays_in_the_text() {
        let (text, attachments) =
            parse_attachment_markers("ini berkasnya [DOCUMENT:bukan berkas apa pun");

        assert!(attachments.is_empty(), "{attachments:?}");
        assert!(
            text.contains("[DOCUMENT:bukan berkas apa pun"),
            "the text must survive untouched: {text}"
        );
    }

    /// A `]` on a later line belongs to a later thought. Before the line scope
    /// it closed this marker and swallowed everything in between.
    #[test]
    fn a_bracket_on_a_later_line_does_not_close_an_unclosed_marker() {
        let (text, attachments) =
            parse_attachment_markers("lihat [DOCUMENT:/tidak/ada\nlalu [catatan] berikutnya");

        assert!(attachments.is_empty(), "{attachments:?}");
        assert!(
            text.contains("[DOCUMENT:/tidak/ada") && text.contains("[catatan]"),
            "both lines must survive: {text}"
        );
    }

    /// Line scoping must not cost a well-formed marker that follows other text
    /// on the same line, which is where markers normally sit.
    #[test]
    fn a_closed_marker_after_trailing_text_on_the_same_line_is_still_parsed() {
        let (text, attachments) = parse_attachment_markers(
            "catatan [lihat lampiran] ini dia [IMAGE:/w/chart.png] selesai",
        );

        assert_eq!(attachments.len(), 1, "{attachments:?}");
        assert_eq!(attachments[0].target, "/w/chart.png");
        assert!(
            text.contains("[lihat lampiran]") && !text.contains("[IMAGE:"),
            "the non-marker stays and the marker goes: {text}"
        );
    }

    /// Collect the WARN lines emitted while `run` executes.
    ///
    /// The first log capture in this crate, and it earns its keep: the whole
    /// point of plan 356's second half is that a broken marker leaves a trace,
    /// and a trace is only observable through a subscriber. Thread-local, so
    /// parallel tests cannot see each other's events.
    fn warnings_from(run: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("lock the log buffer")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl tracing_subscriber::fmt::MakeWriter<'_> for Buffer {
            type Writer = Self;

            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }

        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = buffer.0.lock().expect("lock the log buffer").clone();
        String::from_utf8(bytes).expect("log output is utf-8")
    }

    /// Finding F-13: the 2026-09-11 Slack reply was invisible because nothing
    /// was logged at any level. A marker that cannot be recovered must still be
    /// reported, naming the kind and the target so the path can be checked.
    #[test]
    fn an_unclosed_marker_that_cannot_be_delivered_is_still_reported() {
        let logged = warnings_from(|| {
            let (_text, attachments) =
                parse_attachment_markers("ini berkasnya [DOCUMENT:/tidak/ada/catatan.txt");
            assert!(attachments.is_empty(), "{attachments:?}");
        });

        assert!(
            logged.contains("unclosed attachment marker"),
            "silence is what made this invisible: {logged:?}"
        );
        assert!(
            logged.contains("/tidak/ada/catatan.txt") && logged.contains("Document"),
            "the report has to name the kind and the target: {logged:?}"
        );
    }

    #[test]
    fn a_missing_file_is_refused_naming_what_the_model_wrote() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace");

        let refused = resolve_attachment_path("Telegram", "test123.txt", &workspace)
            .expect_err("a missing file must be refused");

        assert!(
            refused
                .to_string()
                .contains("attachment path not found: test123.txt"),
            "{refused}"
        );
    }
}

// ── Outbound attachments ────────────────────────────────────────────────────
//
// The marker vocabulary the model emits when it wants a file delivered, and the
// rules for turning one into something safe to upload. Lifted out of
// `telegram.rs` on 2026-09-10, when Discord became the second channel to need
// it: the parsing and the workspace confinement were never Telegram-specific,
// and a second copy of the confinement check is the last thing this should grow.

/// What kind of attachment a marker asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    Document,
    Video,
    Audio,
    Voice,
}

impl AttachmentKind {
    /// Parse the marker tag. Aliases are accepted because the model writes what
    /// it was told plus what it guesses.
    #[must_use]
    pub fn from_marker(marker: &str) -> Option<Self> {
        match marker.trim().to_ascii_uppercase().as_str() {
            "IMAGE" | "PHOTO" => Some(Self::Image),
            "DOCUMENT" | "FILE" => Some(Self::Document),
            "VIDEO" => Some(Self::Video),
            "AUDIO" => Some(Self::Audio),
            "VOICE" => Some(Self::Voice),
            _ => None,
        }
    }
}

/// One attachment the model asked for: a kind and a path or URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundAttachment {
    pub kind: AttachmentKind,
    pub target: String,
}

/// Is this target a remote URL rather than a local path?
#[must_use]
pub fn is_http_url(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

/// Split a reply into the text a human reads and the attachments to upload.
///
/// A bracketed run that is not a valid marker is left in the text verbatim, so
/// a model writing `[see attached]` does not lose it.
///
/// An unclosed marker is not left silent. On 2026-09-11 a Slack reply ended
/// `[DOCUMENT:/abs/path` with no `]`: the reader saw the raw marker, `send`
/// returned `Ok`, and no line was logged at any level. Now the closing bracket
/// is looked for on the marker's own line only, and a known kind that opens
/// without one is reported and, where the fragment names a file that exists,
/// delivered anyway. See [`recover_unclosed_marker`].
#[must_use]
pub fn parse_attachment_markers(message: &str) -> (String, Vec<OutboundAttachment>) {
    let mut cleaned = String::with_capacity(message.len());
    let mut attachments = Vec::new();
    let mut cursor = 0;

    while cursor < message.len() {
        let Some(open_rel) = message[cursor..].find('[') else {
            cleaned.push_str(&message[cursor..]);
            break;
        };

        let open = cursor + open_rel;
        cleaned.push_str(&message[cursor..open]);

        // The closing bracket has to be on this marker's line. A `]` further
        // down the reply closes a different thought, and accepting it as this
        // marker's swallows every line in between.
        let line_end = message[open..]
            .find('\n')
            .map_or(message.len(), |idx| open + idx);
        let Some(close_rel) = message[open..line_end].find(']') else {
            let fragment = &message[open..line_end];
            match recover_unclosed_marker(fragment) {
                Some(attachment) => attachments.push(attachment),
                None => cleaned.push_str(fragment),
            }
            // The newline itself is still ahead of the cursor, so the next pass
            // copies it and the reply keeps its shape.
            cursor = line_end;
            continue;
        };

        let close = open + close_rel;
        let marker = &message[open + 1..close];

        let parsed = marker.split_once(':').and_then(|(kind, target)| {
            let kind = AttachmentKind::from_marker(kind)?;
            let target = target.trim();
            if target.is_empty() {
                return None;
            }
            Some(OutboundAttachment {
                kind,
                target: target.to_string(),
            })
        });

        if let Some(attachment) = parsed {
            attachments.push(attachment);
        } else {
            cleaned.push_str(&message[open..=close]);
        }

        cursor = close + 1;
    }

    (cleaned.trim().to_string(), attachments)
}

/// What to do with `[DOCUMENT:/abs/path` when no `]` follows on its line.
///
/// Returns the attachment when the fragment names a file that is already there,
/// so the person gets the file the model meant instead of a raw marker. Either
/// way a WARN names the kind and the target: the path is not message text, and
/// silence is what made the 2026-09-11 case invisible.
///
/// Confinement is not decided here. The send path calls
/// [`resolve_attachment_path_in_workspace`], which fails closed, so this can
/// only ever propose a candidate — keeping the workspace boundary in one place
/// rather than copying it into the parser. A URL is left alone: only a path can
/// resolve.
fn recover_unclosed_marker(fragment: &str) -> Option<OutboundAttachment> {
    let (kind, target) = fragment.trim_start_matches('[').split_once(':')?;
    let kind = AttachmentKind::from_marker(kind)?;
    let target = target.trim();
    let resolvable = !target.is_empty()
        && !is_http_url(target)
        && std::path::Path::new(target).is_absolute()
        && std::path::Path::new(target).exists();
    tracing::warn!(
        "unclosed attachment marker in a reply: kind={kind:?}, target={target}, \
         delivered_anyway={resolvable}"
    );
    resolvable.then(|| OutboundAttachment {
        kind,
        target: target.to_string(),
    })
}

/// Is this local path inside the workspace?
///
/// A reply is influenced by whoever is chatting, and a prompt injection that
/// names `~/.rantaiclaw/config.toml` would otherwise exfiltrate provider keys
/// and bot tokens straight into the chat. Canonicalised on both sides so
/// `../` cannot walk out, and **fails closed** when the path cannot be
/// resolved. Mirrors the `file_*` tool sandbox.
#[must_use]
pub fn path_within_workspace(target: &std::path::Path, workspace: &std::path::Path) -> bool {
    let Ok(canonical_target) = target.canonicalize() else {
        return false;
    };
    let workspace_root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    canonical_target.starts_with(&workspace_root)
}

/// Where a marker's target points on this machine, before it is checked.
///
/// A leading `~/` expands against `$HOME`, a relative path joins the workspace
/// root, and an absolute path is taken as written. `~user` is **not** expanded:
/// it stays literal and is then refused by the checks in
/// [`resolve_attachment_path`], which is the safe direction.
fn expand_attachment_path(target: &str, workspace: &std::path::Path) -> std::path::PathBuf {
    if let Some(rest) = target.strip_prefix("~/") {
        return crate::profile::paths::home_dir().join(rest);
    }
    let path = std::path::Path::new(target);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    workspace.join(path)
}

/// Turn a marker's target into a local file to upload, or refuse it.
///
/// The model writes whichever path form it likes, and one afternoon produced all
/// three: a bare name, a `~/` path, and the absolute workspace path. Only the
/// last worked, because every channel checked the string as written and a
/// relative path resolves against the daemon's working directory rather than the
/// workspace where `file_write` puts everything.
///
/// The file must then exist and [`path_within_workspace`] must pass. That check
/// canonicalises and fails closed, and it is the reason a prompt injection
/// naming `config.toml` cannot post provider keys into a chat. Expanding `~` or
/// joining the workspace changes **which** file is named, never whether it is
/// allowed. Errors name the target the model wrote, so the journal still shows
/// the model's own mistake.
///
/// # Errors
///
/// When the resolved file does not exist, or lies outside `workspace`.
pub fn resolve_attachment_path(
    channel: &str,
    target: &str,
    workspace: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let resolved = expand_attachment_path(target, workspace);
    if !resolved.exists() {
        anyhow::bail!("{channel} attachment path not found: {target}");
    }
    if !path_within_workspace(&resolved, workspace) {
        anyhow::bail!(
            "{channel} attachment path is outside the workspace and was blocked: {target}"
        );
    }
    Ok(resolved)
}

/// [`resolve_attachment_path`] against the active workspace.
///
/// Every channel's `send_attachment` calls this, so the path forms, the
/// existence check and the confinement check live in one place. Four copies is
/// how they drifted: all four accepted only the absolute form.
///
/// # Errors
///
/// When the active workspace cannot be resolved, or the target is refused.
pub async fn resolve_attachment_path_in_workspace(
    channel: &str,
    target: &str,
) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context as _;
    let (_config_path, workspace_dir) = crate::config::Config::resolve_active_paths()
        .await
        .context("cannot resolve workspace to validate attachment path")?;
    resolve_attachment_path(channel, target, &workspace_dir)
}

/// What the model is told about attaching files, phrased for one platform.
///
/// Kept as a builder rather than a per-channel constant so the vocabulary
/// cannot drift into per-channel dialects — the model has to be told the same
/// five markers everywhere, or a reply written for one channel leaks literal
/// text on another.
///
/// Syntax alone was not enough. On 2026-09-12 the same request produced a file
/// on WhatsApp and, on Telegram, "bot ini saat ini tidak mendukung pengiriman
/// file sebagai lampiran" with no marker at all: the model disbelieved a
/// capability it had been given, because nothing told it that a marker needs no
/// tool and no approval. Slack, told the same thing, guessed `~/…` and then an
/// absolute path. So the text states the promises the runtime actually keeps,
/// and `workspace` is a parameter because the one path form that works cannot
/// be named without it.
#[must_use]
pub fn delivery_instructions_for(platform: &str, workspace: &std::path::Path) -> String {
    let workspace = workspace.display();
    format!(
        "When responding on {platform}, you can attach a file yourself: put a media marker in \
         your reply and the runtime uploads the file for you. This needs no tool call and no \
         approval. Use one marker per attachment, with this exact syntax: \
         [IMAGE:<path-or-url>], [DOCUMENT:<path-or-url>], [VIDEO:<path-or-url>], \
         [AUDIO:<path-or-url>], or [VOICE:<path-or-url>].\n\n\
         A local path must be absolute and inside this workspace: {workspace}. A path outside \
         the workspace is refused and the file is not sent, so do not guess a home-relative or \
         bare name.\n\n\
         The file has to exist before the marker is sent. Write it first, then attach it.\n\n\
         Keep normal user-facing text outside markers, put the markers at the end of the reply, \
         and never wrap a marker in code fences."
    )
}
