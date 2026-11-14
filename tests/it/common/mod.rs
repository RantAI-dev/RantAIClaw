//! Shared test helpers for the package's single integration test binary.
//!
//! Every `tests/*.rs` file is now a module of one test binary, so the whole
//! suite shares one process. `std::env::set_var` is process-global and
//! `cargo test` runs the tests of one binary on separate threads, so a test
//! that mutates `HOME`, `RANTAICLAW_CONFIG_DIR` or a `KB_*` variable can be
//! seen mid-test by a sibling in another file. The `crate::test_env` guards
//! in `src/test_env.rs` (`pub(crate)`) are not reachable from an integration
//! binary, so this module owns the lock instead.
//!
//! `ENV_LOCK` is the single process-wide lock every such test takes:
//!
//! - async tests (`#[tokio::test]`): `crate::common::ENV_LOCK.lock().await`
//! - sync tests (`#[test]`, no runtime): `crate::common::ENV_LOCK.blocking_lock()`
//!
//! It is a `tokio::sync::Mutex` (not `std::sync::Mutex`) so an async test can
//! hold the guard across an `.await` point; `blocking_lock()` covers the sync
//! callers, which run outside any runtime.
//!
//! Pinning `RANTAICLAW_CONFIG_DIR` covers `config.toml` only. The profile,
//! workspace and audit paths derive from `HOME`. In a debug or test build,
//! `profile::paths::root_for_home` moves a `HOME` outside the temp dir to a
//! per-process temp root, so those paths stay off the real tree without any
//! setup. A test that needs a known root should set `HOME` to its own temp dir.
//!
//! This file lives at `tests/it/common/mod.rs` (not `tests/it/common.rs`) so
//! Cargo's test-target auto-discovery does not treat it as its own test
//! binary; the binary's root declares `mod common;` exactly once, so every
//! file that needs it reaches the same lock through `crate::common`.

use tokio::sync::Mutex;

/// Process-wide lock serializing every test in this binary that mutates a
/// process-global env var (`HOME`, `RANTAICLAW_CONFIG_DIR`,
/// `RANTAICLAW_PROFILE`, the `KB_*` family, credential variables).
///
/// One binary means one process, so a per-file static mutex no longer
/// serializes anything: two files that mutate different variables still
/// clobber each other's, because the variables are process-global. Every such
/// test therefore takes THIS lock.
pub static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// Points `RANTAICLAW_CONFIG_DIR` at a fresh temp directory for the guard's
/// lifetime, holding [`ENV_LOCK`] so no sibling test in the same
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
        let lock = ENV_LOCK.lock().await;
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
