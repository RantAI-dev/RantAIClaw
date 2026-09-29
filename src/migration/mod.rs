//! Markdown memory import glue for the v34 config migration.
//!
//! Two functions remain here because they are load-bearing for
//! `src/config/schema.rs::import_markdown_memory_into_sqlite`, which fires the
//! one-time markdown → sqlite import when a config that still names the
//! retired `markdown` backend is loaded under the current schema. The
//! legacy external-import path (`pub mod openclaw`) was removed together
//! with the `rantaiclaw migrate` command — do not reintroduce that command
//! without also reintroducing this module's former siblings. The markdown
//! backend retirement is what made this glue necessary.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use crate::memory::MemoryCategory;

/// One row of memory recovered from a source workspace. The markdown reader
/// shares this shape with the v34 importer's expectations.
#[derive(Debug, Clone)]
pub(crate) struct SourceEntry {
    pub(crate) key: String,
    pub(crate) content: String,
    pub(crate) category: MemoryCategory,
}

pub(crate) fn read_openclaw_markdown_entries(source_workspace: &Path) -> Result<Vec<SourceEntry>> {
    let mut all = Vec::new();

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

    let daily_dir = source_workspace.join("memory");
    if daily_dir.exists() {
        for file in fs::read_dir(&daily_dir)? {
            let file = file?;
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
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

    Ok(all)
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
/// subdirectory layout.
pub(crate) fn backup_markdown_memory(workspace_dir: &Path) -> Result<Option<std::path::PathBuf>> {
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let pid = std::process::id();
    let backup_root = workspace_dir
        .join("memory")
        .join("migrations")
        .join(format!("markdown-{timestamp}-{pid}"));
    let backup_memory_dir = backup_root.join("memory");

    fs::create_dir_all(&backup_root)?;

    let memory_md = workspace_dir.join("MEMORY.md");
    let mut copied_any = false;
    if memory_md.exists() {
        fs::copy(&memory_md, backup_root.join("MEMORY.md"))?;
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
            fs::copy(&path, backup_memory_dir.join(name))?;
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
        vacuum_brain_db_into(&brain_db, &backup_memory_dir.join("brain.db"))?;
    }

    // Written last, once every copy above has succeeded: see the doc
    // comment above for why the sweep depends on this.
    fs::write(backup_root.join("BACKUP_COMPLETE"), [])?;

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
}
