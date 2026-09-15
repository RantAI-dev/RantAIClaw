//! WhatsApp Web session file housekeeping.
//!
//! Plan 367. The first version of the plan moved session files from the
//! `DELETE /api/v1/channels/whatsapp_web` handler. That stopped on its own
//! first STOP condition (recorded in `plans/364`): a channel listener holds
//! `conn: Arc<Mutex<Connection>>` for the whole life of the channel
//! (`whatsapp_web.rs:923`, `whatsapp_storage.rs:89-96`), nothing closes it
//! when config changes, and on Linux a rename under an open descriptor
//! succeeds and the listener keeps writing into the set-aside copy.
//!
//! This module replaces that with two pieces:
//!
//! 1. A pure [`set_aside_unreferenced_sessions`] that walks the workspace and
//!    moves unreferenced WhatsApp Web session files into `workspace/.unlinked/`
//!    under timestamped names, *only* on a runtime start, before any channel
//!    listener can open them. The listener opens whatever `session_path` says
//!    (`whatsapp_web.rs:923`), so a fresh link gets a fresh file and an old
//!    file that the config no longer references is moved before any process
//!    has a handle on it.
//!
//! 2. The gateway's `DELETE` handler stays config-only: it clears the
//!    `channels_config.whatsapp_web` section and schedules a reload. The
//!    set-aside step runs at the next `build_channel_runtime`, after the
//!    `config.toml` no longer references the old session, and before any
//!    listener opens anything.
//!
//! Pairing happens against a new session file the gateway mints on each
//! `POST /pair`, so two clients never hold one session — plan 364 D-3.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Returned by [`set_aside_unreferenced_sessions`] so callers and tests can
/// distinguish "moved something" from "no unreferenced sessions, no-op".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SetAsideReport {
    /// Number of unreferenced session base files moved. Each base file
    /// counts as one; the `-wal` and `-shm` companions ride along.
    pub moved: usize,
}

/// A WhatsApp Web session base filename the set-aside step recognises.
///
/// The listener opens `WhatsAppWebConfig.session_path` exactly as written
/// (`whatsapp_web.rs:923`); the on-disk provisioner defaults to
/// `whatsapp.db` (`onboard/provision/whatsapp_web.rs:64`). The console's
/// pairing endpoint mints `whatsapp-<unix-seconds>.db` per link. So the
/// pattern `whatsapp*.db` covers the legacy default, the mint, and any
/// future variant without making the rule broader than it has to be.
fn is_session_base_name(name: &str) -> bool {
    name.starts_with("whatsapp") && name.ends_with(".db")
}

/// The SQLite WAL-mode companions for a base session file. SQLite creates
/// these automatically; they are part of the session and must travel with
/// the base. Anything else that happens to live next to `whatsapp.db`
/// (operator notes, manual backups) is left alone.
fn companion_names(base: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(2);
    let wal = format!("{base}-wal");
    let shm = format!("{base}-shm");
    if !out.contains(&wal) {
        out.push(wal);
    }
    if !out.contains(&shm) {
        out.push(shm);
    }
    out
}

/// Pure: list the on-disk filenames the set-aside step would touch in
/// `workspace_dir`, given the set of currently-referenced base filenames
/// (already canonicalised relative to `workspace_dir`). Exposed so tests
/// can assert behaviour without going through the filesystem, and so the
/// `build_channel_runtime` call site can reason about what it just moved.
pub fn plan_set_aside(workspace_dir: &Path, referenced_bases: &[String]) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(workspace_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut to_move: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_session_base_name(name) {
            continue;
        }
        if referenced_bases.iter().any(|b| b == name) {
            continue;
        }
        to_move.push(path);
    }
    to_move
}

