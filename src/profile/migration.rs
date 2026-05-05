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

use std::fs::{self, OpenOptions};
use std::path::Path;

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
