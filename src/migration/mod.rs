//! Markdown memory import glue for the v34 config migration.
//!
//! These functions back the one-time markdown → sqlite import that
//! `src/config/schema.rs` runs when a config that still names the retired
//! `markdown` backend is loaded under the current schema. `Config::load_or_init`
//! calls [`backup_markdown_memory`] directly and leaves the import to
//! `retry_unimported_markdown_imports`; `import_markdown_memory_into_sqlite`,
//! which does both in one call, exists for tests only. The legacy
//! external-import path (`pub mod openclaw`) was removed together with the
//! `rantaiclaw migrate` command. Do not reintroduce that command without also
//! reintroducing this module's former siblings. The markdown backend
//! retirement is what made this glue necessary.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::memory::MemoryCategory;

/// One row of memory recovered from a source workspace. The markdown reader
/// shares this shape with the v34 importer's expectations.
#[derive(Debug, Clone)]
pub(crate) struct SourceEntry {
    pub(crate) key: String,
    pub(crate) content: String,
    pub(crate) category: MemoryCategory,
}

/// Read every entry from a workspace's markdown files.
///
/// When a key appears in several files the import keeps the last one read, so
/// the order decides which value wins: the daily files come first, in name
/// order, and `MEMORY.md`, the curated file, comes last.
pub(crate) fn read_openclaw_markdown_entries(source_workspace: &Path) -> Result<Vec<SourceEntry>> {
    let mut all = Vec::new();

    let daily_dir = source_workspace.join("memory");
    if daily_dir.exists() {
        let mut listed = Vec::new();
        for file in fs::read_dir(&daily_dir)? {
            listed.push(file?.path());
        }
        for path in sorted_daily_markdown_files(listed) {
            let content = fs::read_to_string(&path)?;
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("openclaw_daily");
            all.extend(parse_markdown_file(
                &path,
                &content,
                MemoryCategory::Daily,
                stem,
            ));
        }
    }

    let core_path = source_workspace.join("MEMORY.md");
    if core_path.exists() {
        let content = fs::read_to_string(&core_path)?;
        all.extend(parse_markdown_file(
            &core_path,
            &content,
            MemoryCategory::Core,
            "openclaw_core",
        ));
    }

    Ok(all)
}

/// The `*.md` files among `paths`, in name order. `read_dir` order is not
/// guaranteed, so the caller's order never decides which daily file wins.
fn sorted_daily_markdown_files(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = paths
        .into_iter()
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md"))
        .collect();
    files.sort();
    files
}

/// The entries one markdown file holds, without the projection block.
pub(crate) fn read_markdown_file_entries(path: &Path) -> Result<Vec<SourceEntry>> {
    let content = fs::read_to_string(path)?;
    Ok(parse_markdown_file(
        path,
        &content,
        MemoryCategory::Core,
        "live",
    ))
}

#[allow(clippy::needless_pass_by_value)]
fn parse_markdown_file(
    _path: &Path,
    content: &str,
    default_category: MemoryCategory,
    stem: &str,
) -> Vec<SourceEntry> {
    use crate::memory::snapshot::{PROJECTION_BEGIN, PROJECTION_END};

    let mut entries = Vec::new();
    // A retry re-reads the frozen backup, which normally predates any
    // projection. Skipping the markers and everything between them anyway
    // guards a re-run against importing its own generated block as if it
    // were operator content, which is how a past bug made `MEMORY.md` grow
    // with every retry.
    let mut in_projection = false;

    for (idx, raw_line) in content.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed == PROJECTION_BEGIN {
            in_projection = true;
            continue;
        }
        if trimmed == PROJECTION_END {
            in_projection = false;
            continue;
        }
        if in_projection {
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let line = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        let (key, text) = match parse_structured_memory_line(line) {
            Some((k, v)) => (normalize_key(k, idx), v.trim().to_string()),
            None => (
                format!("openclaw_{stem}_{}", idx + 1),
                line.trim().to_string(),
            ),
        };

        if text.is_empty() {
            continue;
        }

        entries.push(SourceEntry {
            key,
            content: text,
            category: default_category.clone(),
        });
    }

    entries
}

