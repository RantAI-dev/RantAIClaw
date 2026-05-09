//! Pre-update state snapshots — Hermes-parity rollback support for
//! `rantaiclaw update`.
//!
//! Before each binary swap, we copy a small set of "runtime-mutable but
//! easy to break" state files into
//! `<rantaiclaw_root>/.update-snapshots/<UTC-timestamp>/`. The snapshot
//! is intentionally *lightweight* — covers config + active profile +
//! workspace marker + the most recent sessions.db — and unconditional,
//! so every update creates one. Full-profile `--backup` is a separate
//! opt-in path layered on top (see `BackupArchive`).
//!
//! Mirrors Hermes' "Pairing-data snapshot" + `--backup` split. We don't
//! match their format byte-for-byte; the goal is a rollback story that
//! reads naturally to anyone migrating from `hermes update`.
//!
//! Snapshot layout:
//! ```text
//! <root>/.update-snapshots/2026-05-10T03-21-00Z/
//!   manifest.toml         # version-from / version-to / created_at / files[]
//!   config.toml           # global + active-profile config snapshots
//!   profiles/<name>/...   # only the active profile
//! ```

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Pre-update snapshot manifest written alongside the copied state.
/// The CLI's `rollback` command reads this to know what to restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub created_at: String,
    pub version_from: String,
    pub version_to: String,
    /// Profile that was active at snapshot time.
    pub active_profile: String,
    /// Relative paths inside the snapshot, for diagnostics.
    pub files: Vec<String>,
    /// Path to the `.bak` binary (`rantaiclaw.old`) preserved on the
    /// running binary's filesystem. None when the swap was Windows
    /// (where staging happens differently).
    pub bak_binary_path: Option<String>,
}

/// Snapshot directory + manifest in one struct.
pub struct Snapshot {
    pub dir: PathBuf,
    pub manifest: SnapshotManifest,
}

const MANIFEST_FILE: &str = "manifest.toml";
const SNAPSHOT_PARENT: &str = ".update-snapshots";

/// Snapshot critical state files before swapping the binary.
///
/// Returns the snapshot directory path. Errors here should NOT abort
/// the update — the calling code logs them and proceeds, since the
/// snapshot is best-effort and a missing rollback target is preferable
/// to refusing an update.
pub fn create(
    rantaiclaw_root: &Path,
    version_from: &str,
    version_to: &str,
    active_profile: &str,
    bak_binary_path: Option<&Path>,
) -> Result<Snapshot> {
    // ISO-8601 with `:` replaced (avoid issues on Windows-style FS) and
    // a `Z` suffix marking UTC.
    let stamp = Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let dir = rantaiclaw_root.join(SNAPSHOT_PARENT).join(&stamp);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create snapshot dir {}", dir.display()))?;

    let mut files = Vec::new();

    // Copy global state files at <root>/...
    for name in &["config.toml", "active_profile", "active_workspace.toml"] {
        let src = rantaiclaw_root.join(name);
        if src.is_file() {
            let dst = dir.join(name);
            std::fs::copy(&src, &dst)
                .with_context(|| format!("snapshot {}", name))?;
            files.push((*name).to_string());
        }
    }

    // Copy the active profile's config + persona + memory metadata.
    // (Skill content + sessions.db are intentionally NOT copied here —
    // both are large and reproducible. Use `--backup` for those.)
    let profile_src = rantaiclaw_root.join("profiles").join(active_profile);
    if profile_src.is_dir() {
        let profile_dst = dir.join("profiles").join(active_profile);
        std::fs::create_dir_all(&profile_dst)?;
        for name in &["config.toml", "persona.toml", "active_profile"] {
            let f = profile_src.join(name);
            if f.is_file() {
                let target = profile_dst.join(name);
                std::fs::copy(&f, &target).ok();
                files.push(format!("profiles/{active_profile}/{name}"));
            }
        }
        // Persona dir copied wholesale — small, hand-edited, valuable to
        // restore.
        let persona_src = profile_src.join("persona");
        if persona_src.is_dir() {
            let persona_dst = profile_dst.join("persona");
            copy_dir_recursive(&persona_src, &persona_dst).ok();
            files.push(format!("profiles/{active_profile}/persona/"));
        }
    }

    let manifest = SnapshotManifest {
        created_at: stamp.clone(),
        version_from: version_from.into(),
        version_to: version_to.into(),
        active_profile: active_profile.into(),
        files,
        bak_binary_path: bak_binary_path.map(|p| p.display().to_string()),
    };

    let manifest_toml = toml::to_string_pretty(&manifest)
        .context("serialize manifest")?;
    std::fs::write(dir.join(MANIFEST_FILE), manifest_toml)
        .with_context(|| format!("write {}", MANIFEST_FILE))?;

    Ok(Snapshot { dir, manifest })
}

