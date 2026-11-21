//! Bridge the [`log`] facade into [`tracing`] and own the project's default
//! [`EnvFilter`] directives.
//!
//! wa-rs and other third-party crates emit through [`log`] (for example
//! `wa-rs-0.2.0/src/send.rs:302` `log::warn!("Failed to resolve devices ...")`),
//! while RantaiClaw's own subscribers are configured against [`tracing`].
//! Without a bridge every `log` record is silently dropped: the journal has
//! zero `wa_rs` lines even though the library logs warnings on every group
//! send-key retry. [`install_log_bridge`] registers a shim that forwards
//! each `log::Record` into a `tracing::Event`, so the same filter pipeline
//! handles both facades and the operator-visible record lands in the journal
//! with its original target (e.g. `wa_rs`) intact — `EnvFilter`'s
//! target-prefix match sees the bridge's per-record target and the same
//! directive syntax works for `log`-facade records as for `tracing`-native
//! events.
//!
//! An explicit `RUST_LOG` still wins entirely; when unset,
//! [`default_env_filter`] falls back to [`DEFAULT_DIRECTIVES`], which keeps
//! the noisy HTTP/TLS crates at `warn` to prevent flooding. The wa-rs family
//! (`wa_rs`, `wa_rs_libsignal`, and wa-rs's explicit `Client/*` targets) sits
//! at `warn` because wa-rs's per-message `INFO` lines carry full identifiers
//! (LIDs, group JIDs) — those names do not belong in a routine journal. The
//! `WARN` lines (`Failed to resolve devices`, `No sender key for group`,
//! `Failed to establish session`) are the ones worth keeping. To diagnose a
//! WhatsApp Web delivery problem, restart the daemon with
//! `RUST_LOG=info,wa_rs=debug,Client=debug` for a diagnosis session and
//! expect full identifiers to appear in the journal; `EnvFilter` matches
//! targets by plain string prefix, so `wa_rs=debug` also covers the
//! `wa_rs_libsignal` target and `Client=debug` covers every `Client/*` target.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tracing::callsite::{Callsite, Identifier};
use tracing::field::FieldSet;
use tracing::metadata::{Kind, Level, Metadata};
use tracing::subscriber::Interest;

/// Build an [`Identifier`] for a static `Callsite`. Same as
/// `tracing_core::identify_callsite!`, but inlined because that macro lives in
/// `tracing_core` which is not a direct dependency.
const fn anchor_id(callsite: &'static dyn Callsite) -> Identifier {
    Identifier(callsite)
}

/// Default [`EnvFilter`] directives applied when `RUST_LOG` is unset.
///
/// The base level is `info`. `hyper`, `reqwest`, `rustls`, and `h2` are
/// pinned to `warn` because they are noisy during normal operation
/// (request/response framing, TLS handshakes, h2 control frames). `wa_rs=warn`
/// also covers `wa_rs_libsignal` and every `wa_rs::*` target via plain string
/// prefix matching, and `Client=warn` covers wa-rs's explicit `Client/*`
/// targets (`Client/TcToken`, `Client/Receipt`, `Client/Send`, …). These two
/// pins keep wa-rs's per-message `INFO` lines (which carry full LIDs and
/// group JIDs) out of the routine journal while leaving the diagnosis-relevant
/// `WARN` lines (`Failed to resolve devices`, `No sender key for group`,
/// `Failed to establish session`) visible. An explicit `RUST_LOG` overrides
/// this entirely — for example
/// `RUST_LOG=info,wa_rs=debug,Client=debug`
/// is the diagnosis-session value; prefix matching means `wa_rs=debug`
/// covers `wa_rs_libsignal` and `Client=debug` covers every `Client/*` target.
pub const DEFAULT_DIRECTIVES: &str =
    "info,hyper=warn,reqwest=warn,rustls=warn,h2=warn,wa_rs=warn,Client=warn";

/// Build the default [`EnvFilter`], honouring `RUST_LOG` if the operator set
/// it.
#[must_use]
pub fn default_env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_DIRECTIVES))
}

// ===== Callsite machinery =================================================
//
// `tracing::Event::new` requires a `&'static Metadata<'static>`, which means
// the callsite has to live for the life of the process. We build one
// `Metadata` per distinct `(target, level)` pair we see (e.g. `wa_rs` at
// WARN, `hyper` at DEBUG, …). All per-`Metadata` `FieldSet`s share the
// same anchor `Identifier` so the dispatcher's interest cache collapses
// them into a single entry; per-target filtering is enforced by the
// explicit `dispatch.enabled(meta)` check inside `LogBridge::log` and
// `LogBridge::enabled`, because the macro path's interest-cache short-circuit
// would otherwise accept every record (the anchor's static `log` INFO target
// is `Interest::always()` for the base `info` directive).

