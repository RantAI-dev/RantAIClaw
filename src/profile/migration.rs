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

/// Public entry point. Call this once at the very top of `Config::load_or_init`
/// (and any other config-reading entry that bypasses it). Returns `Ok(true)`
/// iff the migration actually fired this call; `Ok(false)` otherwise.
pub fn maybe_migrate_legacy_layout() -> Result<bool> {
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
fn global_data_dir() -> Option<PathBuf> {
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
