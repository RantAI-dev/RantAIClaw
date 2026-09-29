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
        let mut daily_files = Vec::new();
        for file in fs::read_dir(&daily_dir)? {
            let path = file?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                daily_files.push(path);
            }
        }
        daily_files.sort();
        for path in daily_files {
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
pub(crate) fn replace_file_atomically(path: &Path, contents: &[u8]) -> Result<()> {
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

fn sync_file(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("fsync {}", path.display()))
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<()> {
    sync_file(dir)
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
pub(crate) fn backup_markdown_memory(workspace_dir: &Path) -> Result<Option<PathBuf>> {
    let started = Utc::now();
    let pending_marker = pending_markdown_import_marker(workspace_dir);
    let cutoff = if pending_marker.exists() {
        started
    } else {
        started
    };

    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let pid = std::process::id();
    let migrations_dir = workspace_dir.join("memory").join("migrations");
    let backup_root = migrations_dir.join(format!("markdown-{timestamp}-{pid}"));
    let backup_memory_dir = backup_root.join("memory");

    fs::create_dir_all(&backup_root)?;

    let memory_md = workspace_dir.join("MEMORY.md");
    let mut copied_any = false;
    if memory_md.exists() {
        let dest = backup_root.join("MEMORY.md");
        fs::copy(&memory_md, &dest)?;
        sync_file(&dest)?;
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
            let dest = backup_memory_dir.join(name);
            fs::copy(&path, &dest)?;
            sync_file(&dest)?;
            copied_any = true;
        }
    }

    if !copied_any {
        let _ = fs::remove_dir_all(&backup_root);
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
    sync_dir(&backup_root)?;
    sync_dir(&migrations_dir)?;

    // Written last, once every copy above has succeeded and reached the disk:
    // see the doc comment above for why the sweep depends on this.
    write_marker_durably(
        &backup_root.join("BACKUP_COMPLETE"),
        cutoff.to_rfc3339().as_bytes(),
    )?;

    Ok(Some(backup_root))
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

    #[test]
    fn backup_complete_marker_records_the_time_the_backup_started() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("MEMORY.md"), "- **k**: v\n").unwrap();

        let before = chrono::Utc::now();
        let backup_dir = backup_markdown_memory(tmp.path()).unwrap().unwrap();
        let after = chrono::Utc::now();

        let recorded =
            chrono::DateTime::parse_from_rfc3339(backup_complete_time(&backup_dir).trim())
                .expect("the marker holds an RFC 3339 time")
                .with_timezone(&chrono::Utc);
        assert!(
            before <= recorded && recorded <= after,
            "{recorded} is outside {before} .. {after}"
        );
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
    fn daily_files_are_read_before_memory_md_and_in_name_order() {
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

        assert_eq!(values, ["earlier day", "later day", "curated"]);
    }
}
