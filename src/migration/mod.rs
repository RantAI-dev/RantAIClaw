//! Markdown memory import glue for the v34 config migration.
//!
//! Two functions remain here because they are load-bearing for
//! `src/config/schema.rs::import_markdown_memory_into_sqlite`, which fires the
//! one-time markdown → sqlite import when a config that still names the
//! retired `markdown` backend is loaded under the current schema. The
//! legacy external-import path (`pub mod openclaw`) was removed together
//! with the `rantaiclaw migrate` command — do not reintroduce that command
//! without also reintroducing this module's former siblings.
//!
//! See plan 469 (the `migrate` command removal) and plan 451 (the markdown
//! backend retirement, which made this glue necessary).

use anyhow::Result;
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
    let mut entries = Vec::new();

    for (idx, raw_line) in content.lines().enumerate() {
        let trimmed = raw_line.trim();
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
/// Copy `MEMORY.md` and every `*.md` in `memory/` (non-recursive, so
/// `memory/archive/` is left alone) under a `markdown-<timestamp>` directory
/// the operator can find later.
pub(crate) fn backup_markdown_memory(workspace_dir: &Path) -> Result<Option<std::path::PathBuf>> {
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let backup_root = workspace_dir
        .join("memory")
        .join("migrations")
        .join(format!("markdown-{timestamp}"));

    fs::create_dir_all(&backup_root)?;

    let memory_md = workspace_dir.join("MEMORY.md");
    let mut copied_any = false;
    if memory_md.exists() {
        if let Some(name) = memory_md.file_name() {
            fs::copy(&memory_md, backup_root.join(name))?;
            copied_any = true;
        }
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
            fs::copy(&path, backup_root.join(name))?;
            copied_any = true;
        }
    }

    if copied_any {
        Ok(Some(backup_root))
    } else {
        let _ = fs::remove_dir_all(&backup_root);
        Ok(None)
    }
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
