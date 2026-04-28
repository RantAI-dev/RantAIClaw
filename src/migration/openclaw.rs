//! OpenClaw / ZeroClaw → RantaiClaw config migration.
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md` §7.2.
//! Plan: `docs/superpowers/plans/2026-04-27-onboarding-depth-v2.md` Wave 4 — Task 4C.
//!
//! Detects an OpenClaw (or pre-rename ZeroClaw) install on the local
//! filesystem and migrates it into a fresh RantaiClaw profile via
//! `ProfileManager::create_clone_from_path`.
//!
//! What's translated:
//!
//! - `[provider]` / `[providers]` blocks → identical (both formats use the
//!   same keys, so they're emitted verbatim into the new profile config).
//! - `[gateway]` block → identical.
//! - `[autonomy]` / `[approvals]` blocks: OpenClaw didn't have them, so we
//!   generate L2-Smart defaults (Wave 4A's preset). Inline stub for now —
//!   Wave 4A may rewrite `defaults_l2_smart_toml()` once their preset
//!   materialises.
//! - `skills/<slug>/` directories: copied verbatim.
//! - `secrets/api_keys.toml`: copied verbatim (per spec §7.2 the explicit
//!   `migrate from-openclaw` call is a deliberate user action, so we do not
//!   require the `--include-secrets` opt-in here that `migrate_legacy_layout`
//!   enforces for unattended legacy migration).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::profile::ProfileManager;

/// Origin label for a detected on-disk install. Drives only the user-facing
/// summary text — translation is identical for both variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceVariant {
    /// `~/.openclaw/` (or `~/.config/openclaw/`).
    OpenClaw,
    /// `~/.zeroclaw/` — pre-rename layout. Same shape, older brand.
    ZeroClaw,
}

impl SourceVariant {
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenClaw => "OpenClaw",
            Self::ZeroClaw => "ZeroClaw",
        }
    }
}

/// A detected on-disk OpenClaw / ZeroClaw install ready to migrate.
#[derive(Debug, Clone)]
pub struct DetectedSource {
    pub root: PathBuf,
    pub variant: SourceVariant,
}

impl DetectedSource {
    /// `<root>/config.toml`.
    pub fn config_toml(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// `<root>/skills/`.
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// `<root>/secrets/`.
    pub fn secrets_dir(&self) -> PathBuf {
        self.root.join("secrets")
    }
}

/// Summary of what was migrated. Used to print the final user-facing line and
/// to make integration-test assertions trivial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationSummary {
    pub source_root: PathBuf,
    pub source_variant: SourceVariant,
    pub profile_name: String,
    pub skills_migrated: usize,
    pub secrets_migrated: usize,
    pub config_blocks_migrated: usize,
}

impl MigrationSummary {
    pub fn print_human(&self) {
        println!(
            "Migrated {} skills, {} secrets, {} config blocks from {} ({}) → profile {:?}",
            self.skills_migrated,
            self.secrets_migrated,
            self.config_blocks_migrated,
            self.source_variant.label(),
            self.source_root.display(),
            self.profile_name,
        );
    }
}

/// Default candidate roots, in detection priority order. Public so the CLI
/// can list them in the "no source detected" error message.
pub fn detection_paths() -> Vec<PathBuf> {
    let home = match directories::UserDirs::new() {
        Some(u) => u.home_dir().to_path_buf(),
        None => return vec![],
    };
    vec![
        home.join(".openclaw"),
        home.join(".zeroclaw"),
        home.join(".config").join("openclaw"),
        home.join(".config").join("zeroclaw"),
    ]
}

/// Walk the standard candidate locations and return the first one that looks
/// like an OpenClaw / ZeroClaw install (i.e. has a `config.toml` at the root).
pub fn detect() -> Option<DetectedSource> {
    for path in detection_paths() {
        if let Some(found) = detect_at(&path) {
            return Some(found);
        }
    }
    None
}

/// Test-friendly: probe a specific root. Returns `None` if it doesn't look
/// like an OpenClaw install.
pub fn detect_at(root: &Path) -> Option<DetectedSource> {
    if !root.is_dir() {
        return None;
    }
    if !root.join("config.toml").is_file() {
        return None;
    }
    let variant = classify_variant(root);
    Some(DetectedSource {
        root: root.to_path_buf(),
        variant,
    })
}

fn classify_variant(root: &Path) -> SourceVariant {
    // Only the directory name matters — the file shape is identical.
    let name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if name.contains("zeroclaw") {
        SourceVariant::ZeroClaw
    } else {
        SourceVariant::OpenClaw
    }
}

/// Migrate the given on-disk install into a brand-new profile.
///
/// Fails up-front if the destination profile already exists and `force` is
/// `false` (we never silently merge into an existing profile here — that's
/// `migrate from-openclaw`'s `--overwrite` flag's job in the higher-level
/// flow, which Wave 4 wires in subsequent passes).
pub fn migrate_to_profile(
    source: &DetectedSource,
    profile_name: &str,
    force: bool,
) -> Result<MigrationSummary> {
    if !source.root.is_dir() {
        bail!(
            "{} source root {} no longer exists",
            source.variant.label(),
            source.root.display()
        );
    }

    ProfileManager::create_clone_from_path(profile_name, &source.root, force)
        .with_context(|| format!("create profile {profile_name:?} from {}", source.root.display()))?;

    let config_blocks_migrated = count_translated_config_blocks(&source.config_toml())?;
    let skills_migrated = count_skill_dirs(&source.skills_dir())?;
    let secrets_migrated = count_secret_files(&source.secrets_dir())?;

    Ok(MigrationSummary {
        source_root: source.root.clone(),
        source_variant: source.variant,
        profile_name: profile_name.to_string(),
        skills_migrated,
        secrets_migrated,
        config_blocks_migrated,
    })
}

/// Translate a source `config.toml` into a destination `config.toml` body.
///
/// Pure string-level merge: we keep top-level keys + `[provider]`,
/// `[providers.*]`, `[gateway]` verbatim, then append synthesised
/// `[autonomy]` / `[approvals]` defaults if the source didn't have them.
///
/// Returns `(body, blocks_translated_count)` where the count is the number
/// of distinct top-level sections present in source (top-level scalar block
/// counts as one if any keys exist) plus any default sections we appended.
pub fn translate_config(source_toml: &str) -> (String, usize) {
    let mut out = String::new();
    out.push_str("# Migrated from OpenClaw / ZeroClaw config.toml\n");
    out.push_str("# Provider, gateway and top-level keys preserved verbatim.\n");
    out.push_str("# [autonomy] + [approvals] blocks synthesised with L2-Smart defaults.\n\n");
    out.push_str(source_toml.trim_end());
    out.push_str("\n\n");

    let mut blocks_translated = count_top_level_blocks(source_toml);

    if !source_toml.contains("[autonomy]") {
        out.push_str(defaults_l2_smart_autonomy_toml());
        out.push('\n');
        blocks_translated += 1;
    }
    if !source_toml.contains("[approvals]") {
        out.push_str(defaults_l2_smart_approvals_toml());
        out.push('\n');
        blocks_translated += 1;
    }

    (out, blocks_translated)
}

/// Count the number of distinct top-level config "blocks" in a TOML string —
/// the unbracketed preamble (if it has any keys) plus each `[section]` /
/// `[section.subsection]` header. Heuristic, not a full TOML parse — we only
/// use this for the user-facing summary number.
fn count_top_level_blocks(source_toml: &str) -> usize {
    let mut count = 0;
    let mut saw_preamble_key = false;
    let mut in_table = false;
    for raw in source_toml.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            count += 1;
            in_table = true;
        } else if !in_table && line.contains('=') {
            saw_preamble_key = true;
        }
    }
    if saw_preamble_key {
        count += 1;
    }
    count
}

/// Number of skill directories under `<source>/skills/` (each subdirectory
/// counts once; loose files are ignored).
fn count_skill_dirs(skills_dir: &Path) -> Result<usize> {
    if !skills_dir.is_dir() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(skills_dir)
        .with_context(|| format!("read_dir {}", skills_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            count += 1;
        }
    }
    Ok(count)
}

/// Number of files in `<source>/secrets/`. Used for the summary line only —
/// the actual copy happens inside `ProfileManager::create_clone_from_path`.
fn count_secret_files(secrets_dir: &Path) -> Result<usize> {
    if !secrets_dir.is_dir() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(secrets_dir)
        .with_context(|| format!("read_dir {}", secrets_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            count += 1;
        }
    }
    Ok(count)
}

/// Count config blocks for the summary. Reads the file, defers to
/// `translate_config` so we count synthesised defaults consistently.
fn count_translated_config_blocks(source_config: &Path) -> Result<usize> {
    if !source_config.is_file() {
        return Ok(0);
    }
    let body = fs::read_to_string(source_config)
        .with_context(|| format!("read {}", source_config.display()))?;
    Ok(translate_config(&body).1)
}

/// L2-Smart `[autonomy]` defaults — see `RantaiClaw v0.5.0` autonomy spec.
///
/// Stub: matches the shape Wave 4A is producing in `src/approval/presets/`.
/// Inlined here so this wave is independent; can be re-pointed once Wave 4A
/// lands `presets::l2_smart::AUTONOMY_TOML`.
pub fn defaults_l2_smart_autonomy_toml() -> &'static str {
    "[autonomy]\nlevel = \"smart\"\nworkspace_only = true\nallowed_commands = [\"ls\", \"cat\", \"grep\", \"rg\", \"find\", \"git status\", \"git diff\", \"git log\"]\nforbidden_paths = [\"/etc\", \"/root\", \"~/.ssh\", \"~/.aws\", \"~/.config/gh\"]\nmax_actions_per_hour = 240\nmax_cost_per_day_cents = 500\n"
}

/// L2-Smart `[approvals]` defaults.
pub fn defaults_l2_smart_approvals_toml() -> &'static str {
    "[approvals]\nmode = \"smart\"\nallowlist_path = \"policy/command_allowlist.toml\"\nforbidden_paths_path = \"policy/forbidden_paths.toml\"\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn detect_at_returns_none_for_empty_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(detect_at(tmp.path()).is_none());
    }

    #[test]
    fn detect_at_returns_none_when_config_toml_missing() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("skills")).unwrap();
        assert!(detect_at(tmp.path()).is_none());
    }

    #[test]
    fn detect_at_recognises_openclaw_layout() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join(".openclaw");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("config.toml"), "default_provider = \"x\"\n").unwrap();
        let found = detect_at(&root).expect("detect should succeed");
        assert_eq!(found.variant, SourceVariant::OpenClaw);
    }

    #[test]
    fn detect_at_recognises_zeroclaw_layout_by_dirname() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join(".zeroclaw");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("config.toml"), "default_provider = \"x\"\n").unwrap();
        let found = detect_at(&root).expect("detect should succeed");
        assert_eq!(found.variant, SourceVariant::ZeroClaw);
    }

    #[test]
    fn translate_config_appends_default_autonomy_block_when_missing() {
        let src = "default_provider = \"openrouter\"\n[gateway]\nport = 8765\n";
        let (out, blocks) = translate_config(src);
        assert!(out.contains("[autonomy]"));
        assert!(out.contains("[approvals]"));
        // 1 preamble + 1 [gateway] + 1 synthesised [autonomy] + 1 synthesised [approvals]
        assert_eq!(blocks, 4);
    }

    #[test]
    fn translate_config_preserves_existing_autonomy_block() {
        let src = "[autonomy]\nlevel = \"strict\"\n";
        let (out, _) = translate_config(src);
        // The original autonomy block survives; we don't override it.
        assert!(out.contains("level = \"strict\""));
        // We don't emit a default L2-Smart block on top — the synthesised
        // block sets `level = "smart"`, which must NOT be present here.
        assert!(
            !out.contains("level = \"smart\""),
            "must not append L2-Smart default when source already has [autonomy]"
        );
    }

    #[test]
    fn count_top_level_blocks_handles_preamble_only() {
        assert_eq!(count_top_level_blocks("foo = 1\nbar = 2\n"), 1);
    }

    #[test]
    fn count_top_level_blocks_counts_each_section() {
        let src = "[a]\nx=1\n[b.c]\ny=2\n";
        assert_eq!(count_top_level_blocks(src), 2);
    }

    #[test]
    fn count_top_level_blocks_ignores_comments_and_blanks() {
        let src = "# comment\n\n[a]\nx=1\n";
        assert_eq!(count_top_level_blocks(src), 1);
    }

    #[test]
    fn count_skill_dirs_counts_only_directories() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("skills/one")).unwrap();
        std::fs::create_dir_all(tmp.path().join("skills/two")).unwrap();
        std::fs::write(tmp.path().join("skills/loose.txt"), "ignored").unwrap();
        assert_eq!(count_skill_dirs(&tmp.path().join("skills")).unwrap(), 2);
    }

    #[test]
    fn count_skill_dirs_returns_zero_for_missing_dir() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(count_skill_dirs(&tmp.path().join("nope")).unwrap(), 0);
    }

    #[test]
    fn detection_paths_lists_all_four_candidates() {
        let paths = detection_paths();
        assert_eq!(paths.len(), 4, "must probe ~/.openclaw, ~/.zeroclaw, ~/.config/openclaw, ~/.config/zeroclaw");
    }
}