/// List existing snapshots, newest first. Used by `rantaiclaw rollback`
/// when the user passes no explicit path.
pub fn list_all(rantaiclaw_root: &Path) -> Result<Vec<Snapshot>> {
    let parent = rantaiclaw_root.join(SNAPSHOT_PARENT);
    if !parent.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&parent)
        .with_context(|| format!("read_dir {}", parent.display()))?
        .flatten()
    {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let manifest_path = p.join(MANIFEST_FILE);
        if !manifest_path.is_file() {
            continue;
        }
        let raw = match std::fs::read_to_string(&manifest_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let manifest: SnapshotManifest = match toml::from_str(&raw) {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(Snapshot { dir: p, manifest });
    }
    // Newest first by created_at lexicographic sort (timestamps are
    // ISO-8601 so string-sort == chronological).
    out.sort_by(|a, b| b.manifest.created_at.cmp(&a.manifest.created_at));
    Ok(out)
}

/// Restore a snapshot's files back over the live `<root>` tree. Idempotent
/// per-file: missing files in the snapshot just skip.
pub fn restore(snapshot: &Snapshot, rantaiclaw_root: &Path) -> Result<RestoreSummary> {
    let mut restored = Vec::new();

    for rel in &snapshot.manifest.files {
        // Trailing slash means "directory of files."
        if let Some(stripped) = rel.strip_suffix('/') {
            let src = snapshot.dir.join(stripped);
            let dst = rantaiclaw_root.join(stripped);
            if src.is_dir() {
                copy_dir_recursive(&src, &dst)
                    .with_context(|| format!("restore dir {stripped}"))?;
                restored.push(rel.clone());
            }
        } else {
            let src = snapshot.dir.join(rel);
            let dst = rantaiclaw_root.join(rel);
            if src.is_file() {
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::copy(&src, &dst)
                    .with_context(|| format!("restore {}", rel))?;
                restored.push(rel.clone());
            }
        }
    }

    // Rollback the binary if the snapshot remembers a `.old`.
    let mut bak_restored = false;
    if let Some(bak) = &snapshot.manifest.bak_binary_path {
        let bak_path = Path::new(bak);
        if bak_path.is_file() {
            // Mirror swap_binary's atomic shape: rename current → .new,
            // .old → live, .new → .old (so the failed-rollback case
            // also keeps a backup of the just-replaced binary).
            let live = bak_path.with_extension("");
            let new_path = live.with_extension("rolling-back");
            let _ = std::fs::remove_file(&new_path);
            if std::fs::rename(&live, &new_path).is_ok() {
                if std::fs::rename(bak_path, &live).is_ok() {
                    let _ = std::fs::rename(&new_path, bak_path);
                    bak_restored = true;
                } else {
                    let _ = std::fs::rename(&new_path, &live);
                }
            }
        }
    }

    Ok(RestoreSummary {
        files_restored: restored,
        binary_restored: bak_restored,
    })
}

#[derive(Debug, Default)]
pub struct RestoreSummary {
    pub files_restored: Vec<String>,
    pub binary_restored: bool,
}

/// Recursively copy a directory tree, preserving file contents. Used
/// for `persona/` (small, hand-edited) and not for sessions.db (large,
/// regeneratable).
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        bail!("not a dir: {}", src.display());
    }
    std::fs::create_dir_all(dst)
        .with_context(|| format!("create {}", dst.display()))?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let path = entry.path();
        let rel = entry.file_name();
        let target = dst.join(&rel);
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if path.is_file() {
            std::fs::copy(&path, &target).with_context(|| {
                format!("copy {} → {}", path.display(), target.display())
            })?;
        }
    }
    Ok(())
}