/// Move WhatsApp Web session files in `workspace_dir` that no current config
/// references into `workspace_dir/.unlinked/`, timestamped. Returns the count
/// of base files moved. Never deletes. Safe to call when `.unlinked/` does
/// not exist yet. A second call with the same inputs is a no-op.
///
/// The `.unlinked/` directory is created with mode 0o700 if missing; the
/// moved files keep their mode. Failure to move a single file is logged
/// and skipped so one bad file does not stop the rest, but if the
/// `.unlinked/` directory cannot be created the function returns the error
/// — the operator has to know the runtime could not put unreferenced
/// sessions somewhere safe.
pub fn set_aside_unreferenced_sessions(
    workspace_dir: &Path,
    referenced_bases: &[String],
) -> anyhow::Result<SetAsideReport> {
    let unlinked_dir = workspace_dir.join(".unlinked");
    if !unlinked_dir.exists() {
        std::fs::create_dir_all(&unlinked_dir)
            .map_err(|e| anyhow::anyhow!("could not create {}: {e}", unlinked_dir.display()))?;
    }

    let unix_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut report = SetAsideReport::default();
    for base_path in plan_set_aside(workspace_dir, referenced_bases) {
        let Some(base_name) = base_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let target_name = format!("{unix_now}-{base_name}");
        let target = unlinked_dir.join(&target_name);
        if let Err(e) = move_with_companions(&base_path, base_name, &target) {
            tracing::warn!(
                base = %base_path.display(),
                target = %target.display(),
                error = %e,
                "could not set aside WhatsApp Web session file; leaving in place"
            );
            continue;
        }
        report.moved += 1;
    }
    Ok(report)
}

/// Move `base_path` and any `-wal` / `-shm` companion into `target`. Each
/// sibling is moved independently so a missing companion is not an error.
///
/// A companion moves to `{target file name}{suffix}`, where the suffix is what
/// `companion_names` appended to the base (`-wal` or `-shm`). So every name in
/// `.unlinked/` is the original name with one `<unix>-` prefix, and restoring a
/// session means moving the files back and dropping that prefix: SQLite finds
/// the WAL beside the base only under the base's name plus `-wal`.
fn move_with_companions(base_path: &Path, base_name: &str, target: &Path) -> anyhow::Result<()> {
    std::fs::rename(base_path, target).map_err(|e| {
        anyhow::anyhow!(
            "rename {} -> {}: {e}",
            base_path.display(),
            target.display()
        )
    })?;
    let target_parent = target.parent().unwrap_or_else(|| Path::new("."));
    for sibling_name in companion_names(base_name) {
        let sibling = base_path.with_file_name(&sibling_name);
        if !sibling.exists() {
            continue;
        }
        // `companion_names` builds every name as the base plus a suffix, so
        // this always matches. A name that did not would stay where it is
        // rather than be moved under a name SQLite cannot pair with its base.
        let Some(suffix) = sibling_name.strip_prefix(base_name) else {
            continue;
        };
        let sibling_target_name = format!(
            "{}{suffix}",
            target.file_name().and_then(|n| n.to_str()).unwrap_or("")
        );
        let sibling_target = target_parent.join(sibling_target_name);
        if let Err(e) = std::fs::rename(&sibling, &sibling_target) {
            tracing::warn!(
                sibling = %sibling.display(),
                error = %e,
                "could not move WhatsApp Web session companion; left in place"
            );
        }
    }
    Ok(())
}

