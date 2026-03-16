//! Runtime config overrides — persisted to config.runtime.toml alongside the base config.toml.
//!
//! Merge strategy: runtime overrides win for any key present in both files.
//! This preserves user comments and manual edits in the base config.

use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use toml::Value as TomlValue;

/// Derive runtime config path from base config path.
/// `config.toml` → `config.runtime.toml`
pub fn runtime_path(base_config_path: &Path) -> PathBuf {
    let stem = base_config_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let ext = base_config_path
        .extension()
        .unwrap_or_default()
        .to_string_lossy();
    base_config_path.with_file_name(format!("{}.runtime.{}", stem, ext))
}

/// Read runtime overrides from disk. Returns empty table if file doesn't exist.
pub fn read_runtime_overrides(base_config_path: &Path) -> Result<TomlValue> {
    let path = runtime_path(base_config_path);
    if !path.exists() {
        return Ok(TomlValue::Table(toml::map::Map::new()));
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let value: TomlValue = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(value)
}

/// Write a specific section to config.runtime.toml.
/// Reads existing overrides, merges the new section, writes back.
pub fn write_runtime_section(base_config_path: &Path, section: &str, value: TomlValue) -> Result<()> {
    let path = runtime_path(base_config_path);
    let mut overrides = read_runtime_overrides(base_config_path)?;

    if let TomlValue::Table(ref mut table) = overrides {
        table.insert(section.to_string(), value);
    }

    let content = toml::to_string_pretty(&overrides)
        .context("Failed to serialize runtime overrides")?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// Remove a section from config.runtime.toml.
pub fn remove_runtime_section(base_config_path: &Path, section: &str) -> Result<()> {
    let path = runtime_path(base_config_path);
    if !path.exists() {
        return Ok(());
    }
    let mut overrides = read_runtime_overrides(base_config_path)?;
    if let TomlValue::Table(ref mut table) = overrides {
        table.remove(section);
    }
    let content = toml::to_string_pretty(&overrides)
        .context("Failed to serialize runtime overrides")?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// Deep-merge two TOML values. `override_val` wins for any key present in both.
pub fn deep_merge(base: &mut TomlValue, override_val: &TomlValue) {
    match (base, override_val) {
        (TomlValue::Table(base_table), TomlValue::Table(override_table)) => {
            for (key, ov) in override_table {
                if let Some(bv) = base_table.get_mut(key) {
                    deep_merge(bv, ov);
                } else {
                    base_table.insert(key.clone(), ov.clone());
                }
            }
        }
        (base, override_val) => {
            *base = override_val.clone();
        }
    }
}

/// Load config with runtime overrides merged on top.
/// Called at startup: reads base config.toml, then merges config.runtime.toml.
pub fn load_with_runtime_overrides(base_config_path: &Path) -> Result<String> {
    let base_content = std::fs::read_to_string(base_config_path)
        .with_context(|| format!("Failed to read {}", base_config_path.display()))?;
    let mut base: TomlValue = toml::from_str(&base_content)
        .with_context(|| format!("Failed to parse {}", base_config_path.display()))?;

    let overrides = read_runtime_overrides(base_config_path)?;
    deep_merge(&mut base, &overrides);

    toml::to_string_pretty(&base).context("Failed to serialize merged config")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_runtime_path() {
        let base = Path::new("/root/.rantaiclaw/config.toml");
        assert_eq!(runtime_path(base), PathBuf::from("/root/.rantaiclaw/config.runtime.toml"));
    }

    #[test]
    fn test_deep_merge_tables() {
        let mut base: TomlValue = toml::from_str("[a]\nx = 1\ny = 2").unwrap();
        let over: TomlValue = toml::from_str("[a]\ny = 99\nz = 3").unwrap();
        deep_merge(&mut base, &over);
        let table = base.as_table().unwrap().get("a").unwrap().as_table().unwrap();
        assert_eq!(table.get("x").unwrap().as_integer(), Some(1));
        assert_eq!(table.get("y").unwrap().as_integer(), Some(99));
        assert_eq!(table.get("z").unwrap().as_integer(), Some(3));
    }

    #[test]
    fn test_read_missing_runtime_returns_empty() {
        let result = read_runtime_overrides(Path::new("/nonexistent/config.toml")).unwrap();
        assert!(result.as_table().unwrap().is_empty());
    }

    #[test]
    fn test_write_and_read_runtime_section() {
        let mut base = NamedTempFile::new().unwrap();
        writeln!(base, "[gateway]\nport = 8080").unwrap();
        let base_path = base.path();

        let section_toml: TomlValue = toml::from_str("bot_token = \"abc\"").unwrap();
        write_runtime_section(base_path, "channels_config", section_toml).unwrap();

        let overrides = read_runtime_overrides(base_path).unwrap();
        assert!(overrides.as_table().unwrap().contains_key("channels_config"));
    }

    #[test]
    fn test_remove_runtime_section() {
        let mut base = NamedTempFile::new().unwrap();
        writeln!(base, "[gateway]\nport = 8080").unwrap();
        let base_path = base.path();

        let section_toml: TomlValue = toml::from_str("key = \"value\"").unwrap();
        write_runtime_section(base_path, "test_section", section_toml).unwrap();
        remove_runtime_section(base_path, "test_section").unwrap();

        let overrides = read_runtime_overrides(base_path).unwrap();
        assert!(!overrides.as_table().unwrap().contains_key("test_section"));
    }
}
