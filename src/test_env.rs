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
/// directory is deleted when the test drops the guard.
#[must_use = "the ENV_LOCK is released the moment this drops; bind it to a named local"]
pub(crate) struct EnvAuditRedirect {
    _env: tokio::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
}

/// A temp directory holding fake binaries named `systemctl`, `launchctl`,
/// `rc-service`, `rc-update`, and `schtasks`. Prepending the directory to
/// `PATH` lets a test prove that production code does not spawn one of the
/// five service-manager programs: if the guard is in place, the fake is never
/// reached; if the guard is removed (mutation check), the fake answers the
/// query and writes a marker on restart so the failing test can be diagnosed.
///
/// The marker file lives next to the binary and is unique to this guard. The
/// `ENV_LOCK` is NOT acquired here — every caller must hold it for the lifetime
/// of the returned guard (see [`with_fake_service_manager`]).
#[must_use = "PATH is reverted the moment this drops; bind it to a named local"]
pub(crate) struct FakeServiceManager {
    _env: EnvGuard,
    pub(crate) dir: tempfile::TempDir,
}

impl FakeServiceManager {
    pub(crate) fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("rantaiclaw-fake-svcmgr-")
            .tempdir()
            .expect("fake service-manager dir");
        for name in [
            "systemctl",
            "launchctl",
            "rc-service",
            "rc-update",
            "schtasks",
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, FAKE_SERVICE_MANAGER_SCRIPT)
                .unwrap_or_else(|e| panic!("write fake {name}: {e}"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .unwrap_or_else(|e| panic!("chmod fake {name}: {e}"));
            }
        }
        let prev = std::env::var_os("PATH");
        let bin = dir.path().to_string_lossy();
        let new_path = match prev.as_ref().map(|p| p.to_string_lossy().into_owned()) {
            Some(p) => format!("{bin}:{p}"),
            None => bin.into_owned(),
        };
        let env = EnvGuard::set("PATH", &new_path);
        Self { _env: env, dir }
    }

    /// Path of the marker file the `systemctl` fake writes on restart.
    pub(crate) fn restart_marker(&self) -> std::path::PathBuf {
        self.dir.path().join("systemctl.restarted.marker")
    }

    /// Path of the marker file the `rc-service` fake writes on restart.
    pub(crate) fn rc_service_restart_marker(&self) -> std::path::PathBuf {
        self.dir.path().join("rc-service.restarted.marker")
    }
}

/// Acquire [`ENV_LOCK`] and install a [`FakeServiceManager`]. Tests that use
/// this helper cannot reach the host's real `systemctl`/`launchctl`/etc.
///
/// The lock is held by the lock guard passed in via the caller — `ENV_LOCK` is
/// reentrant-aware (it's a `tokio::sync::Mutex` acquired once per process)
/// and a sync test uses `blocking_lock`. We do not call `lock()` again here
/// because the fake's `EnvGuard` only mutates `PATH` and we want to keep that
/// critical section explicit at the test.
pub(crate) fn with_fake_service_manager<F: FnOnce(&FakeServiceManager)>(f: F) {
    let _lock = ENV_LOCK.blocking_lock();
    let fake = FakeServiceManager::new();
    f(&fake);
}

/// A single shell script that handles every program name in `$0`. Logs every
/// call, writes a marker on restart, and answers the queries the production
/// code sends so the test takes the "active / will restart" branch when the
/// guard is removed (the mutation scenario).
const FAKE_SERVICE_MANAGER_SCRIPT: &str = r#"#!/bin/sh
PROG="$(basename "$0")"
DIR="$(dirname "$0")"
MARKER="$DIR/$PROG.restarted.marker"
LOG="$DIR/$PROG.log"

echo "$@" >> "$LOG"

case "$PROG" in
  systemctl)
    # Strip option flags so the first positional arg is the action.
    ACTION=""
    for arg in "$@"; do
      case "$arg" in
        --user|--no-block|--no-pager) continue ;;
      esac
      ACTION="$arg"
      break
    done
    case "$ACTION" in
      restart) echo "restarted $(date +%s.%N)" >> "$MARKER"; exit 0 ;;
      is-active) echo "active"; exit 0 ;;
      status) echo "active"; exit 0 ;;
      cat) cat <<'EOF'
[Unit]
Description=test
EOF
        exit 0 ;;
    esac
    exit 0
    ;;
  launchctl)
    case "$1" in
      list) exit 0 ;;
    esac
    exit 0
    ;;
  rc-service)
    case "$2" in
      status) echo "started"; exit 0 ;;
      restart) echo "restarted $(date +%s.%N)" >> "$MARKER"; exit 0 ;;
    esac
    exit 0
    ;;
  rc-update|schtasks)
    exit 0
    ;;
esac
exit 0
"#;
