//! One-shot migration from the v0.4.x flat layout (`~/.rantaiclaw/{config.toml, workspace, ...}`)
//! to the v0.5.0 profile-aware layout (`~/.rantaiclaw/profiles/default/...`).
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md` §7.1.
//!
//! Invariants:
//! - **Idempotent.** Re-runs after success are silent no-ops (the detection
//!   predicate stops being true the moment the active_profile marker exists).
//! - **Race-safe.** Concurrent invocations are serialized via an exclusive
//!   advisory lock at `~/.rantaiclaw/migrate.lock`. The losing caller bails
//!   out silently — by the time it gives up the winner has finished.
//! - **Atomic per file.** `rename` on the same filesystem is atomic; on
//!   `EXDEV` we fall back to recursive copy + delete.
//! - **Reversible-on-crash.** Locked + idempotent — a half-finished
//!   migration just retries on the next launch.
//!
//! Symlink lifecycle (Unix only):
//! - v0.5.0: created (silent fallback for external scripts)
//! - v0.6.0: warn-on-direct-access (planned; not implemented here)
//! - v0.7.0: removed (planned; not implemented here)

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;

use crate::profile::paths;

/// Debug-build guard: skip the legacy-layout migration unless `HOME` is under
/// `std::env::temp_dir()` (or the opt-out is set). A run that never pins `HOME`
/// would otherwise move the developer's real flat layout into
/// `profiles/default/`; it also fires for `cargo run` and spawned debug
/// binaries. Compiled out of release builds; production always migrates.
#[cfg(any(test, debug_assertions))]
fn home_is_test_safe() -> bool {
    if crate::profile::dev_guard::allow_real_config_dir() {
        return true;
    }
    home_is_test_safe_for(std::env::var_os("HOME").as_deref())
}

/// The `HOME` half of [`home_is_test_safe`], with the value passed in so a test
/// can cover the unset case without touching the process environment.
#[cfg(any(test, debug_assertions))]
fn home_is_test_safe_for(home: Option<&std::ffi::OsStr>) -> bool {
    let Some(home) = home else {
        // `directories::UserDirs` (via `dirs-sys`) falls back to a
        // `getpwuid_r` lookup when `HOME` is unset, so `paths::rantaiclaw_root()`
        // still resolves to the real system user's home directory rather than
        // erroring out. An unset HOME is therefore unsafe, not exempt.
        return false;
    };
    crate::profile::dev_guard::is_under_temp_dir(Path::new(home))
}

/// Public entry point. Call this once at the very top of `Config::load_or_init`
/// (and any other config-reading entry that bypasses it). Returns `Ok(true)`
/// iff the migration actually fired this call; `Ok(false)` otherwise.
pub fn maybe_migrate_legacy_layout() -> Result<bool> {
    #[cfg(any(test, debug_assertions))]
    {
        if !home_is_test_safe() {
            return Ok(false);
        }
    }

    if !needs_migration() {
        return Ok(false);
    }

    let root = paths::rantaiclaw_root();
    fs::create_dir_all(&root)
        .with_context(|| format!("create rantaiclaw root {}", root.display()))?;

    let lock_path = paths::migration_lock_file();
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open migration lock {}", lock_path.display()))?;

    // Race-loser path: another process already holds the lock. They will
    // finish or have finished the migration; we silently no-op.
    if lock_file.try_lock_exclusive().is_err() {
        return Ok(false);
    }

    // Re-check inside the lock so we don't double-migrate after the loser
    // wakes up post-success.
    let did = if needs_migration() {
        let result = perform_migration();
        // Always release the lock, even on failure.
        let _ = FileExt::unlock(&lock_file);
        result?;
        true
    } else {
        let _ = FileExt::unlock(&lock_file);
        false
    };

    Ok(did)
}

/// The pre-profile global data dir (`~/.local/share/rantaiclaw/` on Linux)
/// where `sessions.db` and `kb.db` leaked before the per-profile fix. `None`
/// only when the platform has no resolvable data dir (no HOME).
#[cfg(any(test, debug_assertions))]
fn global_data_dir() -> Option<PathBuf> {
    // A debug build must not move a database out of the developer's real data
    // dir into a profile, so a data dir outside the temp dir counts as none.
    platform_data_dir().filter(|d| {
        crate::profile::dev_guard::allow_real_config_dir()
            || crate::profile::dev_guard::is_under_temp_dir(d)
    })
}

#[cfg(not(any(test, debug_assertions)))]
fn global_data_dir() -> Option<PathBuf> {
    platform_data_dir()
}

fn platform_data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "rantaiclaw").map(|d| d.data_dir().to_path_buf())
}

/// One-shot migration of the global `sessions.db` into the `default` profile.
///
/// Pre-fix every profile shared one `~/.local/share/rantaiclaw/sessions.db`;
/// each profile now owns `profiles/<name>/sessions/sessions.db`. We MOVE the
/// legacy file into `profiles/default/` (running without `--profile` resolves
/// to `default`, so it inherits the history; other profiles start empty).
///
/// Invariants mirror `maybe_migrate_legacy_layout`: idempotent (the source is
/// gone after a successful move), race-safe (advisory flock), and it never
/// clobbers a populated destination. Returns `Ok(true)` iff a move happened.
pub fn maybe_migrate_global_sessions_db() -> Result<bool> {
    let Some(global) = global_data_dir() else {
        return Ok(false);
    };
    migrate_global_db_locked(
        &global.join("sessions.db"),
        &paths::sessions_db("default"),
        "migrate_sessions.lock",
    )
}

/// One-shot migration of the global `kb.db` into the `default` profile.
///
/// Same rationale and guarantees as [`maybe_migrate_global_sessions_db`]: the
/// knowledge base used to live at one global `~/.local/share/rantaiclaw/kb.db`
/// shared by every profile. We MOVE it into `profiles/default/kb.db` so each
/// profile owns its own corpus. Returns `Ok(true)` iff a move happened.
pub fn maybe_migrate_global_kb_db() -> Result<bool> {
    let Some(global) = global_data_dir() else {
        return Ok(false);
    };
    migrate_global_db_locked(
        &global.join("kb.db"),
        &paths::kb_db("default"),
        "migrate_kb.lock",
    )
}

/// Shared driver for the global-db → per-profile-db migrations. Detection is
/// "source exists AND destination does not"; a populated destination is never
/// overwritten. The move is WAL-checkpointed first and `EXDEV`-safe.
fn migrate_global_db_locked(src: &Path, dst: &Path, lock_name: &str) -> Result<bool> {
    if !src.exists() || dst.exists() {
        return Ok(false);
    }

    let root = paths::rantaiclaw_root();
    fs::create_dir_all(&root)
        .with_context(|| format!("create rantaiclaw root {}", root.display()))?;

    let lock_path = root.join(lock_name);
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open db-migration lock {}", lock_path.display()))?;

    // Race-loser path: another process holds the lock and will finish (or has
    // finished) the move; silently no-op.
    if lock_file.try_lock_exclusive().is_err() {
        return Ok(false);
    }

    // Re-check under the lock so a woken loser cannot double-move.
    let did = if src.exists() && !dst.exists() {
        let result = checkpoint_and_move_db(src, dst);
        let _ = FileExt::unlock(&lock_file);
        result?;
        true
    } else {
        let _ = FileExt::unlock(&lock_file);
        false
    };
    Ok(did)
}

/// Fold a SQLite WAL back into its main `.db`, then move the single file to
/// `dst` and drop the now-inert `-wal`/`-shm` sidecars at the source.
///
/// The checkpoint is load-bearing: `sessions.db-wal` can be larger than the
/// `.db` itself, so a naive `mv sessions.db` would silently lose every
/// uncommitted page. `wal_checkpoint(TRUNCATE)` writes those pages into the
/// main file and zeroes the WAL before we touch it.
fn checkpoint_and_move_db(src: &Path, dst: &Path) -> Result<()> {
    if let Ok(conn) = rusqlite::Connection::open(src) {
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        // Ignore the returned (busy, log, checkpointed) row — a failure here
        // just means we fall back to moving whatever is already in the .db.
        let _: std::result::Result<i64, _> =
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0));
        let _ = conn.close();
    }

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create db parent {}", parent.display()))?;
    }

    match fs::rename(src, dst) {
        Ok(()) => {}
        Err(e) if is_cross_device(&e) => {
            copy_recursive(src, dst)?;
            remove_recursive(src)?;
        }
        Err(e) => {
            return Err(e).with_context(|| format!("move {} -> {}", src.display(), dst.display()));
        }
    }

    // Best-effort: the sidecars are 0-byte after TRUNCATE; leave nothing stale.
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(sidecar(src, suffix));
    }
    Ok(())
}

/// `sessions.db` + `-wal` → `sessions.db-wal`. SQLite names sidecars by
/// appending to the full db filename, not by swapping the extension.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Detection predicate (also exposed for tests).
pub fn needs_migration() -> bool {
    let root = paths::rantaiclaw_root();
    root.join("config.toml").exists()
        && !root.join("profiles").exists()
        && !root.join("active_profile").exists()
}

fn perform_migration() -> Result<()> {
    let root = paths::rantaiclaw_root();
    let dest = paths::profile_dir("default");
    fs::create_dir_all(&dest).with_context(|| format!("create profile dir {}", dest.display()))?;

    // Anything that lived at `~/.rantaiclaw/<name>` and now lives at
    // `~/.rantaiclaw/profiles/default/<name>`.
    //
    // `.secret_key` MUST move with `config.toml`. SecretStore derives its
    // key path from `config_path.parent()`, so leaving the legacy key at
    // root while the profile dir spawns a fresh one breaks api_key
    // decryption on the next load.
    let movables = [
        "config.toml",
        ".secret_key",
        "secrets",
        "workspace",
        "memory",
        "sessions",
        "skills",
        "persona",
        "policy",
        "audit.log",
        ".onboard_progress",
    ];
    for name in &movables {
        let src = root.join(name);
        if !src.exists() {
            continue;
        }
        let dst = dest.join(name);
        match fs::rename(&src, &dst) {
            Ok(()) => {}
            Err(e) => {
                if is_cross_device(&e) {
                    copy_recursive(&src, &dst)?;
                    remove_recursive(&src)?;
                } else if dst.exists() {
                    // Partial-state recovery: dst already has the data;
                    // best-effort remove src so we don't leave a stale copy.
                    let _ = remove_recursive(&src);
                } else {
                    return Err(e)
                        .with_context(|| format!("move {} -> {}", src.display(), dst.display()));
                }
            }
        }
    }

    // Transitional symlinks (Unix only — Windows: skip silently). These
    // exist so external scripts that still read the old paths keep working
    // until v0.7.0.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        // Re-create only if not already present (a previous partial run
        // may have made them).
        let cfg_link = root.join("config.toml");
        if !cfg_link.exists() {
            let _ = symlink(dest.join("config.toml"), &cfg_link);
        }
        let ws_link = root.join("workspace");
        if !ws_link.exists() {
            let _ = symlink(dest.join("workspace"), &ws_link);
        }
    }

    fs::write(paths::active_profile_file(), "default\n").context("write active_profile marker")?;
    fs::write(paths::version_file(), env!("CARGO_PKG_VERSION")).context("write version stamp")?;
    fs::write(
        root.join("MIGRATION_NOTICE.md"),
        include_str!("migration_notice.md"),
    )
    .context("write MIGRATION_NOTICE.md")?;

    eprintln!(
        "==> Migrated to profile-aware layout (profiles/default/). \
         See ~/.rantaiclaw/MIGRATION_NOTICE.md"
    );
    Ok(())
}

fn is_cross_device(e: &std::io::Error) -> bool {
    // EXDEV = 18 on Linux. Use libc constant on unix; on other platforms,
    // any rename failure that isn't already-handled lands here.
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EXDEV)
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let target = dst.join(entry.file_name());
            if ft.is_dir() {
                copy_recursive(&entry.path(), &target)?;
            } else if ft.is_file() {
                fs::copy(entry.path(), &target)?;
                // Best-effort permissions preservation
                if let Ok(meta) = entry.metadata() {
                    let _ = fs::set_permissions(&target, meta.permissions());
                }
            }
            // symlinks: ignored. v0.4.x layout has none we care about.
        }
    } else if src.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
    }
    Ok(())
}

fn remove_recursive(p: &Path) -> Result<()> {
    if !p.exists() {
        return Ok(());
    }
    if p.is_dir() {
        fs::remove_dir_all(p)?;
    } else {
        fs::remove_file(p)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `directories::UserDirs::new()` (reached via `paths::rantaiclaw_root()`
    /// deeper in this module) falls back to a `getpwuid_r` lookup when `HOME`
    /// is unset, resolving to the real system user's home directory — not a
    /// safe default. The guard must refuse an unset `HOME`, not admit it.
    #[test]
    fn home_is_test_safe_refuses_an_unset_home() {
        assert!(
            !home_is_test_safe_for(None),
            "an unset HOME must not be treated as safe"
        );
    }

    /// The global data dir is where `sessions.db` and `kb.db` leaked before the
    /// per-profile fix. With `HOME` outside the temp dir it is the developer's
    /// real data dir, so a debug build must report none and migrate nothing.
    #[test]
    fn global_data_dir_is_none_when_home_is_not_under_temp_dir() {
        let _env_guard = crate::test_env::ENV_LOCK.blocking_lock();
        let fake_home = non_temp_dir_scratch_dir();
        let _g_home = crate::test_env::EnvGuard::set("HOME", &fake_home);
        let _g_xdg = crate::test_env::EnvGuard::unset("XDG_DATA_HOME");
        let _g_allow = crate::test_env::EnvGuard::unset("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");

        assert_eq!(global_data_dir(), None);
    }

    #[test]
    fn global_data_dir_follows_a_home_under_the_temp_dir() {
        let _env_guard = crate::test_env::ENV_LOCK.blocking_lock();
        let temp_home = tempfile::tempdir().expect("temp home");
        let _g_home = crate::test_env::EnvGuard::set("HOME", temp_home.path());
        let _g_xdg = crate::test_env::EnvGuard::unset("XDG_DATA_HOME");
        let _g_allow = crate::test_env::EnvGuard::unset("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");

        let dir = global_data_dir().expect("a temp home has a data dir");

        assert!(dir.starts_with(temp_home.path()), "{dir:?}");
    }

    #[test]
    fn global_data_dir_follows_home_when_the_opt_out_is_set() {
        let _env_guard = crate::test_env::ENV_LOCK.blocking_lock();
        let fake_home = non_temp_dir_scratch_dir();
        let _g_home = crate::test_env::EnvGuard::set("HOME", &fake_home);
        let _g_xdg = crate::test_env::EnvGuard::unset("XDG_DATA_HOME");
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");

        let dir = global_data_dir().expect("the opt-out keeps the platform data dir");

        assert!(dir.starts_with(&fake_home), "{dir:?}");
    }

    /// A directory that is deliberately NOT under `std::env::temp_dir()` — a
    /// throwaway spot inside this crate's own `target/`, so a guard that
    /// fails to fire only ever touches build output, never the developer's
    /// real `$HOME`.
    fn non_temp_dir_scratch_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "rantaiclaw_test_legacy_guard_{}",
                uuid::Uuid::new_v4()
            ))
    }

    /// Write a v0.4.x flat layout (`config.toml`, no `profiles/`, no
    /// `active_profile`) under `home/.rantaiclaw`, satisfying
    /// [`needs_migration`]'s predicate.
    fn seed_legacy_layout(home: &Path) {
        let root = home.join(".rantaiclaw");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("config.toml"), "schema_version = 1\n").unwrap();
    }

    /// Without a `HOME` pin, the guard must refuse to touch a legacy layout
    /// at all — not even the read-only `needs_migration` check — because
    /// `HOME` here is a scratch dir outside `std::env::temp_dir()`, standing
    /// in for the developer's real, unpinned `$HOME`.
    #[test]
    fn maybe_migrate_legacy_layout_skips_when_home_is_not_under_temp_dir() {
        let _env_guard = crate::test_env::ENV_LOCK.blocking_lock();
        let fake_home = non_temp_dir_scratch_dir();
        seed_legacy_layout(&fake_home);
        assert!(
            !fake_home.starts_with(std::env::temp_dir()),
            "test setup bug: the scratch dir must not itself be under temp_dir"
        );
        let _g_home = crate::test_env::EnvGuard::set("HOME", &fake_home);
        let _g_allow = crate::test_env::EnvGuard::unset("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");

        let migrated = maybe_migrate_legacy_layout().unwrap();

        assert!(!migrated, "the guard must report no migration happened");
        assert!(
            !fake_home.join(".rantaiclaw").join("profiles").exists(),
            "the guard must refuse the migration outright, not just report false"
        );

        let _ = fs::remove_dir_all(&fake_home);
    }

    /// Pinning `HOME` to an actual tempdir is the documented way to opt into
    /// exercising a real migration; the guard must not block it.
    #[test]
    fn maybe_migrate_legacy_layout_runs_when_home_is_under_temp_dir() {
        let _env_guard = crate::test_env::ENV_LOCK.blocking_lock();
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        seed_legacy_layout(&temp_home);
        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_allow = crate::test_env::EnvGuard::unset("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");

        let migrated = maybe_migrate_legacy_layout().unwrap();

        assert!(
            migrated,
            "a legacy layout under a pinned temp HOME must migrate"
        );
        assert!(temp_home
            .join(".rantaiclaw")
            .join("profiles")
            .join("default")
            .exists());

        let _ = fs::remove_dir_all(&temp_home);
    }
}