/// Anchor `Callsite` shared by every per-(target, level) `Metadata`. Its
/// `metadata()` returns a static `Metadata` with target `"log"` and a
/// placeholder level of `INFO`; the placeholder is never read at runtime
/// because per-(target, level) `Metadata` values carry the real target and
/// level, and the dispatcher is queried per event using that per-target
/// `Metadata`.
struct Anchor;

static ANCHOR: Anchor = Anchor;
static ANCHOR_META: Metadata<'static> = Metadata::new(
    "log",
    "log",
    Level::INFO,
    None,
    None,
    None,
    FieldSet::new(&["message"], anchor_id(&ANCHOR)),
    Kind::EVENT,
);

impl Callsite for Anchor {
    fn set_interest(&self, _: Interest) {}
    fn metadata(&self) -> &Metadata<'static> {
        &ANCHOR_META
    }
}

/// Per-(target, level) metadata cache. Lookup is by `(String, Level)`; on
/// miss we leak a `Metadata` and a `&'static str` into the heap so the
/// returned reference is `'static`. Leaks are bounded by the number of
/// distinct (target, level) pairs seen during the process lifetime — at
/// most a few dozen in practice (wa-rs, hyper, reqwest, rustls, h2 across
/// warn/info/debug).
static META_CACHE: OnceLock<Mutex<HashMap<(String, Level), &'static Metadata<'static>>>> =
    OnceLock::new();

fn metadata_for(target: &str, level: Level) -> &'static Metadata<'static> {
    let cache = META_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().expect("callsite metadata cache poisoned");
    let key = (target.to_owned(), level);
    if let Some(&meta) = cache.get(&key) {
        return meta;
    }
    // Leak the target string so the Metadata's `target: &'static str` field
    // satisfies the `'static` bound on `Event::new`.
    let target_static: &'static str = Box::leak(target.to_owned().into_boxed_str());
    let meta: &'static Metadata<'static> = Box::leak(Box::new(Metadata::new(
        "log event",
        target_static,
        level,
        None,
        None,
        None,
        // Reuse the anchor's `Identifier` so per-(target, level) `Metadata`s
        // share one callsite for interest caching; the per-target `target()`
        // and per-event `level()` are still consulted by `EnvFilter` at
        // `enabled()` time.
        FieldSet::new(&["message"], anchor_id(&ANCHOR)),
        Kind::EVENT,
    )));
    cache.insert(key, meta);
    meta
}

// ===== Bridge =============================================================

/// Map a [`log::Level`] to its [`tracing::Level`] equivalent. Extracted so
/// `LogBridge::enabled` and `LogBridge::log` share the same mapping without
/// duplicating the match.
fn tracing_level_for(level: log::Level) -> Level {
    match level {
        log::Level::Error => Level::ERROR,
        log::Level::Warn => Level::WARN,
        log::Level::Info => Level::INFO,
        log::Level::Debug => Level::DEBUG,
        log::Level::Trace => Level::TRACE,
    }
}

/// Forward `log::Log` records to `tracing::Event`. Stateless; trivially
/// `Sync`/`Send` because every method takes `&self`.
struct LogBridge;

