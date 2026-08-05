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

use std::ffi::OsString;
use std::path::Path;
use tokio::sync::Mutex;

pub(crate) static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// Point `HOME` at a temp dir and put the previous value back on drop, so a
/// panicking test does not leak the override into the next one.
///
/// `HOME` is the lever that moves per-profile state: the profile root is
/// `home_dir()/.rantaiclaw/profiles/<name>` (`profile/paths.rs`), so
/// `RANTAICLAW_CONFIG_DIR` does **not** move `sessions.db` — pinning that one
/// instead leaves the test writing into the operator's real session history.
///
/// Only meaningful while `ENV_LOCK` is held.
pub(crate) struct HomeGuard(Option<OsString>);

impl HomeGuard {
    pub(crate) fn set(path: &Path) -> Self {
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", path);
        Self(prev)
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(prev) => std::env::set_var("HOME", prev),
            None => std::env::remove_var("HOME"),
        }
    }
}