fn parse_structured_memory_line(line: &str) -> Option<(&str, &str)> {
    if !line.starts_with("**") {
        return None;
    }

    let rest = line.strip_prefix("**")?;
    let key_end = rest.find("**:")?;
    let key = rest.get(..key_end)?.trim();
    let value = rest.get(key_end + 3..)?.trim();

    if key.is_empty() || value.is_empty() {
        return None;
    }

    Some((key, value))
}

fn normalize_key(key: &str, fallback_idx: usize) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return format!("openclaw_{fallback_idx}");
    }
    trimmed.to_string()
}

/// Name of the marker that records a markdown backup is still owed.
const PENDING_MARKER: &str = "PENDING";

/// Where the marker of an owed markdown backup lives.
///
/// The marker sits inside `memory/migrations/` so that an install that never
/// used markdown, which has no such directory, pays one failed directory read
/// per load and nothing more.
pub(crate) fn pending_markdown_import_marker(workspace_dir: &Path) -> PathBuf {
    workspace_dir
        .join("memory")
        .join("migrations")
        .join(PENDING_MARKER)
}

/// Record that a markdown backup was attempted and failed, so a later load
/// retries the backup and the import even when the config on disk no longer
/// names `markdown` (a save in the failed session writes the migrated config).
///
/// The marker holds the time of the first failure. A marker that already
/// exists is left alone: rows the runtime stored after the first failure must
/// stay newer than every backup made for this import, however many attempts
/// it takes.
pub(crate) fn record_pending_markdown_import(workspace_dir: &Path) -> Result<()> {
    let marker = pending_markdown_import_marker(workspace_dir);
    let dir = marker
        .parent()
        .context("pending marker path has no parent directory")?;
    fs::create_dir_all(dir)?;
    let created = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker);
    match created {
        Ok(mut file) => {
            file.write_all(Utc::now().to_rfc3339().as_bytes())?;
            file.sync_all()?;
            sync_dir(dir)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The instant a marker file records: the RFC 3339 time it holds, or, for a
/// marker written empty by an earlier build, its modification time.
pub(crate) fn marker_time(marker: &Path) -> Result<DateTime<Utc>> {
    let text =
        fs::read_to_string(marker).with_context(|| format!("read marker {}", marker.display()))?;
    if let Ok(time) = DateTime::parse_from_rfc3339(text.trim()) {
        return Ok(time.with_timezone(&Utc));
    }
    let modified = fs::metadata(marker)
        .and_then(|meta| meta.modified())
        .with_context(|| format!("read the modification time of {}", marker.display()))?;
    Ok(modified.into())
}

/// Write `contents` to `path` and flush the file and its directory entry to
/// disk before returning, so a marker never outlives the data it vouches for.
pub(crate) fn write_marker_durably(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    if let Some(dir) = path.parent() {
        sync_dir(dir)?;
    }
    Ok(())
}

/// Replace `path` with `contents` through a temp file in the same directory
/// and a rename, so a crash leaves either the old file or the new one.
///
/// A symlink at `path` is resolved first, so the link stays a link and its
/// target is replaced. The replacement keeps the mode of the file it
/// replaces.
pub(crate) fn replace_file_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let resolved;
    let path = if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        resolved = fs::canonicalize(path)
            .with_context(|| format!("resolve symlink {}", path.display()))?;
        resolved.as_path()
    } else {
        path
    };
    let dir = path
        .parent()
        .context("path to replace has no parent directory")?;
    let name = path
        .file_name()
        .context("path to replace has no file name")?
        .to_string_lossy();
    let temp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = fs::File::create(&temp)?;
        if let Ok(existing) = fs::metadata(path) {
            fs::set_permissions(&temp, existing.permissions())?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Flush a regular file to disk. The handle is opened for writing because
/// Windows turns `sync_all` into `FlushFileBuffers`, which fails with "Access
/// is denied" on a handle opened read-only.
fn sync_file(path: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("fsync {}", path.display()))
}

/// Treat the two errors a filesystem gives when it cannot fsync a directory as
/// success, as SQLite does. The entry is then as durable as that filesystem
/// makes it, and failing would leave a finished marker looking like a failure.
#[cfg(unix)]
fn accept_unsupported_dir_fsync(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(e) if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOTSUP)) => Ok(()),
        other => other,
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<()> {
    accept_unsupported_dir_fsync(fs::File::open(dir).and_then(|handle| handle.sync_all()))
        .with_context(|| format!("fsync {}", dir.display()))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Back up the markdown memory files before the v34 import rewrites `MEMORY.md`.
///
/// The backup mirrors the live workspace layout under a
/// `markdown-<timestamp>-<pid>` directory: `MEMORY.md` at its root and every
/// `*.md` in `memory/` (non-recursive, so `memory/archive/` is left alone)
/// under its own `memory/` subdirectory. Mirroring the layout lets
/// `read_openclaw_markdown_entries` read the backup directory exactly as it
/// would the live workspace, so a retry re-reads the frozen originals rather
/// than whatever the live files have become since.
///
/// Also copies `memory/brain.db`, if one exists, into the same `memory/`
/// subdirectory (see [`vacuum_brain_db_into`]): the import overwrites a
/// conflicting key with the markdown value (the operator's live value), so
/// the pre-import sqlite state is recoverable only from this copy.
///
/// The PID in the directory name keeps two processes that both start a
/// backup in the same second (the daemon and a CLI command, say) from ever
/// writing into the same directory. A `BACKUP_COMPLETE` marker file, written
/// last, is what tells `retry_unimported_markdown_imports` this backup is
/// whole: a copy that fails partway (a daily file, `ENOSPC`, the `VACUUM`)
/// leaves a directory without that marker, which the sweep then leaves
/// alone rather than importing from a partial state. The same absence is
/// why a flat, pre-marker backup from an earlier development build is never
/// picked up either — it has neither this marker nor the `memory/`
/// subdirectory layout. Every copy and the directories are flushed to disk
/// before the marker is written.
///
/// The marker holds an RFC 3339 time, the cutoff for the import: a row whose
/// `updated_at` is later than it was stored after the backup, so the import
/// leaves it alone. The cutoff is the time the backup started, taken before
/// anything is copied, because a row written while the copy runs may or may
/// not be in the `brain.db` snapshot. When a `PENDING` marker exists the
/// cutoff is the earlier time it holds, because the runtime may have stored
/// rows between the failed attempt and this one.
///
/// A call that fails, or finds nothing to copy, removes the directory it
/// created, so a failed load leaves no partial directory behind.
pub(crate) fn backup_markdown_memory(workspace_dir: &Path) -> Result<Option<PathBuf>> {
    backup_markdown_memory_started_at(workspace_dir, Utc::now())
}

/// The backup directory name is derived from the start time, so a caller that
/// fixes the start time also fixes the name.
fn backup_dir_name(started: DateTime<Utc>) -> String {
    let timestamp = started
        .with_timezone(&chrono::Local)
        .format("%Y%m%d-%H%M%S");
    format!("markdown-{timestamp}-{}", std::process::id())
}

fn backup_markdown_memory_started_at(
    workspace_dir: &Path,
    started: DateTime<Utc>,
) -> Result<Option<PathBuf>> {
    let pending_marker = pending_markdown_import_marker(workspace_dir);
    let cutoff = if pending_marker.exists() {
        marker_time(&pending_marker)?.min(started)
    } else {
        started
    };

    let backup_root = workspace_dir
        .join("memory")
        .join("migrations")
        .join(backup_dir_name(started));

    // Only a directory this call created is this call's to remove; an earlier
    // backup that got the same name (same second, same process) is not.
    let created = !backup_root.exists();
    fs::create_dir_all(&backup_root)?;
    let backup = fill_backup_dir(workspace_dir, &backup_root, cutoff);
    if created && !matches!(backup, Ok(Some(_))) {
        if let Err(e) = fs::remove_dir_all(&backup_root) {
            // Path and error only: no note content.
            tracing::warn!(
                error = %format!("{e:#}"),
                dir = %backup_root.display(),
                "failed to remove the partial markdown memory backup directory"
            );
        }
    }
    backup
}

/// Copy the markdown files (and `brain.db`) into `backup_root` and write the
/// completeness marker last. `Ok(None)` means there was nothing to copy.
fn fill_backup_dir(
    workspace_dir: &Path,
    backup_root: &Path,
    cutoff: DateTime<Utc>,
) -> Result<Option<PathBuf>> {
    let migrations_dir = backup_root
        .parent()
        .context("backup directory has no parent")?;
    let backup_memory_dir = backup_root.join("memory");

    let memory_md = workspace_dir.join("MEMORY.md");
    let mut copied_any = false;
    if memory_md.exists() {
        copy_file_durably(&memory_md, &backup_root.join("MEMORY.md"))?;
        copied_any = true;
    }

    let daily_dir = workspace_dir.join("memory");
    if daily_dir.exists() {
        for file in fs::read_dir(&daily_dir)? {
            let file = file?;
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            fs::create_dir_all(&backup_memory_dir)?;
            copy_file_durably(&path, &backup_memory_dir.join(name))?;
            copied_any = true;
        }
    }

    if !copied_any {
        return Ok(None);
    }

    let brain_db = daily_dir.join("brain.db");
    if brain_db.exists() {
        fs::create_dir_all(&backup_memory_dir)?;
        let dest = backup_memory_dir.join("brain.db");
        vacuum_brain_db_into(&brain_db, &dest)?;
        sync_file(&dest)?;
    }

    if backup_memory_dir.exists() {
        sync_dir(&backup_memory_dir)?;
    }
    sync_dir(backup_root)?;
    sync_dir(migrations_dir)?;

    // Written last, once every copy above has succeeded and reached the disk:
    // see the doc comment above for why the sweep depends on this.
    write_marker_durably(
        &backup_root.join("BACKUP_COMPLETE"),
        cutoff.to_rfc3339().as_bytes(),
    )?;

    Ok(Some(backup_root.to_path_buf()))
}

/// Copy `src` to `dest` and flush the copy through the handle that wrote it.
/// A second open for the flush would ask for write access, which a copy of a
/// read-only note does not grant.
///
/// On Unix the copy gets the mode of `src` when it is created, so it is never
/// wider than the note, not even for the moment before the first byte lands.
/// Elsewhere the mode is applied after the flush, because a read-only
/// attribute would block later writes.
fn copy_file_durably(src: &Path, dest: &Path) -> Result<()> {
    let mut input = fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let permissions = input.metadata()?.permissions();
    let mut output = create_copy_destination(dest, &permissions)
        .with_context(|| format!("create {}", dest.display()))?;
    std::io::copy(&mut input, &mut output)
        .with_context(|| format!("copy {} to {}", src.display(), dest.display()))?;
    output
        .sync_all()
        .with_context(|| format!("fsync {}", dest.display()))?;
    #[cfg(not(unix))]
    fs::set_permissions(dest, permissions)
        .with_context(|| format!("set the mode of {}", dest.display()))?;
    Ok(())
}

/// Create `dest` for writing with a mode no wider than `permissions`. The
/// creation mode is cut by the umask and does not apply to a file that already
/// exists, so the exact mode is set on the open handle before anything is
/// written to it.
#[cfg(unix)]
fn create_copy_destination(
    dest: &Path,
    permissions: &fs::Permissions,
) -> std::io::Result<fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(permissions.mode())
        .open(dest)?;
    file.set_permissions(permissions.clone())?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_copy_destination(
    dest: &Path,
    _permissions: &fs::Permissions,
) -> std::io::Result<fs::File> {
    fs::File::create(dest)
}

/// Copy `brain.db` into the backup with `VACUUM INTO`, not a plain file
/// copy.
///
/// `brain.db` runs in WAL mode, so recently committed rows can sit only in
/// the `-wal` file until sqlite checkpoints it into the main file; a plain
/// `fs::copy` of just the `.db` file — without its matching `-wal`/`-shm` —
/// can silently miss that data. `VACUUM INTO` reads a consistent snapshot of
/// the live logical database, WAL included, and writes it out whole to a
/// fresh file.
fn vacuum_brain_db_into(source: &Path, dest: &Path) -> Result<()> {
    // `VACUUM INTO` takes the destination as a SQL string; a lossy
    // conversion here would silently target the wrong file on a path with
    // invalid UTF-8, so reject it outright instead.
    let dest_str = dest.to_str().with_context(|| {
        format!(
            "backup destination path is not valid UTF-8: {}",
            dest.display()
        )
    })?;
    let conn = rusqlite::Connection::open(source)
        .with_context(|| format!("open {} for backup", source.display()))?;
    // A running daemon can hold brain.db for ordinary traffic; wait for it
    // rather than fail the backup with SQLITE_BUSY.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .context("set busy timeout for the backup")?;
    conn.execute("VACUUM INTO ?1", rusqlite::params![dest_str])
        .with_context(|| format!("vacuum {} into {}", source.display(), dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_structured_markdown_line() {
        let line = "**user_pref**: likes Rust";
        let parsed = parse_structured_memory_line(line).unwrap();
        assert_eq!(parsed.0, "user_pref");
        assert_eq!(parsed.1, "likes Rust");
    }

    #[test]
    fn parse_unstructured_markdown_generates_key() {
        let entries = parse_markdown_file(
            Path::new("/tmp/MEMORY.md"),
            "- plain note",
            MemoryCategory::Core,
            "core",
        );
        assert_eq!(entries.len(), 1);
        assert!(entries[0].key.starts_with("openclaw_core_"));
        assert_eq!(entries[0].content, "plain note");
    }

    #[test]
    fn normalize_key_handles_empty_string() {
        let key = normalize_key("", 42);
        assert_eq!(key, "openclaw_42");
    }

    #[test]
    fn normalize_key_trims_whitespace() {
        let key = normalize_key("  my_key  ", 0);
        assert_eq!(key, "my_key");
    }

    #[test]
    fn parse_structured_markdown_rejects_empty_key() {
        assert!(parse_structured_memory_line("****:value").is_none());
    }

    #[test]
    fn parse_structured_markdown_rejects_empty_value() {
        assert!(parse_structured_memory_line("**key**:").is_none());
    }

    #[test]
    fn parse_structured_markdown_rejects_no_stars() {
        assert!(parse_structured_memory_line("key: value").is_none());
    }

    fn backup_complete_time(backup_dir: &Path) -> String {
        fs::read_to_string(backup_dir.join("BACKUP_COMPLETE")).unwrap()
    }

    /// The clock is read once, before anything is copied. A marker that read it
    /// again when it was written would hold the finish time, and a row stored
    /// while the copy ran would then look older than the backup.
    #[test]
    fn backup_complete_marker_holds_the_start_time_it_was_given_not_the_finish_time() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("MEMORY.md"), "- **k**: v\n").unwrap();
        let started = "2001-02-03T04:05:06Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();

        let backup_dir = backup_markdown_memory_started_at(tmp.path(), started)
            .unwrap()
            .unwrap();

        let recorded =
            chrono::DateTime::parse_from_rfc3339(backup_complete_time(&backup_dir).trim())
                .expect("the marker holds an RFC 3339 time")
                .with_timezone(&chrono::Utc);
        assert_eq!(recorded, started);
    }

    /// `FlushFileBuffers` on Windows needs a handle that can write, so
    /// `sync_file` opens the file for writing. Linux lets a read-only handle
    /// fsync, so the probe is a file the caller cannot write: the open, and
    /// with it `sync_file`, must fail. Root ignores the mode bits, so the probe
    /// cannot run as root.
    #[cfg(unix)]
    #[test]
    fn sync_file_opens_the_file_with_write_access() {
        use std::os::unix::fs::PermissionsExt;

        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("note.md");
        fs::write(&path, "abc").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();

        assert!(
            sync_file(&path).is_err(),
            "sync_file must ask for write access to the file"
        );
    }

    #[test]
    fn sync_file_succeeds_while_another_handle_holds_the_file_read_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("note.md");
        fs::write(&path, "abc").unwrap();
        let _reader = fs::File::open(&path).unwrap();

        sync_file(&path).unwrap();
    }

    /// The copy must never be wider than the note at any moment, so the mode
    /// is applied when the file is created, before any byte is written. A file
    /// made with `File::create` starts at `0o666` minus the umask.
    #[cfg(unix)]
    #[test]
    fn a_copy_is_created_no_wider_than_the_note_it_copies() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let dest = tmp.path().join("copy.md");

        let copy = create_copy_destination(&dest, &fs::Permissions::from_mode(0o400)).unwrap();

        assert_eq!(copy.metadata().unwrap().permissions().mode() & 0o777, 0o400);
    }

    #[cfg(unix)]
    #[test]
    fn backup_keeps_the_owner_only_mode_of_a_note() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let note = tmp.path().join("MEMORY.md");
        fs::write(&note, "- **k**: v\n").unwrap();
        fs::set_permissions(&note, fs::Permissions::from_mode(0o600)).unwrap();

        let backup_dir = backup_markdown_memory(tmp.path()).unwrap().unwrap();

        assert_eq!(
            fs::metadata(backup_dir.join("MEMORY.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    /// A backup copy inherits the mode of the note it copies. A note the
    /// operator made read-only must still back up, so the copy is flushed
    /// through the handle that wrote it, not through a second open that asks
    /// for write access.
    #[cfg(unix)]
    #[test]
    fn backup_copies_a_read_only_note() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let note = tmp.path().join("MEMORY.md");
        fs::write(&note, "- **k**: v\n").unwrap();
        fs::set_permissions(&note, fs::Permissions::from_mode(0o444)).unwrap();

        let backup_dir = backup_markdown_memory(tmp.path()).unwrap().unwrap();

        let copy = backup_dir.join("MEMORY.md");
        assert_eq!(fs::read_to_string(&copy).unwrap(), "- **k**: v\n");
        assert_eq!(
            fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o444,
            "the copy keeps the mode of the note"
        );
    }

    /// A filesystem that cannot fsync a directory answers EINVAL or ENOTSUP.
    /// The marker is on disk by then, so that is not a failed write.
    #[cfg(unix)]
    #[test]
    fn a_directory_fsync_the_filesystem_refuses_counts_as_success() {
        for code in [libc::EINVAL, libc::ENOTSUP] {
            accept_unsupported_dir_fsync(Err(std::io::Error::from_raw_os_error(code)))
                .unwrap_or_else(|e| panic!("errno {code} must count as success: {e}"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_fsync_that_fails_for_another_reason_still_fails() {
        for code in [libc::EIO, libc::ENOSPC] {
            assert!(
                accept_unsupported_dir_fsync(Err(std::io::Error::from_raw_os_error(code))).is_err(),
                "errno {code} must still fail"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn sync_dir_succeeds_on_a_real_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        sync_dir(tmp.path()).unwrap();
    }

    /// procfs answers a directory fsync with EINVAL. `sync_dir` has to treat
    /// that as success, or a finished marker would read as a failed write.
    #[cfg(target_os = "linux")]
    #[test]
    fn sync_dir_accepts_a_directory_the_filesystem_cannot_fsync() {
        let proc_dir = Path::new("/proc/self");
        if !proc_dir.is_dir() {
            return;
        }

        sync_dir(proc_dir).unwrap();
    }

    /// A copy that fails partway must not leave its directory behind: every
    /// later start would otherwise find one more partial directory.
    #[test]
    fn a_backup_that_fails_partway_removes_the_directory_it_created() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("MEMORY.md"), "- **k**: v\n").unwrap();
        // A directory named like a note: copying it fails after `MEMORY.md`
        // was already copied.
        fs::create_dir_all(tmp.path().join("memory").join("broken.md")).unwrap();

        assert!(backup_markdown_memory(tmp.path()).is_err());

        let leftovers: Vec<_> = fs::read_dir(tmp.path().join("memory").join("migrations"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    /// The directory of an earlier, finished backup is not the failed
    /// attempt's to remove, even when both got the same name. The name comes
    /// from the start time, so a fixed start time makes the collision certain.
    #[test]
    fn a_failed_backup_leaves_a_directory_it_did_not_create() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("MEMORY.md"), "- **k**: v\n").unwrap();
        fs::create_dir_all(tmp.path().join("memory").join("broken.md")).unwrap();
        let started = "2001-02-03T04:05:06Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        let earlier = tmp
            .path()
            .join("memory")
            .join("migrations")
            .join(backup_dir_name(started));
        fs::create_dir_all(&earlier).unwrap();
        fs::write(earlier.join("BACKUP_COMPLETE"), "").unwrap();

        assert!(backup_markdown_memory_started_at(tmp.path(), started).is_err());

        assert!(earlier.join("BACKUP_COMPLETE").exists());
    }

    #[test]
    fn a_backup_made_while_an_import_is_pending_records_the_pending_time() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("MEMORY.md"), "- **k**: v\n").unwrap();
        let migrations = tmp.path().join("memory").join("migrations");
        fs::create_dir_all(&migrations).unwrap();
        fs::write(migrations.join("PENDING"), "2020-01-01T00:00:00+00:00").unwrap();

        let backup_dir = backup_markdown_memory(tmp.path()).unwrap().unwrap();

        assert_eq!(
            backup_complete_time(&backup_dir),
            "2020-01-01T00:00:00+00:00",
            "rows stored after the first failed attempt must count as newer than this backup"
        );
    }

    #[test]
    fn a_pending_marker_is_recorded_once_and_keeps_its_first_time() {
        let tmp = tempfile::TempDir::new().unwrap();
        record_pending_markdown_import(tmp.path()).unwrap();
        let marker = pending_markdown_import_marker(tmp.path());
        let first = fs::read_to_string(&marker).unwrap();
        chrono::DateTime::parse_from_rfc3339(first.trim()).expect("an RFC 3339 time");

        std::thread::sleep(std::time::Duration::from_millis(20));
        record_pending_markdown_import(tmp.path()).unwrap();

        assert_eq!(fs::read_to_string(&marker).unwrap(), first);
    }

    #[test]
    fn memory_md_is_read_after_the_daily_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("memory")).unwrap();
        fs::write(tmp.path().join("MEMORY.md"), "- **k**: curated\n").unwrap();
        fs::write(
            tmp.path().join("memory").join("2026-09-02.md"),
            "- **k**: later day\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("memory").join("2026-09-01.md"),
            "- **k**: earlier day\n",
        )
        .unwrap();

        let values: Vec<String> = read_openclaw_markdown_entries(tmp.path())
            .unwrap()
            .into_iter()
            .map(|e| e.content)
            .collect();

        assert_eq!(values.len(), 3);
        assert_eq!(
            values.last().map(String::as_str),
            Some("curated"),
            "the last entry read wins a key, so the curated file must come last"
        );
    }

    /// The order comes from the file names, not from the order the directory
    /// listing hands the paths over in, and only `*.md` files count.
    #[test]
    fn daily_markdown_files_are_sorted_whatever_the_listing_order() {
        let dir = Path::new("memory");
        let listed = vec![
            dir.join("2026-09-03.md"),
            dir.join("brain.db"),
            dir.join("2026-09-01.md"),
            dir.join("notes.txt"),
            dir.join("2026-09-02.md"),
        ];

        assert_eq!(
            sorted_daily_markdown_files(listed),
            vec![
                dir.join("2026-09-01.md"),
                dir.join("2026-09-02.md"),
                dir.join("2026-09-03.md"),
            ]
        );
    }
}
