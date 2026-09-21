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
/// Unset every credential variable a test must not inherit from the developer's
/// own shell, restoring them all on drop.
///
/// `has_usable_credential` reads the environment, so any test asserting "no
/// credential anywhere" passes vacuously on a machine with `OPENROUTER_API_KEY`
/// exported — and asserts the opposite of what it means on a machine without
/// it. Both doctor and providers need this, so it lives here rather than as a
/// third copy.
///
/// Only meaningful while [`ENV_LOCK`] is held.
pub(crate) struct CredentialEnvScrub(#[allow(dead_code)] Vec<EnvGuard>);

impl CredentialEnvScrub {
    pub(crate) fn new() -> Self {
        Self(
            [
                "RANTAICLAW_API_KEY",
                "API_KEY",
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_OAUTH_TOKEN",
                "OPENROUTER_API_KEY",
                "GROQ_API_KEY",
                "OLLAMA_API_KEY",
                "GEMINI_API_KEY",
                "GOOGLE_API_KEY",
                "DASHSCOPE_API_KEY",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
                "QWEN_OAUTH_TOKEN",
                "QWEN_OAUTH_REFRESH_TOKEN",
                "XDG_CONFIG_HOME",
            ]
            .into_iter()
            .map(EnvGuard::unset)
            .collect(),
        )
    }
}

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

/// Set (or clear) an arbitrary env var and put the previous value back on drop,
/// so a test that panics between the set and its trailing `remove_var` does not
/// leak the override into the next test sharing `ENV_LOCK`. The generic sibling
/// of [`HomeGuard`] — for `RANTAICLAW_API_KEY`, `RANTAICLAW_PROVIDER`,
/// `RANTAICLAW_CONFIG_DIR`, `PORT`, etc.
///
/// Bind it to a NAMED local (`let _guard = EnvGuard::set(...)`), never `let _ =`
/// — the latter drops immediately and restores before the test body runs.
///
/// Only meaningful while `ENV_LOCK` is held.
#[must_use = "the override is reverted the moment this guard is dropped; bind it to a named local"]
pub(crate) struct EnvGuard {
    key: &'static str,
    prev: Option<OsString>,
}

impl EnvGuard {
    /// Set `key` to `value` for the guard's lifetime.
    pub(crate) fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prev }
    }

    /// Ensure `key` is UNSET for the guard's lifetime (restoring any prior value
    /// on drop) — the `remove_var("X"); set_var("Y", …)` precedence pattern.
    pub(crate) fn unset(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(prev) => std::env::set_var(self.key, prev),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Acquire [`ENV_LOCK`] and point `RANTAICLAW_AUDIT_DIR_OVERRIDE` at a fresh
/// per-test temp directory, so any `record_tool_call` triggered during the
/// test lands there instead of in the operator's real `audit.log`. Returns
/// the two guards so the test body binds them as `let (_env, _audit) = …`.
///
/// The directory is a [`tempfile::TempDir`] held inside the returned
/// [`EnvAuditRedirect`], so it is removed from disk the moment the test drops
/// the guard, rather than left behind under the system temp directory.
#[must_use = "both guards revert the moment they drop; bind them to named locals"]
pub(crate) async fn redirect_audit_temp() -> (EnvAuditRedirect, EnvGuard) {
    let env = ENV_LOCK.lock().await;
    let dir = tempfile::Builder::new()
        .prefix("rantaiclaw-audit-redirect-")
        .tempdir()
        .expect("audit redirect tempdir");
    let audit = EnvGuard::set("RANTAICLAW_AUDIT_DIR_OVERRIDE", dir.path());
    (
        EnvAuditRedirect {
            _env: env,
            _dir: dir,
        },
        audit,
    )
}

/// Marker guard held for as long as the audit redirect must stay in effect.
/// Carries the `ENV_LOCK` guard and the temp directory
/// `RANTAICLAW_AUDIT_DIR_OVERRIDE` points at, so the helper can return a
/// tuple `(env_guard, audit_guard)` without leaking the lock type, and the
/// directory is deleted when the test drops this guard.
#[must_use = "the ENV_LOCK is released the moment this drops; bind it to a named local"]
pub(crate) struct EnvAuditRedirect {
    _env: tokio::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
}
