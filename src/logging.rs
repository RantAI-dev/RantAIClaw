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
//! with its original target (e.g. `wa_rs`) intact — `wa_rs=info` filtering
//! works through the standard `EnvFilter` directive.
//!
//! An explicit `RUST_LOG` still wins entirely; when unset,
//! [`default_env_filter`] falls back to [`DEFAULT_DIRECTIVES`], which keeps
//! the noisy HTTP/TLS crates at `warn` to prevent flooding and lets `wa_rs`
//! through at `info`.

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
/// (request/response framing, TLS handshakes, h2 control frames). `wa_rs=info`
/// documents intent for the wa-rs bridge, but it matches only targets with the
/// `wa_rs` prefix. wa-rs's explicit `Client/*` target strings (`Client/TcToken`,
/// `Client/Receipt`) are at info/warn and pass on the base `info` level rather
/// than through the directive.
pub const DEFAULT_DIRECTIVES: &str = "info,hyper=warn,reqwest=warn,rustls=warn,h2=warn,wa_rs=info";

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};

    /// Pin the directive string. A refactor that loses the noisy-crate pins
    /// or the `wa_rs=info` line would otherwise silently flood the journal;
    /// this test catches it here, not in a reviewer reading a giant diff.
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
            DEFAULT_DIRECTIVES.contains("wa_rs=info"),
            "wa_rs=info pin missing: {DEFAULT_DIRECTIVES}"
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
            // Allowed: `hyper=warn` and the explicit `wa_rs=info` pin.
            log::warn!(target: "hyper", "hyper warn line that should pass");
            log::info!(target: "wa_rs::send", "wa_rs info line that should pass");
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

        // Allowed records must reach the formatter, and they must carry the
        // real per-(target, level) `Metadata::target()` — not the anchor's
        // placeholder `"log"` — otherwise the formatter would still show the
        // anchor's static target and per-target filtering would be a lie.
        assert!(
            captured.contains("hyper warn line"),
            "hyper WARN did not reach the subscriber: {captured:?}"
        );
        assert!(
            captured.contains("wa_rs info line"),
            "wa_rs INFO did not reach the subscriber: {captured:?}"
        );
        assert!(
            captured.contains("hyper"),
            "real target hyper did not reach the formatter: {captured:?}"
        );
        assert!(
            captured.contains("wa_rs::send"),
            "real target wa_rs::send did not reach the formatter: {captured:?}"
        );
    }
}