impl log::Log for LogBridge {
    /// Ask the active `tracing` subscriber whether it would accept a record
    /// at `(metadata.target(), level)`. This mirrors the check inside
    /// [`LogBridge::log`], so callers of the `log::log_enabled!` macro — which
    /// consults `Log::enabled` before formatting a record — get an answer
    /// consistent with what `log()` would forward, and nobody formats a record
    /// the bridge would then drop.
    /// The leak set is bounded by the number of distinct `(target, level)`
    /// pairs the process ever sees (a few dozen in practice).
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        let level = tracing_level_for(metadata.level());
        let meta = metadata_for(metadata.target(), level);
        tracing::dispatcher::get_default(|dispatch| dispatch.enabled(meta))
    }

    fn log(&self, record: &log::Record<'_>) {
        let level = tracing_level_for(record.level());
        let meta = metadata_for(record.target(), level);
        tracing::dispatcher::get_default(|dispatch| {
            // Enforce per-(target, level) filtering against the actual
            // subscriber. Without this explicit check, `dispatch.event()`
            // would honour the anchor callsite's cached `Interest::always()`
            // (set when the subscriber first registered the "log" INFO
            // anchor), letting `mio::poll`/`notify::inotify` TRACE and any
            // other below-directive `log` records flood the journal even
            // when the base filter is `info`. `dispatch.enabled(meta)` is a
            // direct `Subscriber::enabled` query — not interest-cached —
            // so `EnvFilter` evaluates the dynamic `(target, level)` pair
            // against its directive set on every record.
            if !dispatch.enabled(meta) {
                return;
            }
            let message_field = meta
                .fields()
                .field("message")
                .expect("message field missing from bridge metadata");
            let values: [(&tracing::field::Field, Option<&dyn tracing::field::Value>); 1] = [(
                &message_field,
                Some(record.args() as &dyn tracing::field::Value),
            )];
            let value_set = meta.fields().value_set(&values);
            let event = tracing::Event::new(meta, &value_set);
            dispatch.event(&event);
        });
    }

    fn flush(&self) {}
}

/// Register the global `log::Log` shim.
///
/// Idempotent: `log::set_logger` is a one-shot process-global; if another
/// init already installed a logger, the [`log::SetLoggerError`] is silently
/// ignored. Sets `log::set_max_level(Trace)` so per-target filtering is owned
/// by the `tracing` subscriber (typically [`default_env_filter`]).
///
/// Call this once at each subscriber-init site (the CLI's `main()` and the
/// TUI's `install_tui_tracing`) before/after building the tracing subscriber
/// — the order does not matter because `log` records emitted before any
/// tracing subscriber exists are dropped either way.
pub fn install_log_bridge() {
    // One-shot process-global; ignore `AlreadySet`.
    let _ = log::set_logger(&LogBridge);
    log::set_max_level(log::LevelFilter::Trace);
}

/// Matches one URL: a scheme, `://`, then the rest of the token. The token ends
/// at whitespace, a double quote, a backtick, an angle bracket, or a square
/// bracket or curly brace. A square-bracket pair that holds an IPv6 literal
/// (`[::1]`) stays in the token, so the port and the query after it are
/// matched. Single quotes and parentheses stay in the token, because RFC 3986
/// allows them in userinfo, so a password that holds one is redacted whole. A
/// URL inside a sentence keeps the text after it.
fn url_regex() -> &'static regex::bytes::Regex {
    static URL: OnceLock<regex::bytes::Regex> = OnceLock::new();
    URL.get_or_init(|| {
        regex::bytes::Regex::new(
            r#"(?P<scheme>[A-Za-z][A-Za-z0-9+.\-]*://)(?P<rest>(?:\[[0-9A-Fa-f:.]*\]|[^\s"`<>\[\]{}])*)"#,
        )
        .expect("URL pattern is a valid regex")
    })
}

/// Rebuilds one URL without its userinfo (`user:pass@`) and without its query
/// string or fragment (everything from the first `?` or `#`). The scheme and
/// the host and path stay, so an operator still sees which endpoint failed.
fn redact_url(scheme: &[u8], rest: &[u8]) -> Vec<u8> {
    let without_query = match rest.iter().position(|&b| b == b'?' || b == b'#') {
        Some(end) => &rest[..end],
        None => rest,
    };
    let (authority, path) = match without_query.iter().position(|&b| b == b'/') {
        Some(slash) => without_query.split_at(slash),
        None => (without_query, &without_query[without_query.len()..]),
    };
    // Userinfo ends at the last `@` of the authority. A host never holds `@`.
    let host = match authority.iter().rposition(|&b| b == b'@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    [scheme, host, path].concat()
}

/// Redacts every URL in `bytes` and returns the bytes otherwise unchanged.
/// Text without a URL is borrowed back as it came in, so a line with no URL
/// reaches the output byte for byte.
pub fn redact_urls_in_bytes(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let url = url_regex();
    if !url.is_match(bytes) {
        return std::borrow::Cow::Borrowed(bytes);
    }
    std::borrow::Cow::Owned(
        url.replace_all(bytes, |caps: &regex::bytes::Captures<'_>| {
            redact_url(&caps["scheme"], &caps["rest"])
        })
        .into_owned(),
    )
}

