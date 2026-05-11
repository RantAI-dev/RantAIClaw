//! Runtime allowlist overlay — basenames the user approved at runtime
//! and chose to persist across restarts.
//!
//! Layout: `<policy_dir>/runtime_allowlist.toml`
//! Schema: `commands = ["brew", "rg", ...]`
//!
//! The overlay is loaded once at startup and merged with the boot-time
//! `allowed_commands` list. New entries appended at runtime are written
//! atomically (tmp + rename) so a crash mid-write cannot corrupt the file.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "runtime_allowlist.toml";

#[derive(Debug, Default, Serialize, Deserialize)]
struct OverlayFile {
    #[serde(default)]
    commands: Vec<String>,
}

/// Path to the overlay file inside a policy_dir.
pub fn overlay_path(policy_dir: &Path) -> PathBuf {
    policy_dir.join(FILE_NAME)
}

/// Load basenames from the overlay file. Returns empty set if missing.
pub fn load(policy_dir: &Path) -> Result<BTreeSet<String>> {
    let path = overlay_path(policy_dir);
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("read runtime allowlist: {}", path.display()))?;
    let parsed: OverlayFile = toml::from_str(&body)
        .with_context(|| format!("parse runtime allowlist: {}", path.display()))?;
    Ok(parsed.commands.into_iter().collect())
}

/// Append a basename to the overlay file, preserving any existing entries.
/// Idempotent: appending a basename that already exists is a no-op.
pub fn append(policy_dir: &Path, basename: &str) -> Result<()> {
    let basename = basename.trim();
    if basename.is_empty() {
        anyhow::bail!("runtime allowlist: empty basename");
    }
    if basename.contains(char::is_whitespace) {
        anyhow::bail!("runtime allowlist: basename must be a single token");
    }
    std::fs::create_dir_all(policy_dir)
        .with_context(|| format!("create policy dir: {}", policy_dir.display()))?;

    let mut commands = load(policy_dir)?;
    if !commands.insert(basename.to_string()) {
        return Ok(());
    }

    let file = OverlayFile {
        commands: commands.into_iter().collect(),
    };
    let body = toml::to_string_pretty(&file).context("serialize runtime allowlist")?;

    let target = overlay_path(policy_dir);
    let tmp = target.with_extension("toml.tmp");
    std::fs::write(&tmp, body)
        .with_context(|| format!("write runtime allowlist tmp: {}", tmp.display()))?;
    std::fs::rename(&tmp, &target)
        .with_context(|| format!("rename runtime allowlist: {}", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_returns_empty() {
        let temp = tempfile::tempdir().unwrap();
        let set = load(temp.path()).unwrap();
        assert!(set.is_empty());
    }

    #[test]
    fn append_creates_file_with_entry() {
        let temp = tempfile::tempdir().unwrap();
        append(temp.path(), "brew").unwrap();
        let set = load(temp.path()).unwrap();
        assert!(set.contains("brew"));
    }

    #[test]
    fn append_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        append(temp.path(), "brew").unwrap();
        append(temp.path(), "brew").unwrap();
        let set = load(temp.path()).unwrap();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn append_accumulates_distinct_entries() {
        let temp = tempfile::tempdir().unwrap();
        append(temp.path(), "brew").unwrap();
        append(temp.path(), "rg").unwrap();
        append(temp.path(), "fd").unwrap();
        let set = load(temp.path()).unwrap();
        assert_eq!(set.len(), 3);
        assert!(set.contains("brew") && set.contains("rg") && set.contains("fd"));
    }

    #[test]
    fn append_rejects_whitespace_basename() {
        let temp = tempfile::tempdir().unwrap();
        assert!(append(temp.path(), "brew install").is_err());
        assert!(append(temp.path(), "").is_err());
    }

    #[test]
    fn append_creates_policy_dir_if_missing() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("policy-nonexistent");
        append(&nested, "brew").unwrap();
        let set = load(&nested).unwrap();
        assert!(set.contains("brew"));
    }
}
