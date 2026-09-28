//! Shared test helpers for integration test binaries under `tests/`.
//!
//! `tests/*.rs` files each compile as their own test binary, so the
//! `crate::test_env` guards in `src/test_env.rs` (`pub(crate)`) are not
//! reachable here. Any integration test that reaches
//! `Config::load_or_init()` or `Config::save()` must pin its own
//! `RANTAICLAW_CONFIG_DIR` instead, or it resolves against the real
//! `$HOME/.rantaiclaw` when the ambient environment does not already
//! override it.
//!
//! This file lives at `tests/common/mod.rs` (not `tests/common.rs`) so
//! Cargo's test-target auto-discovery does not treat it as its own test
//! binary; each test file that needs it declares `mod common;`.

use tokio::sync::Mutex;

/// Process-wide lock serializing every test in a binary that mutates
/// `RANTAICLAW_CONFIG_DIR`. `std::env::set_var` is process-global, and
/// `cargo test` runs the `#[test]`/`#[tokio::test]` functions of one binary
/// on separate threads, so an unlocked pair of tests racing this var can
/// have one read the other's temp dir mid-test.
static CONFIG_ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// Points `RANTAICLAW_CONFIG_DIR` at a fresh temp directory for the guard's
/// lifetime, holding [`CONFIG_ENV_LOCK`] so no sibling test in the same
/// binary can race the env var. Restores whatever value (or absence) the var
/// held before `acquire` on drop, rather than unconditionally clearing it —
/// a sequential acquire must hand the var back to its caller, not erase it
/// out from under one.
#[must_use = "the override is reverted the moment this guard is dropped; bind it to a named local"]
pub struct ConfigDirGuard {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl ConfigDirGuard {
    /// Acquire the lock and set `RANTAICLAW_CONFIG_DIR` to a fresh temp
    /// directory.
    pub async fn acquire() -> Self {
        let lock = CONFIG_ENV_LOCK.lock().await;
        let prev = std::env::var_os("RANTAICLAW_CONFIG_DIR");
        let dir = tempfile::tempdir().expect("tempdir for RANTAICLAW_CONFIG_DIR");
        std::env::set_var("RANTAICLAW_CONFIG_DIR", dir.path());
        Self {
            _lock: lock,
            dir,
            prev,
        }
    }

    /// The temp directory `RANTAICLAW_CONFIG_DIR` currently points at.
    pub fn path(&self) -> &std::path::Path {
        self.dir.path()
    }
}

impl Drop for ConfigDirGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(prev) => std::env::set_var("RANTAICLAW_CONFIG_DIR", prev),
            None => std::env::remove_var("RANTAICLAW_CONFIG_DIR"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_points_config_dir_at_the_guards_tempdir() {
        let guard = ConfigDirGuard::acquire().await;
        let set = std::env::var("RANTAICLAW_CONFIG_DIR").expect("guard sets the var");
        assert_eq!(std::path::Path::new(&set), guard.path());
    }
}