/// [`redact_urls_in_bytes`] for a `String`, for free text that a response body
/// carries. Redaction cuts only at ASCII bytes, so the result stays UTF-8.
pub fn redact_urls(text: &str) -> String {
    String::from_utf8_lossy(&redact_urls_in_bytes(text.as_bytes())).into_owned()
}

/// Writer that redacts URLs in every line it passes on.
///
/// The fmt layer may call `write` several times for one event. The writer
/// holds the bytes until it sees a newline, so a URL split across calls is
/// redacted whole. Whatever is left when the writer is flushed or dropped
/// goes out redacted as well.
pub struct RedactingWriter<W: std::io::Write> {
    inner: W,
    pending: Vec<u8>,
}

impl<W: std::io::Write> RedactingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            pending: Vec::new(),
        }
    }
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            self.inner.write_all(&redact_urls_in_bytes(&line))?;
        }
        Ok(buf.len())
    }

    /// Forwards the flush to the inner writer only. The partial line stays
    /// buffered until its newline arrives, so a flush in the middle of a URL
    /// cannot send the first half of it out.
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W: std::io::Write> RedactingWriter<W> {
    /// Sends the unterminated remainder, if any, through the redactor.
    fn write_pending(&mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            self.inner.write_all(&redact_urls_in_bytes(&rest))?;
        }
        Ok(())
    }
}

impl<W: std::io::Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        // Nothing can report a failed write made during drop; the event is lost either way.
        let _ = self.write_pending();
        let _ = self.inner.flush();
    }
}

/// `MakeWriter` that wraps every writer of `inner` in a [`RedactingWriter`].
/// Pass it to `with_writer` on a `tracing_subscriber` fmt builder.
pub struct RedactingMakeWriter<M> {
    inner: M,
}

impl<M> RedactingMakeWriter<M> {
    pub fn new(inner: M) -> Self {
        Self { inner }
    }
}

impl<'a, M> tracing_subscriber::fmt::MakeWriter<'a> for RedactingMakeWriter<M>
where
    M: tracing_subscriber::fmt::MakeWriter<'a>,
{
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter::new(self.inner.make_writer())
    }
}

/// Writer selection for [`build_subscriber`]. Both of its arms pass their
/// writer through here, so every line reaches the output redacted.
fn redacting_writer<M>(inner: M) -> impl for<'a> tracing_subscriber::fmt::MakeWriter<'a>
where
    M: for<'a> tracing_subscriber::fmt::MakeWriter<'a>,
{
    RedactingMakeWriter::new(inner)
}

/// The fmt subscriber over one writer, with the redaction applied once here.
/// The two arms of [`build_subscriber`] differ only in the writer and in ANSI.
fn subscriber_over<M>(
    env_filter: tracing_subscriber::EnvFilter,
    writer: M,
    ansi: bool,
) -> Box<dyn tracing::Subscriber + Send + Sync>
where
    M: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    Box::new(
        tracing_subscriber::fmt::Subscriber::builder()
            .with_env_filter(env_filter)
            .with_writer(redacting_writer(writer))
            .with_ansi(ansi)
            .finish(),
    )
}

