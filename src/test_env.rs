//! Shared, process-wide serialization for tests that mutate config-resolution
//! environment variables.
//!
//! `Config::load_or_init` (and the profile/store resolution beneath it) reads
//! **process-global** env vars — `HOME`, `RANTAICLAW_CONFIG_DIR`,
//! `RANTAICLAW_WORKSPACE`, `RANTAICLAW_PROFILE`. `cargo test --lib` runs every
//! unit test in one process across many threads, so a per-module lock does
//! **not** serialize a test in `channels::slack` against one in
//! `channels::mattermost`: they hold different mutexes and clobber each other's
//! env var mid-test, which surfaced as flaky `unwrap()`-on-`None` panics.
//!
//! Every test that sets one of those vars must acquire THIS single lock:
//! - async tests (`#[tokio::test]`): `test_env::ENV_LOCK.lock().await`
//! - sync tests (`#[test]`, no runtime): `test_env::ENV_LOCK.blocking_lock()`
//!
//! It is a `tokio::sync::Mutex` (not `std::sync::Mutex`) so the async tests can
//! hold the guard across `.await` points; `blocking_lock()` covers the sync
//! callers, which run outside any runtime.

use tokio::sync::Mutex;

pub(crate) static ENV_LOCK: Mutex<()> = Mutex::const_new(());