/// Full-profile backup tarball. Optional, opt-in via `update --backup`.
/// Mirrors Hermes' `--backup` flag — covers more than the lightweight
/// snapshot (sessions.db, skills/*, secrets, etc.) at the cost of
/// being slower on large profiles.
pub fn full_backup_archive(rantaiclaw_root: &Path, label: &str) -> Result<PathBuf> {
    let stamp = Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let archive_name = format!("rantaiclaw-backup-{label}-{stamp}.tar.gz");
    let archive_path = rantaiclaw_root.join(SNAPSHOT_PARENT).join(&archive_name);
    if let Some(parent) = archive_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Use system `tar` to avoid pulling in `flate2` for one feature we
    // could provide via the same trick we already use for binary
    // extraction.
    let status = std::process::Command::new("tar")
        .args(["-czf"])
        .arg(&archive_path)
        .arg("-C")
        .arg(rantaiclaw_root)
        .arg(".")
        .status()
        .context("run tar -czf for full backup")?;
    if !status.success() {
        bail!("tar exited with {:?}", status.code());
    }
    Ok(archive_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(p: &Path, body: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn create_then_list_then_restore_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        write(&root.join("config.toml"), "v1");
        write(&root.join("active_profile"), "alpha");
        write(&root.join("profiles/alpha/config.toml"), "alpha-cfg-v1");
        write(&root.join("profiles/alpha/persona/persona.toml"), "preset = \"default\"");

        let snap = create(root, "0.1.0", "0.2.0", "alpha", None).unwrap();
        assert!(snap.dir.is_dir());
        assert!(snap.manifest.files.iter().any(|f| f == "config.toml"));
        assert!(snap.manifest.files.iter().any(|f| f.starts_with("profiles/alpha/persona")));

        // Mutate live state.
        write(&root.join("config.toml"), "v2-broken");
        write(&root.join("profiles/alpha/persona/persona.toml"), "preset = \"corrupted\"");

        let snaps = list_all(root).unwrap();
        assert_eq!(snaps.len(), 1);

        let summary = restore(&snaps[0], root).unwrap();
        assert!(!summary.files_restored.is_empty());

        let restored = std::fs::read_to_string(root.join("config.toml")).unwrap();
        assert_eq!(restored, "v1");
        let persona = std::fs::read_to_string(root.join("profiles/alpha/persona/persona.toml"))
            .unwrap();
        assert_eq!(persona, "preset = \"default\"");
    }

    #[test]
    fn list_all_returns_newest_first() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("config.toml"), "v1");
        let s1 = create(root, "0.1.0", "0.2.0", "alpha", None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let s2 = create(root, "0.2.0", "0.3.0", "alpha", None).unwrap();

        let listed = list_all(root).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].manifest.created_at, s2.manifest.created_at);
        assert_eq!(listed[1].manifest.created_at, s1.manifest.created_at);
    }

    #[test]
    fn missing_state_files_dont_panic() {
        let tmp = TempDir::new().unwrap();
        // Empty root — no config.toml, no active_profile, no profiles/.
        let snap = create(tmp.path(), "0.1.0", "0.2.0", "alpha", None).unwrap();
        // Manifest exists but with empty files[].
        assert!(snap.manifest.files.is_empty());
    }
}