/// Build the process-wide log subscriber. With a log file, lines go to the
/// file without ANSI colour codes. Without one, lines go to stderr, so stdout
/// stays clean for piped output. Both writers pass every line through
/// [`redacting_writer`], so no URL userinfo or query string reaches either.
#[must_use]
pub fn build_subscriber(
    env_filter: tracing_subscriber::EnvFilter,
    log_file: Option<std::fs::File>,
) -> Box<dyn tracing::Subscriber + Send + Sync> {
    match log_file {
        Some(file) => subscriber_over(env_filter, std::sync::Mutex::new(file), false),
        // The `ansi` feature of tracing-subscriber is on in Cargo.toml, so the
        // builder's default for stderr is ANSI on. Stated here so it is explicit.
        None => subscriber_over(env_filter, std::io::stderr, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};

    /// Pin the directive string. A refactor that loses the noisy-crate pins
    /// or the `wa_rs=warn` / `Client=warn` lines would otherwise flood the
    /// journal with per-message WhatsApp identifiers (LIDs, group JIDs); this
    /// test catches it here, not in a reviewer reading a giant diff.
    #[test]
    fn default_directives_contain_required_pins() {
        assert!(
            DEFAULT_DIRECTIVES.contains("info"),
            "base info level missing: {DEFAULT_DIRECTIVES}"
        );
        assert!(
            DEFAULT_DIRECTIVES.contains("hyper=warn"),
            "hyper=warn pin missing: {DEFAULT_DIRECTIVES}"
        );
        assert!(
            DEFAULT_DIRECTIVES.contains("reqwest=warn"),
            "reqwest=warn pin missing: {DEFAULT_DIRECTIVES}"
        );
        assert!(
            DEFAULT_DIRECTIVES.contains("rustls=warn"),
            "rustls=warn pin missing: {DEFAULT_DIRECTIVES}"
        );
        assert!(
            DEFAULT_DIRECTIVES.contains("h2=warn"),
            "h2=warn pin missing: {DEFAULT_DIRECTIVES}"
        );
        assert!(
            DEFAULT_DIRECTIVES.contains("wa_rs=warn"),
            "wa_rs=warn pin missing: {DEFAULT_DIRECTIVES}"
        );
        assert!(
            DEFAULT_DIRECTIVES.contains("Client=warn"),
            "Client=warn pin missing: {DEFAULT_DIRECTIVES}"
        );
    }

    /// The same `Buffer` pattern `src/channels/mod_tests.rs` and
    /// `src/channels/media.rs` use for their capture tests. In-tree copy
    /// avoids making a test-only helper visible outside this module.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("buffer lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for Buffer {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    fn captured_text(buffer: &Buffer) -> String {
        String::from_utf8(buffer.0.lock().expect("buffer lock").clone()).expect("utf-8 output")
    }

    /// An event carries an endpoint with userinfo and a query, in a field and
    /// in the message, and goes through the production writer type. The host
    /// stays, the userinfo and the query do not. Two control lines go through
    /// the same writer: one plain URL and one line with no URL.
    #[test]
    fn redacting_writer_strips_userinfo_and_query_from_event_lines() {
        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(RedactingMakeWriter::new(buffer.clone()))
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let endpoint = "custom:https://user:secret@host/v1?key=abc";
            tracing::warn!(provider = endpoint, "refresh failed for {endpoint}");
            tracing::info!("models at https://example.com/v1/models");
            tracing::info!("no url on this line");
        });

        let captured = captured_text(&buffer);
        assert!(
            !captured.contains("secret"),
            "userinfo reached the log: {captured}"
        );
        assert!(
            !captured.contains("key=abc"),
            "query reached the log: {captured}"
        );
        assert!(captured.contains("host/v1"), "host was removed: {captured}");
        assert!(
            captured.contains("https://example.com/v1/models"),
            "a plain URL was altered: {captured}"
        );
        assert!(
            captured.contains("no url on this line"),
            "a line without a URL was altered: {captured}"
        );
    }

    /// Lines without a URL, and a plain URL, pass through the writer unchanged.
    #[test]
    fn redacting_writer_passes_lines_without_userinfo_or_query_through_unchanged() {
        use tracing_subscriber::fmt::MakeWriter as _;

        let buffer = Buffer::default();
        let mut writer = RedactingMakeWriter::new(buffer.clone()).make_writer();
        io::Write::write_all(&mut writer, b"plain line, https://example.com/v1/models\n")
            .expect("write");
        io::Write::write_all(&mut writer, b"no url at all\r\n").expect("write");
        io::Write::flush(&mut writer).expect("flush");

        assert_eq!(
            captured_text(&buffer),
            "plain line, https://example.com/v1/models\nno url at all\r\n"
        );
    }

    /// A URL split over two write calls, with the split inside the userinfo and
    /// the query, is redacted as one URL. No part of the secret gets out.
    #[test]
    fn redacting_writer_redacts_a_url_split_across_write_calls() {
        let buffer = Buffer::default();
        let mut writer = RedactingWriter::new(buffer.clone());
        io::Write::write_all(&mut writer, b"endpoint custom:https://user:se").expect("write");
        io::Write::write_all(&mut writer, b"cret@host/v1?key=").expect("write");
        io::Write::write_all(&mut writer, b"abc\n").expect("write");
        io::Write::flush(&mut writer).expect("flush");

        let captured = captured_text(&buffer);
        assert!(!captured.contains("secret"), "userinfo escaped: {captured}");
        assert!(!captured.contains("key=abc"), "query escaped: {captured}");
        assert_eq!(captured, "endpoint custom:https://host/v1\n");
    }

    /// A last line with no newline is held until the writer is dropped, and then
    /// goes out redacted.
    #[test]
    fn redacting_writer_redacts_an_unterminated_line_on_drop() {
        let buffer = Buffer::default();
        {
            let mut writer = RedactingWriter::new(buffer.clone());
            io::Write::write_all(&mut writer, b"open https://user:secret@host/v1?key=abc")
                .expect("write");
        }

        assert_eq!(captured_text(&buffer), "open https://host/v1");
    }

    /// Userinfo may hold `'`, `(` and `)`, which RFC 3986 allows as sub-delims.
    /// The userinfo ends at the last `@` of the authority, so the whole password
    /// and the query go, and text after the URL stays. Lines without those
    /// characters come out unchanged.
    #[test]
    fn url_redaction_covers_userinfo_with_quote_and_parentheses() {
        let redacted = redact_urls(
            "a https://user:it's@host/v1?key=abc b\n\
             c https://user:p'a(s)s@host/v1?key=abc d\n\
             e https://example.com/v1/models f\n",
        );

        assert_eq!(
            redacted,
            "a https://host/v1 b\nc https://host/v1 d\ne https://example.com/v1/models f\n"
        );
    }

    /// An IPv6 literal host sits between `[` and `]`. The brackets belong to
    /// the host, so the port and the path stay and the query goes, with or
    /// without userinfo in front of the literal.
    #[test]
    fn url_redaction_keeps_an_ipv6_literal_host_and_drops_the_query() {
        let redacted = redact_urls(
            "a http://[::1]:8080/v1?key=abc b\n\
             c http://user:pw@[::1]:8080/v1?key=abc d\n",
        );

        assert_eq!(
            redacted,
            "a http://[::1]:8080/v1 b\nc http://[::1]:8080/v1 d\n"
        );
    }

    /// A flush between the two halves of a URL emits nothing: the writer keeps
    /// the partial line until its newline arrives, so the URL goes out whole
    /// and redacted.
    #[test]
    fn redacting_writer_keeps_a_partial_line_across_flush() {
        let buffer = Buffer::default();
        let mut writer = RedactingWriter::new(buffer.clone());
        io::Write::write_all(&mut writer, b"endpoint https://user:se").expect("write");
        io::Write::flush(&mut writer).expect("flush");
        io::Write::write_all(&mut writer, b"cret@host/v1?key=abc\n").expect("write");

        assert_eq!(captured_text(&buffer), "endpoint https://host/v1\n");
    }

    /// The subscriber the stderr arm builds, given an in-memory writer in
    /// place of stderr. A warn with an endpoint in a field goes through it,
    /// and neither the userinfo nor the query reaches the writer.
    #[test]
    fn subscriber_over_scrubs_the_writer_it_is_given() {
        let buffer = Buffer::default();
        let sink = buffer.clone();
        let subscriber = subscriber_over(
            tracing_subscriber::EnvFilter::new("info"),
            move || sink.clone(),
            true,
        );

        tracing::subscriber::with_default(subscriber, || {
            let endpoint = "custom:https://user:secret@host/v1?key=abc";
            tracing::warn!(provider = endpoint, "refresh failed for {endpoint}");
        });

        let captured = captured_text(&buffer);
        assert!(
            !captured.contains("secret"),
            "userinfo reached the writer: {captured}"
        );
        assert!(
            !captured.contains("key=abc"),
            "query reached the writer: {captured}"
        );
        assert!(captured.contains("host/v1"), "host was removed: {captured}");
    }

    /// The stderr arm of `build_subscriber` writes through a function-style
    /// writer, not a `Mutex<File>`. The shared writer selection must scrub a
    /// writer of that kind too.
    #[test]
    fn redacting_make_writer_scrubs_the_stderr_style_writer() {
        let buffer = Buffer::default();
        let sink = buffer.clone();
        let subscriber = tracing_subscriber::fmt::Subscriber::builder()
            .with_writer(redacting_writer(move || sink.clone()))
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let endpoint = "custom:https://user:secret@host/v1?key=abc";
            tracing::warn!(provider = endpoint, "refresh failed for {endpoint}");
        });

        let captured = captured_text(&buffer);
        assert!(
            !captured.contains("secret"),
            "userinfo reached the stderr-style writer: {captured}"
        );
        assert!(
            !captured.contains("key=abc"),
            "query reached the stderr-style writer: {captured}"
        );
        assert!(captured.contains("host/v1"), "host was removed: {captured}");
    }

    /// The whole point of the bridge: a `log::warn!` (and `log::info!`) emitted
    /// through the `log` crate must reach the active `tracing` subscriber so
    /// the record lands in the same journal as tracing-native events.
    ///
    /// `install_log_bridge` is idempotent (silently ignores `AlreadySet`),
    /// so it is safe to call at the top of every test in this module —
    /// `log::set_logger` returns `Err(SetLoggerError)` on the second call,
    /// the bridge installed by the first call stays in place, and the
    /// thread-local subscriber installed by `with_default` still receives
    /// every forwarded record.
    #[test]
    fn log_records_forward_to_tracing_subscriber() {
        install_log_bridge();

        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            log::warn!(target: "wa_rs_test_target", "warning message body");
            log::info!(target: "wa_rs_test_target", "info message body");
        });

        let captured =
            String::from_utf8(buffer.0.lock().expect("buffer lock").clone()).expect("utf-8 output");
        assert!(
            captured.contains("warning message body"),
            "log::warn! body did not reach the tracing subscriber: {captured:?}"
        );
        assert!(
            captured.contains("info message body"),
            "log::info! body did not reach the tracing subscriber: {captured:?}"
        );
        assert!(
            captured.contains("wa_rs_test_target"),
            "log record target did not reach the tracing subscriber: {captured:?}"
        );
    }

    /// The default `EnvFilter` must hold against `log` records that the
    /// `wa-rs`/`hyper`/`mio::poll`/`notify::inotify` flood the bridge with.
    /// Without the explicit `dispatch.enabled(meta)` check inside the bridge,
    /// every `log::trace!`/`log::debug!` reaches the formatter because the
    /// dispatcher collapses the per-(target, level) `Metadata` into the
    /// anchor's interest cache (the anchor's "log" INFO target is `always`
    /// for the `info` directive, so the macro path skips per-event filtering).
    /// Build the filter directly from `DEFAULT_DIRECTIVES` so a stray
    /// `RUST_LOG` in the test env cannot change what we are asserting.
    #[test]
    fn default_filter_rejects_noisy_records_below_their_pinned_level() {
        install_log_bridge();

        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(DEFAULT_DIRECTIVES))
            .with_writer(buffer.clone())
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            // Below the base `info` level — must be rejected.
            log::trace!(target: "mio::poll", "mio trace line that should be filtered");
            log::trace!(
                target: "notify::inotify",
                "notify trace line that should be filtered"
            );
            log::debug!(target: "hyper", "hyper debug line that should be filtered");
            // Above the base level but below the `hyper=warn` pin — rejected.
            log::info!(target: "hyper", "hyper info line that should be filtered");
            // wa-rs per-message INFO lines carry full LIDs / group JIDs and
            // are exactly what the `wa_rs=warn` and `Client=warn` pins exist
            // to filter out — the prefix match must reach `wa_rs::receipt`,
            // `wa_rs_libsignal`, and `Client/Receipt` too. The matching WARN
            // lines (`Failed to resolve devices`, `No sender key for group`)
            // are the ones worth keeping.
            log::info!(
                target: "wa_rs::receipt",
                "wa_rs receipt info line that should be filtered"
            );
            log::warn!(
                target: "wa_rs::send",
                "wa_rs send warn line that should pass"
            );
            log::info!(
                target: "wa_rs_libsignal",
                "wa_rs libsignal info line that should be filtered"
            );
            log::info!(
                target: "Client/Receipt",
                "client receipt info line that should be filtered"
            );
            log::warn!(
                target: "Client/Receipt",
                "client receipt warn line that should pass"
            );
            // Allowed: `hyper=warn` and the wa-rs WARN lines.
            log::warn!(target: "hyper", "hyper warn line that should pass");
        });

        let captured =
            String::from_utf8(buffer.0.lock().expect("buffer lock").clone()).expect("utf-8 output");

        // Rejected records must not appear at all in the formatter output.
        assert!(
            !captured.contains("mio trace line"),
            "mio::poll TRACE leaked through the default filter: {captured:?}"
        );
        assert!(
            !captured.contains("notify trace line"),
            "notify::inotify TRACE leaked through the default filter: {captured:?}"
        );
        assert!(
            !captured.contains("hyper debug line"),
            "hyper DEBUG leaked through the default filter: {captured:?}"
        );
        assert!(
            !captured.contains("hyper info line"),
            "hyper INFO leaked past the hyper=warn pin: {captured:?}"
        );
        assert!(
            !captured.contains("wa_rs receipt info line"),
            "wa_rs::receipt INFO leaked past the wa_rs=warn pin: {captured:?}"
        );
        assert!(
            !captured.contains("wa_rs libsignal info line"),
            "wa_rs_libsignal INFO leaked past the wa_rs=warn prefix pin: {captured:?}"
        );
        assert!(
            !captured.contains("client receipt info line"),
            "Client/Receipt INFO leaked past the Client=warn pin: {captured:?}"
        );

        // Allowed records must reach the formatter, and they must carry the
        // real per-(target, level) `Metadata::target()` — not the anchor's
        // placeholder `"log"` — otherwise the formatter would still show the
        // anchor's static target and per-target filtering would be a lie.
        assert!(
            captured.contains("hyper warn line"),
            "hyper WARN did not reach the subscriber: {captured:?}"
        );
        assert!(
            captured.contains("wa_rs send warn line"),
            "wa_rs::send WARN did not reach the subscriber: {captured:?}"
        );
        assert!(
            captured.contains("client receipt warn line"),
            "Client/Receipt WARN did not reach the subscriber: {captured:?}"
        );
        assert!(
            captured.contains("hyper"),
            "real target hyper did not reach the formatter: {captured:?}"
        );
        assert!(
            captured.contains("wa_rs::send"),
            "real target wa_rs::send did not reach the formatter: {captured:?}"
        );
        assert!(
            captured.contains("Client/Receipt"),
            "real target Client/Receipt did not reach the formatter: {captured:?}"
        );
    }

    /// `RUST_LOG` overrides the default directives entirely — the documented
    /// `RUST_LOG=info,wa_rs=debug,Client=debug` diagnosis-session value must
    /// let the wa-rs `INFO` lines through that the default filter rejects.
    /// Pin the precedence order in a test that sets `RUST_LOG` under the
    /// crate-wide env-var lock so a stray `RUST_LOG` in the developer's shell
    /// cannot make this pass or fail by accident.
    #[test]
    fn rust_log_overrides_default_directives() {
        install_log_bridge();

        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let _log = crate::test_env::EnvGuard::set("RUST_LOG", "info,wa_rs=debug,Client=debug");

        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(crate::logging::default_env_filter())
            .with_writer(buffer.clone())
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            log::info!(
                target: "wa_rs::receipt",
                "wa_rs receipt info line that RUST_LOG should let through"
            );
            log::info!(
                target: "Client/Receipt",
                "client receipt info line that RUST_LOG should let through"
            );
            log::info!(
                target: "wa_rs_libsignal",
                "wa_rs libsignal info line that RUST_LOG should let through"
            );
        });

        let captured =
            String::from_utf8(buffer.0.lock().expect("buffer lock").clone()).expect("utf-8 output");

        assert!(
            captured.contains("wa_rs receipt info line"),
            "RUST_LOG=wa_rs=debug should let wa_rs::receipt INFO through: {captured:?}"
        );
        assert!(
            captured.contains("client receipt info line"),
            "RUST_LOG=Client=debug should let Client/Receipt INFO through: {captured:?}"
        );
        assert!(
            captured.contains("wa_rs libsignal info line"),
            "RUST_LOG=wa_rs=debug should cover wa_rs_libsignal by prefix: {captured:?}"
        );
    }

    /// The production subscriber built with a log file writes an endpoint with
    /// userinfo and a query, in a field and in the message, without either
    /// part. The host stays. The control lines, a plain URL and a line with no
    /// URL, come out unchanged.
    #[test]
    fn build_subscriber_with_log_file_writes_no_userinfo_or_query() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rantaiclaw-test.log");
        let file = std::fs::File::create(&path).expect("create log file");
        let subscriber = build_subscriber(tracing_subscriber::EnvFilter::new("info"), Some(file));

        tracing::subscriber::with_default(subscriber, || {
            let endpoint = "custom:https://user:secret@host/v1?key=abc";
            tracing::warn!(provider = endpoint, "refresh failed for {endpoint}");
            tracing::info!("models at https://example.com/v1/models");
            tracing::info!("no url on this line");
        });

        let written = std::fs::read_to_string(&path).expect("read log file");
        assert!(
            !written.contains("secret"),
            "userinfo reached the file: {written}"
        );
        assert!(
            !written.contains("key=abc"),
            "query reached the file: {written}"
        );
        assert!(written.contains("host/v1"), "host was removed: {written}");
        assert!(
            written.contains("https://example.com/v1/models"),
            "a plain URL was altered: {written}"
        );
        assert!(
            written.contains("no url on this line"),
            "a line without a URL was altered: {written}"
        );
    }
}