/// Resolve `session_path` from the config to a base filename relative to
/// `workspace_dir`. The config may store an absolute path or one relative
/// to `workspace_dir`; the set-aside step matches by base filename only
/// because that is the only stable identity for a session file (the path
/// can move, the base filename does not).
pub fn session_base_name(workspace_dir: &Path, session_path: &Path) -> Option<String> {
    let base = session_path.file_name()?.to_str()?;
    if !is_session_base_name(base) {
        return None;
    }
    // Anchor the base under workspace_dir so a `session_path` that lives
    // elsewhere cannot accidentally match an unreferenced file.
    let absolute_or_relative = if session_path.is_absolute() {
        session_path.to_path_buf()
    } else {
        workspace_dir.join(session_path)
    };
    if !absolute_or_relative.starts_with(workspace_dir) {
        return None;
    }
    Some(base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn unique_tmp(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "rantaiclaw-whatsapp-set-aside-{label}-{nanos}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn is_session_base_name_matches_whatsapp_db_and_minted() {
        assert!(is_session_base_name("whatsapp.db"));
        assert!(is_session_base_name("whatsapp-1700000000.db"));
        assert!(!is_session_base_name("whatsapp.db-wal"));
        assert!(!is_session_base_name("notes-whatsapp.db"));
        assert!(!is_session_base_name("foo.db"));
        assert!(!is_session_base_name(""));
    }

    #[test]
    fn companion_names_lists_wal_and_shm() {
        let mut names = companion_names("whatsapp.db");
        names.sort();
        assert_eq!(names, vec!["whatsapp.db-shm", "whatsapp.db-wal"]);
    }

    #[test]
    fn plan_set_aside_returns_only_unreferenced_whatsapp_dbs() {
        let ws = unique_tmp("plan");
        fs::write(ws.join("whatsapp.db"), b"old").unwrap();
        fs::write(ws.join("whatsapp-111.db"), b"link-1").unwrap();
        fs::write(ws.join("whatsapp-222.db"), b"link-2").unwrap();
        fs::write(ws.join("notes.txt"), b"keep me").unwrap();
        fs::write(ws.join("whatsapp.db-wal"), b"wal").unwrap();

        let mut plan = plan_set_aside(&ws, &[]);
        plan.sort();
        let names: Vec<String> = plan
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["whatsapp-111.db", "whatsapp-222.db", "whatsapp.db"]
        );

        // Referenced one stays.
        let plan = plan_set_aside(&ws, &["whatsapp.db".to_string()]);
        let names: Vec<String> = plan
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["whatsapp-111.db", "whatsapp-222.db"]);

        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn set_aside_unreferenced_sessions_moves_bases_and_companions() {
        let ws = unique_tmp("move");
        fs::write(ws.join("whatsapp.db"), b"old").unwrap();
        fs::write(ws.join("whatsapp.db-wal"), b"wal").unwrap();
        fs::write(ws.join("whatsapp.db-shm"), b"shm").unwrap();
        fs::write(ws.join("whatsapp-100.db"), b"keep").unwrap();

        let report = set_aside_unreferenced_sessions(&ws, &["whatsapp-100.db".to_string()])
            .expect("set-aside must succeed");

        assert_eq!(report.moved, 1, "only one base file was unreferenced");
        assert!(!ws.join("whatsapp.db").exists(), "base moved");
        assert!(!ws.join("whatsapp.db-wal").exists(), "wal moved");
        assert!(!ws.join("whatsapp.db-shm").exists(), "shm moved");
        assert!(ws.join("whatsapp-100.db").exists(), "referenced base kept");

        let unlinked = ws.join(".unlinked");
        assert!(unlinked.is_dir(), ".unlinked created");
        let entries: Vec<String> = fs::read_dir(&unlinked)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        // The exact set, not "some entry contains wal": the old check passed
        // while every companion carried the base name twice
        // (`<unix>-whatsapp.db-whatsapp.db-wal`), which no restore can use.
        let (prefixes, originals) = split_unlinked_names(&entries);
        assert_eq!(
            originals,
            vec!["whatsapp.db", "whatsapp.db-shm", "whatsapp.db-wal"],
            "each file keeps its original name behind one `<unix>-` prefix: {entries:?}"
        );
        assert_eq!(
            prefixes.len(),
            1,
            "a base and its companions share one prefix: {entries:?}"
        );

        fs::remove_dir_all(&ws).unwrap();
    }

    /// Split every `.unlinked/` name into its `<unix>` prefix and the original
    /// name. Splits on the FIRST dash: the prefix is digits only, while a
    /// console-minted base (`whatsapp-<unix>.db`) has a dash of its own.
    fn split_unlinked_names(entries: &[String]) -> (Vec<String>, Vec<String>) {
        let mut prefixes = Vec::new();
        let mut originals = Vec::new();
        for entry in entries {
            let (prefix, original) = entry
                .split_once('-')
                .unwrap_or_else(|| panic!("`{entry}` has no `<unix>-` prefix"));
            assert!(
                !prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_digit()),
                "`{entry}` does not start with a unix timestamp"
            );
            if !prefixes.iter().any(|p| p == prefix) {
                prefixes.push(prefix.to_string());
            }
            originals.push(original.to_string());
        }
        originals.sort();
        (prefixes, originals)
    }

    /// F-36. Restoring a set-aside session means moving its files back and
    /// dropping the timestamp. That only works if stripping the `<unix>-`
    /// prefix from every entry gives back exactly the names that were in the
    /// workspace; the WAL of the owner's session held most of it, and under its
    /// old name a restored `.db` would have opened without it.
    #[test]
    fn stripping_the_prefix_from_set_aside_files_gives_back_the_workspace_names() {
        let ws = unique_tmp("restore");
        let originals = [
            "whatsapp-1789455121.db",
            "whatsapp-1789455121.db-shm",
            "whatsapp-1789455121.db-wal",
        ];
        for name in originals {
            fs::write(ws.join(name), name.as_bytes()).unwrap();
        }

        let report = set_aside_unreferenced_sessions(&ws, &[]).expect("set-aside must succeed");
        assert_eq!(report.moved, 1, "one base, moved with its companions");

        let entries: Vec<String> = fs::read_dir(ws.join(".unlinked"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let (_, restored) = split_unlinked_names(&entries);
        assert_eq!(
            restored, originals,
            "stripping the prefix must give back the workspace names: {entries:?}"
        );

        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn set_aside_is_a_noop_when_nothing_unreferenced() {
        let ws = unique_tmp("noop");
        fs::write(ws.join("whatsapp.db"), b"keep").unwrap();
        let report = set_aside_unreferenced_sessions(&ws, &["whatsapp.db".to_string()]).unwrap();
        assert_eq!(report.moved, 0);
        assert!(ws.join("whatsapp.db").exists());
        // The .unlinked directory exists (we created it) but is empty.
        let unlinked = ws.join(".unlinked");
        let count = fs::read_dir(&unlinked).unwrap().count();
        assert_eq!(count, 0);
        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn set_aside_does_not_touch_files_outside_workspace() {
        let parent = unique_tmp("outside");
        let ws = parent.join("ws");
        fs::create_dir_all(&ws).unwrap();
        // A sibling workspace outside `ws` must not be touched.
        let other = parent.join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("whatsapp.db"), b"untouched").unwrap();
        fs::write(ws.join("whatsapp-9.db"), b"move me").unwrap();

        let report = set_aside_unreferenced_sessions(&ws, &[]).unwrap();
        assert_eq!(report.moved, 1);
        assert!(other.join("whatsapp.db").exists(), "other kept");
        assert!(!ws.join("whatsapp-9.db").exists(), "ws one moved");

        fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn set_aside_keeps_unrelated_files() {
        let ws = unique_tmp("unrelated");
        fs::write(ws.join("whatsapp-1.db"), b"move").unwrap();
        fs::write(ws.join("operator-notes.txt"), b"keep").unwrap();
        fs::write(ws.join("config.toml"), b"keep").unwrap();

        let report = set_aside_unreferenced_sessions(&ws, &[]).unwrap();
        assert_eq!(report.moved, 1);
        assert!(ws.join("operator-notes.txt").exists());
        assert!(ws.join("config.toml").exists());

        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn second_set_aside_is_noop() {
        let ws = unique_tmp("second");
        fs::write(ws.join("whatsapp.db"), b"old").unwrap();
        let r1 = set_aside_unreferenced_sessions(&ws, &[]).unwrap();
        assert_eq!(r1.moved, 1);
        let r2 = set_aside_unreferenced_sessions(&ws, &[]).unwrap();
        assert_eq!(r2.moved, 0);
        fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn session_base_name_filters_non_whatsapp_paths() {
        let ws = Path::new("/tmp");
        assert_eq!(
            session_base_name(ws, &PathBuf::from("whatsapp.db")),
            Some("whatsapp.db".to_string())
        );
        assert_eq!(
            session_base_name(ws, &PathBuf::from("whatsapp-123.db")),
            Some("whatsapp-123.db".to_string())
        );
        assert_eq!(session_base_name(ws, &PathBuf::from("notes.txt")), None);
    }
}
