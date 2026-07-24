pub mod bundled;
pub mod clawhub;
pub mod install_deps;
pub mod watcher;

use anyhow::{Context, Result};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const OPEN_SKILLS_REPO_URL: &str = "https://github.com/besoeasy/open-skills";
const OPEN_SKILLS_SYNC_MARKER: &str = ".rantaiclaw-open-skills-sync";
const OPEN_SKILLS_SYNC_INTERVAL_SECS: u64 = 60 * 60 * 24 * 7;

/// A skill is a user-defined or community-built capability.
/// Skills live in `~/.rantaiclaw/workspace/skills/<name>/SKILL.md`
/// and can include tool definitions, prompts, and automation scripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub tools: Vec<SkillTool>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(skip)]
    pub location: Option<PathBuf>,
    /// Declared dependencies extracted from `metadata.clawdbot.requires` and
    /// the top-level `env:` block in SKILL.md frontmatter. Used by the
    /// loader to filter unusable skills (missing bins, env, wrong OS) and
    /// to surface "why" via `skills list` / `skills inspect`.
    #[serde(default)]
    pub requires: SkillRequires,
    /// Install recipes parsed from `metadata.clawdbot.install[]`. Each
    /// recipe is one way to fulfil a missing binary requirement
    /// (`brew install ...`, `npm install -g ...`, etc.). The
    /// `skills install-deps` runner picks one preferred recipe per skill
    /// based on host availability. Empty when the skill doesn't ship
    /// install metadata — user has to install deps themselves.
    #[serde(default)]
    pub install_recipes: Vec<SkillInstallRecipe>,
}

/// One install recipe for a skill's binary dependency. Mirrors OpenClaw's
/// `metadata.clawdbot.install[]` entries shape.
///
/// Five recipe kinds supported:
/// * `brew`   — `brew install <formula>`. Cross-platform if Homebrew is set up.
/// * `npm`    — `npm install -g <pkg>` (or pnpm/yarn per `nodeManager`).
/// * `uv`     — `uv tool install <pkg>` (Python tools).
/// * `go`     — `go install <module>`. If `go` itself is missing AND
///              brew is available, the runner bootstraps Go via brew.
/// * `download` — fetch URL, optionally extract, drop in `targetDir`.
///
/// Per ClawHub convention, each recipe MAY have an `os: ["linux"|"darwin"|...]`
/// filter; runners skip recipes whose `os` doesn't match the current host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillInstallRecipe {
    /// Stable identifier (e.g. "brew", "npm-global"). Used for logging.
    #[serde(default)]
    pub id: String,
    /// Recipe kind: brew | npm | uv | go | download.
    #[serde(default)]
    pub kind: String,
    /// Binaries this recipe provides (validates after install).
    #[serde(default)]
    pub bins: Vec<String>,
    /// Human-readable label (e.g. "Install Gemini CLI (brew)").
    #[serde(default)]
    pub label: String,
    /// Platform filter; empty = any.
    #[serde(default)]
    pub os: Vec<String>,
    /// Homebrew formula (kind=brew). Tap-prefixed allowed: `org/tap/formula`.
    #[serde(default)]
    pub formula: Option<String>,
    /// Package name (kind=npm/uv).
    #[serde(default)]
    pub pkg: Option<String>,
    /// Go module path with optional `@version` suffix (kind=go).
    #[serde(default)]
    pub module: Option<String>,
    /// Direct-download URL (kind=download).
    #[serde(default)]
    pub url: Option<String>,
    /// Archive type (`tar.gz`, `tar.bz2`, `zip`, or empty/`raw` for no extract).
    #[serde(default)]
    pub archive: Option<String>,
    /// Number of leading directory components to strip on extract.
    #[serde(default)]
    pub strip_components: Option<usize>,
    /// Target directory for download recipes; defaults to
    /// `~/.rantaiclaw/tools/<skill-slug>/`.
    #[serde(default)]
    pub target_dir: Option<String>,
}

impl SkillInstallRecipe {
    /// Whether this recipe is eligible on the current host (OS filter).
    pub fn matches_os(&self) -> bool {
        if self.os.is_empty() {
            return true;
        }
        let current = std::env::consts::OS;
        self.os.iter().any(|o| o.eq_ignore_ascii_case(current))
    }
}

/// OpenClaw / ClawHub-format declared dependencies for a skill. Mirrors the
/// `metadata.clawdbot.requires` shape used by ClawHub-published skills:
///   metadata: {"clawdbot":{"requires":{"bins":["curl","gh"]},"os":["linux","darwin"]}}
/// plus the YAML-block `env:` style used by skills like `freeride`:
///   env:
///     - name: OPENROUTER_API_KEY
///       required: true
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillRequires {
    /// Binary executables that must be on `$PATH`.
    #[serde(default)]
    pub bins: Vec<String>,
    /// Required environment variables (must be set and non-empty).
    #[serde(default)]
    pub env: Vec<String>,
    /// Allowed operating systems (`linux`, `darwin`, `windows`). Empty = any.
    #[serde(default)]
    pub os: Vec<String>,
}

impl SkillRequires {
    /// Check whether this skill's declared deps are satisfied on the
    /// current host. Returns the list of unmet reasons (empty = OK).
    pub fn unmet(&self) -> Vec<String> {
        let mut reasons = Vec::new();

        if !self.os.is_empty() {
            let current = std::env::consts::OS;
            if !self.os.iter().any(|o| o.eq_ignore_ascii_case(current)) {
                reasons.push(format!(
                    "wrong OS: requires {} (this is {})",
                    self.os.join(" or "),
                    current
                ));
            }
        }

        for bin in &self.bins {
            if which::which(bin).is_err() {
                reasons.push(format!("missing binary `{bin}`"));
            }
        }

        for var in &self.env {
            match std::env::var(var) {
                Ok(v) if !v.is_empty() => {}
                _ => reasons.push(format!("env `{var}` not set")),
            }
        }

        reasons
    }
}

/// A tool defined by a skill (shell command, HTTP call, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTool {
    pub name: String,
    pub description: String,
    /// "shell", "http", "script"
    pub kind: String,
    /// The command/URL/script to execute
    pub command: String,
    #[serde(default)]
    pub args: HashMap<String, String>,
}

/// Skill manifest parsed from SKILL.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillManifest {
    skill: SkillMeta,
    #[serde(default)]
    tools: Vec<SkillTool>,
    #[serde(default)]
    prompts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillMeta {
    name: String,
    description: String,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// Case-insensitive lookup into `skills.entries`. Every other skill lookup
/// (`show`, install-deps, TUI `/skill`) matches case-insensitively; the two
/// load-time filters used to match exactly, so `[skills.entries.Weather]`
/// silently failed to disable a skill named `weather`. Match on the canonical
/// `skill.name` but compare case-insensitively.
fn entry_for<'a>(
    entries: &'a std::collections::HashMap<String, crate::config::SkillEntryConfig>,
    skill_name: &str,
) -> Option<&'a crate::config::SkillEntryConfig> {
    entries
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(skill_name))
        .map(|(_, v)| v)
}

/// Load all skills from the workspace skills directory
pub fn load_skills(workspace_dir: &Path) -> Vec<Skill> {
    load_skills_with_open_skills_config(workspace_dir, None, None)
}

/// Load skills using runtime config values (preferred at runtime).
///
/// Applies two filtering layers on top of disk-loaded skills:
///
/// 1. **`requires` gating** — skills declaring `metadata.clawdbot.requires`
///    in their SKILL.md frontmatter (binaries on `$PATH`, env vars set,
///    OS match) are dropped if any requirement is unmet.
/// 2. **Per-skill enable flag** — skills with
///    `[skills.entries.<name>] enabled = false` in `config.toml` are
///    excluded. Default = enabled. Mirrors OpenClaw's `skills.entries.<name>.enabled`.
pub fn load_skills_with_config(workspace_dir: &Path, config: &crate::config::Config) -> Vec<Skill> {
    let raw = load_skills_with_open_skills_config(
        workspace_dir,
        Some(config.skills.open_skills_enabled),
        config.skills.open_skills_dir.as_deref(),
    );
    raw.into_iter()
        .filter(|s| {
            if let Some(entry) = entry_for(&config.skills.entries, &s.name) {
                if !entry.enabled {
                    tracing::debug!(skill = %s.name, "skipped: disabled in config.toml");
                    return false;
                }
            }
            let unmet = s.requires.unmet();
            if !unmet.is_empty() {
                tracing::debug!(
                    skill = %s.name,
                    reasons = %unmet.join("; "),
                    "skipped: unmet requires"
                );
                return false;
            }
            true
        })
        .collect()
}

/// Returns every disk-loaded skill paired with the list of *unmet* gating
/// reasons (empty = active). Used by `skills list` so the user can see
/// which skills are gated out and why. Active skills sort first.
pub fn load_skills_with_status(
    workspace_dir: &Path,
    config: &crate::config::Config,
) -> Vec<(Skill, Vec<String>)> {
    let raw = load_skills_with_open_skills_config(
        workspace_dir,
        Some(config.skills.open_skills_enabled),
        config.skills.open_skills_dir.as_deref(),
    );
    let mut out: Vec<(Skill, Vec<String>)> = raw
        .into_iter()
        .map(|s| {
            let mut reasons = s.requires.unmet();
            if let Some(entry) = entry_for(&config.skills.entries, &s.name) {
                if !entry.enabled {
                    reasons.insert(0, "disabled in config.toml".to_string());
                }
            }
            (s, reasons)
        })
        .collect();
    out.sort_by_key(|(_, reasons)| !reasons.is_empty());
    out
}

/// Resolve `name` to a loaded skill (case-insensitive, same as `skills show`),
/// then return a clone of `config` with `skills.entries.<canonical>.enabled`
/// set to `enabled`, keyed by the skill's canonical `skill.name`. The returned
/// `String` is that canonical name (for user-facing messages). Preserves any
/// existing `api_key`/`env`/`config` on the entry via `entry(..).or_default()`.
///
/// Errors if no loaded skill matches `name` (so a typo fails loudly instead of
/// writing an orphan `entries` key).
///
/// Kept pure/no-I/O so both the sync CLI (`skills enable`/`skills disable`)
/// and the async TUI in-picker toggle can persist the returned config with
/// their own idiom.
///
/// Resolves against [`load_skills_with_status`], **not**
/// [`load_skills_with_config`]: the latter filters out skills already
/// disabled via `entries.<name>.enabled = false`, which would make
/// `skills enable <name>` unable to find (and thus unable to re-enable) the
/// very skill it's being asked to enable. `load_skills_with_status` returns
/// every disk-loaded skill regardless of current gating/disable state, which
/// is what name resolution needs here.
pub(crate) fn set_skill_enabled(
    config: &crate::config::Config,
    name: &str,
    enabled: bool,
) -> Result<(crate::config::Config, String)> {
    let skills = load_skills_with_status(&config.workspace_dir, config);
    let canonical = skills
        .iter()
        .find(|(s, _)| s.name.eq_ignore_ascii_case(name))
        .map(|(s, _)| s.name.clone())
        .ok_or_else(|| anyhow::anyhow!("No skill named '{name}'. Run `rantaiclaw skills list`."))?;

    let mut updated = config.clone();
    // Collapse any pre-existing case-variant key onto the canonical key so we
    // don't leave both `[skills.entries.Weather]` and `[skills.entries.weather]`.
    let existing_variant_key = updated
        .skills
        .entries
        .keys()
        .find(|k| k.eq_ignore_ascii_case(&canonical) && *k != &canonical)
        .cloned();
    if let Some(old_key) = existing_variant_key {
        if let Some(entry) = updated.skills.entries.remove(&old_key) {
            updated.skills.entries.insert(canonical.clone(), entry);
        }
    }
    updated
        .skills
        .entries
        .entry(canonical.clone())
        .or_default()
        .enabled = enabled;
    Ok((updated, canonical))
}

/// Remove an installed skill by name, enforcing the same traversal-reject +
/// 3-root containment gate as the original `skills remove` implementation
/// (034). Shared by the CLI `skills remove` command (the arm below) and the
/// gateway `DELETE /api/v1/skills/{name}` route, so both surfaces enforce
/// identical removal-safety guarantees instead of one of them re-implementing
/// the containment check. Returns the skill's canonical (manifest) name on
/// success. Errors with a message starting `"Skill not found"` when `name`
/// does not resolve to a loaded skill — callers that need to distinguish
/// "not found" from other failures (e.g. to pick an HTTP status) match on
/// that prefix rather than a typed error, since this mirrors the plain
/// `anyhow` error style already used throughout this module.
pub(crate) fn remove_skill(
    workspace_dir: &Path,
    config: &crate::config::Config,
    name: &str,
) -> Result<String> {
    // Reject path traversal attempts
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        anyhow::bail!("Invalid skill name: {name}");
    }

    // Resolve by loaded identity — the same rule `show`/`list` use —
    // instead of joining `name` as a directory under a single root.
    // The primary install paths (ClawHub, bundled starter/core packs)
    // write to `profile.skills_dir()` (root 1), which a bare
    // `skills_dir(workspace_dir)` join never reaches, and a skill's
    // on-disk directory name can differ from the manifest `name:`
    // shown by `list`/`show`.
    let skills = load_skills_with_config(workspace_dir, config);
    let skill = skills
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow::anyhow!("Skill not found: {name}. Run `rantaiclaw skills list`."))?;

    let manifest = skill
        .location
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Skill '{name}' has no on-disk location"))?;
    let skill_dir = manifest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Skill '{name}' manifest has no parent directory"))?;

    // Containment gate: `skill_dir`'s parent must canonicalize to one
    // of the three known skill roots (mirrors `load_workspace_skills`,
    // `:298-346`). Canonicalize each *root*, not `skill_dir` itself, so
    // a legit `skills install /tmp/foo` symlink (whose target lives
    // outside the workspace) stays removable — this is the same
    // "canonicalize the parent, not the symlink target" property the
    // pre-fix single-root check relied on.
    let mut candidate_roots: Vec<PathBuf> = Vec::new();
    if let Ok(profile) = crate::profile::ProfileManager::active() {
        candidate_roots.push(profile.skills_dir());
    }
    if let Some(profile_root) = workspace_dir.parent() {
        candidate_roots.push(profile_root.join("skills"));
    }
    candidate_roots.push(skills_dir(workspace_dir));

    let dir_parent = skill_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Skill path escapes known skills roots: {name}"))?;
    let canonical_dir_parent = dir_parent
        .canonicalize()
        .unwrap_or_else(|_| dir_parent.to_path_buf());
    let contained = candidate_roots.iter().any(|root| {
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        canonical_root == canonical_dir_parent
    });
    if !contained {
        anyhow::bail!("Skill path escapes known skills roots: {name}");
    }

    // Use symlink_metadata so we don't fail on dangling symlinks.
    let meta = std::fs::symlink_metadata(skill_dir)
        .map_err(|_| anyhow::anyhow!("Skill not found: {name}"))?;

    if meta.file_type().is_symlink() {
        std::fs::remove_file(skill_dir)?;
    } else {
        std::fs::remove_dir_all(skill_dir)?;
    }

    Ok(skill.name.clone())
}

fn load_skills_with_open_skills_config(
    workspace_dir: &Path,
    config_open_skills_enabled: Option<bool>,
    config_open_skills_dir: Option<&str>,
) -> Vec<Skill> {
    let mut skills = Vec::new();

    if let Some(open_skills_dir) =
        ensure_open_skills_repo(config_open_skills_enabled, config_open_skills_dir)
    {
        skills.extend(load_open_skills(&open_skills_dir));
    }

    skills.extend(load_workspace_skills(workspace_dir));
    skills
}

fn load_workspace_skills(workspace_dir: &Path) -> Vec<Skill> {
    // Skills can live in three places:
    //   1. `<active_profile>/skills/`        — clawhub::install_one,
    //                                          bundled::install_starter_pack
    //   2. `<workspace_dir>/../skills/`      — profile-relative when
    //                                          workspace_dir is the canonical
    //                                          `<profile>/workspace/` shape
    //   3. `<workspace_dir>/skills/`         — v0.4.x layout, also used by the
    //                                          local-path `skills install`
    //                                          which symlinks into here
    //
    // (1) and (2) collapse to the same path in default user setups, but split
    // when `active_workspace.toml` overrides `workspace_dir` to a non-profile
    // path — in that case (1) is the only place install_one writes to and
    // skipping it makes `/skills` look empty after a successful install.
    // Pre-v0.6.23 the loader only checked (2) and (3), causing the
    // "/skills shows nothing after clawhub install" bug.
    //
    // Dedupe by name. Order: profile → workspace-parent → workspace.
    // Earlier sources win on conflict.
    let mut skills = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    if let Ok(profile) = crate::profile::ProfileManager::active() {
        for s in load_skills_from_directory(&profile.skills_dir()) {
            if seen.insert(s.name.clone()) {
                skills.push(s);
            }
        }
    }

    if let Some(profile_root) = workspace_dir.parent() {
        let profile_skills = profile_root.join("skills");
        for s in load_skills_from_directory(&profile_skills) {
            if seen.insert(s.name.clone()) {
                skills.push(s);
            }
        }
    }

    let workspace_skills = workspace_dir.join("skills");
    for s in load_skills_from_directory(&workspace_skills) {
        if seen.insert(s.name.clone()) {
            skills.push(s);
        }
    }

    skills
}

fn load_skills_from_directory(skills_dir: &Path) -> Vec<Skill> {
    if !skills_dir.exists() {
        return Vec::new();
    }

    let mut skills = Vec::new();

    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return skills;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        // Try SKILL.toml first, then SKILL.md
        let manifest_path = path.join("SKILL.toml");
        let md_path = path.join("SKILL.md");

        if manifest_path.exists() {
            if let Ok(skill) = load_skill_toml(&manifest_path) {
                skills.push(skill);
            }
        } else if md_path.exists() {
            if let Ok(skill) = load_skill_md(&md_path, &path) {
                skills.push(skill);
            }
        }
    }

    skills
}

fn load_open_skills(repo_dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();

    let Ok(entries) = std::fs::read_dir(repo_dir) else {
        return skills;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let is_markdown = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
        if !is_markdown {
            continue;
        }

        let is_readme = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("README.md"));
        if is_readme {
            continue;
        }

        if let Ok(skill) = load_open_skill_md(&path) {
            skills.push(skill);
        }
    }

    skills
}

fn parse_open_skills_enabled(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn open_skills_enabled_from_sources(
    config_open_skills_enabled: Option<bool>,
    env_override: Option<&str>,
) -> bool {
    if let Some(raw) = env_override {
        if let Some(enabled) = parse_open_skills_enabled(raw) {
            return enabled;
        }
        if !raw.trim().is_empty() {
            tracing::warn!(
                "Ignoring invalid RANTAICLAW_OPEN_SKILLS_ENABLED (valid: 1|0|true|false|yes|no|on|off)"
            );
        }
    }

    config_open_skills_enabled.unwrap_or(false)
}

fn open_skills_enabled(config_open_skills_enabled: Option<bool>) -> bool {
    let env_override = std::env::var("RANTAICLAW_OPEN_SKILLS_ENABLED").ok();
    open_skills_enabled_from_sources(config_open_skills_enabled, env_override.as_deref())
}

fn resolve_open_skills_dir_from_sources(
    env_dir: Option<&str>,
    config_dir: Option<&str>,
    home_dir: Option<&Path>,
) -> Option<PathBuf> {
    let parse_dir = |raw: &str| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    };

    if let Some(env_dir) = env_dir.and_then(parse_dir) {
        return Some(env_dir);
    }
    if let Some(config_dir) = config_dir.and_then(parse_dir) {
        return Some(config_dir);
    }
    home_dir.map(|home| home.join("open-skills"))
}

fn resolve_open_skills_dir(config_open_skills_dir: Option<&str>) -> Option<PathBuf> {
    let env_dir = std::env::var("RANTAICLAW_OPEN_SKILLS_DIR").ok();
    let home_dir = UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    resolve_open_skills_dir_from_sources(
        env_dir.as_deref(),
        config_open_skills_dir,
        home_dir.as_deref(),
    )
}

fn ensure_open_skills_repo(
    config_open_skills_enabled: Option<bool>,
    config_open_skills_dir: Option<&str>,
) -> Option<PathBuf> {
    if !open_skills_enabled(config_open_skills_enabled) {
        return None;
    }

    let repo_dir = resolve_open_skills_dir(config_open_skills_dir)?;

    if !repo_dir.exists() {
        if !clone_open_skills_repo(&repo_dir) {
            return None;
        }
        let _ = mark_open_skills_synced(&repo_dir);
        return Some(repo_dir);
    }

    if should_sync_open_skills(&repo_dir) {
        if pull_open_skills_repo(&repo_dir) {
            let _ = mark_open_skills_synced(&repo_dir);
        } else {
            tracing::warn!(
                "open-skills update failed; using local copy from {}",
                repo_dir.display()
            );
        }
    }

    Some(repo_dir)
}

fn clone_open_skills_repo(repo_dir: &Path) -> bool {
    if let Some(parent) = repo_dir.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                "failed to create open-skills parent directory {}: {err}",
                parent.display()
            );
            return false;
        }
    }

    let output = Command::new("git")
        .args(["clone", "--depth", "1", OPEN_SKILLS_REPO_URL])
        .arg(repo_dir)
        .output();

    match output {
        Ok(result) if result.status.success() => {
            tracing::info!("initialized open-skills at {}", repo_dir.display());
            true
        }
        Ok(result) => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            tracing::warn!("failed to clone open-skills: {stderr}");
            false
        }
        Err(err) => {
            tracing::warn!("failed to run git clone for open-skills: {err}");
            false
        }
    }
}

fn pull_open_skills_repo(repo_dir: &Path) -> bool {
    // If user points to a non-git directory via env var, keep using it without pulling.
    if !repo_dir.join(".git").exists() {
        return true;
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["pull", "--ff-only"])
        .output();

    match output {
        Ok(result) if result.status.success() => true,
        Ok(result) => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            tracing::warn!("failed to pull open-skills updates: {stderr}");
            false
        }
        Err(err) => {
            tracing::warn!("failed to run git pull for open-skills: {err}");
            false
        }
    }
}

fn should_sync_open_skills(repo_dir: &Path) -> bool {
    let marker = repo_dir.join(OPEN_SKILLS_SYNC_MARKER);
    let Ok(metadata) = std::fs::metadata(marker) else {
        return true;
    };
    let Ok(modified_at) = metadata.modified() else {
        return true;
    };
    let Ok(age) = SystemTime::now().duration_since(modified_at) else {
        return true;
    };

    age >= Duration::from_secs(OPEN_SKILLS_SYNC_INTERVAL_SECS)
}

fn mark_open_skills_synced(repo_dir: &Path) -> Result<()> {
    std::fs::write(repo_dir.join(OPEN_SKILLS_SYNC_MARKER), b"synced")?;
    Ok(())
}

/// Load a skill from a SKILL.toml manifest
fn load_skill_toml(path: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let manifest: SkillManifest = toml::from_str(&content)?;

    Ok(Skill {
        name: manifest.skill.name,
        description: manifest.skill.description,
        version: manifest.skill.version,
        author: manifest.skill.author,
        tags: manifest.skill.tags,
        tools: manifest.tools,
        prompts: manifest.prompts,
        location: Some(path.to_path_buf()),
        requires: SkillRequires::default(),
        install_recipes: Vec::new(),
    })
}

/// Load a skill from a SKILL.md file (simpler format)
fn load_skill_md(path: &Path, dir: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let frontmatter = parse_yaml_frontmatter(&content);
    let description = frontmatter
        .get("description")
        .cloned()
        .unwrap_or_else(|| extract_description(&content));
    let version = frontmatter
        .get("version")
        .cloned()
        .unwrap_or_else(|| "0.1.0".to_string());
    let tags = frontmatter
        .get("tags")
        .map(|s| parse_yaml_list(s))
        .unwrap_or_default();
    let frontmatter_name = frontmatter.get("name").cloned();
    let (requires, install_recipes) = parse_skill_metadata(&content, &frontmatter);

    Ok(Skill {
        name: frontmatter_name.unwrap_or(name),
        description,
        version,
        author: frontmatter.get("author").cloned(),
        tags,
        tools: Vec::new(),
        prompts: vec![content],
        location: Some(path.to_path_buf()),
        requires,
        install_recipes,
    })
}

/// Extract `SkillRequires` from SKILL.md frontmatter using two well-known
/// ClawHub conventions:
///
/// 1. `metadata: {"clawdbot":{"requires":{"bins":[...]},"os":[...]}}` — the
///    inline-JSON-in-YAML shape used by most ClawHub skills (weather, gog,
///    openai-whisper, github via `requires.bins`).
/// 2. Top-level `env:` YAML list (freeride-style) where each entry is a
///    table with `name` / `required`.
///
/// Both shapes are best-effort parsed without pulling in a full YAML
/// dependency — sufficient for the conventions ClawHub actually publishes.
fn parse_skill_requires(
    content: &str,
    frontmatter: &std::collections::HashMap<String, String>,
) -> SkillRequires {
    parse_skill_metadata(content, frontmatter).0
}

/// Parse both `requires` and `install[]` from SKILL.md frontmatter in
/// one pass. Returned as a tuple so callers that only need one don't
/// pay for the other; `parse_skill_requires` is a thin alias.
fn parse_skill_metadata(
    content: &str,
    frontmatter: &std::collections::HashMap<String, String>,
) -> (SkillRequires, Vec<SkillInstallRecipe>) {
    let mut req = SkillRequires::default();
    let mut recipes: Vec<SkillInstallRecipe> = Vec::new();

    // Shape 1: metadata: {"clawdbot": {...}}
    if let Some(metadata_raw) = frontmatter.get("metadata") {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(metadata_raw) {
            // Accept either the canonical `clawdbot` key (current ClawHub
            // convention) or `openclaw` (older OpenClaw docs convention).
            for ns in ["clawdbot", "openclaw"] {
                if let Some(scoped) = json.get(ns) {
                    if let Some(requires) = scoped.get("requires") {
                        if let Some(bins) = requires.get("bins").and_then(|v| v.as_array()) {
                            req.bins
                                .extend(bins.iter().filter_map(|v| v.as_str().map(String::from)));
                        }
                        if let Some(env) = requires.get("env").and_then(|v| v.as_array()) {
                            req.env
                                .extend(env.iter().filter_map(|v| v.as_str().map(String::from)));
                        }
                    }
                    if let Some(os) = scoped.get("os").and_then(|v| v.as_array()) {
                        req.os
                            .extend(os.iter().filter_map(|v| v.as_str().map(String::from)));
                    }
                    // install[]: each entry is a recipe object. Common
                    // fields: id, kind, bins[], label, os[]; per-kind:
                    // formula | pkg | module | url | archive | …
                    if let Some(install) = scoped.get("install").and_then(|v| v.as_array()) {
                        for entry in install {
                            let mut recipe = SkillInstallRecipe::default();
                            if let Some(s) = entry.get("id").and_then(|v| v.as_str()) {
                                recipe.id = s.into();
                            }
                            if let Some(s) = entry.get("kind").and_then(|v| v.as_str()) {
                                recipe.kind = s.into();
                            }
                            if let Some(arr) = entry.get("bins").and_then(|v| v.as_array()) {
                                recipe.bins.extend(
                                    arr.iter().filter_map(|v| v.as_str().map(String::from)),
                                );
                            }
                            if let Some(s) = entry.get("label").and_then(|v| v.as_str()) {
                                recipe.label = s.into();
                            }
                            if let Some(arr) = entry.get("os").and_then(|v| v.as_array()) {
                                recipe.os.extend(
                                    arr.iter().filter_map(|v| v.as_str().map(String::from)),
                                );
                            }
                            recipe.formula = entry
                                .get("formula")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            recipe.pkg = entry
                                .get("pkg")
                                .or_else(|| entry.get("package"))
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            recipe.module = entry
                                .get("module")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            recipe.url =
                                entry.get("url").and_then(|v| v.as_str()).map(String::from);
                            recipe.archive = entry
                                .get("archive")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            recipe.strip_components = entry
                                .get("stripComponents")
                                .and_then(|v| v.as_u64())
                                .map(|n| n as usize);
                            recipe.target_dir = entry
                                .get("targetDir")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            if !recipe.kind.is_empty() {
                                recipes.push(recipe);
                            }
                        }
                    }
                }
            }
        }
    }

    // Shape 2: top-level `env:` block. We re-parse the full frontmatter
    // body for this — `parse_yaml_frontmatter` is line-oriented and loses
    // the multi-line list structure.
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
    {
        if let Some(end) = rest.find("\n---") {
            for var in extract_yaml_env_block(&rest[..end]) {
                if !req.env.iter().any(|e| e == &var) {
                    req.env.push(var);
                }
            }
        }
    }

    (req, recipes)
}

/// Pull `name: FOO_BAR` entries out of a top-level `env:` YAML block.
///
/// Recognized shape (freeride-style):
/// ```yaml
/// env:
///   - name: OPENROUTER_API_KEY
///     required: true
///   - name: OTHER_VAR
/// ```
/// Lines that aren't part of an `env:` block are ignored. Skips entries
/// where `required: false` is explicit.
fn extract_yaml_env_block(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_env = false;
    let mut env_indent: Option<usize> = None;
    let mut current_name: Option<String> = None;
    let mut current_required = true;

    let flush = |out: &mut Vec<String>, name: &mut Option<String>, required: &mut bool| {
        if let Some(n) = name.take() {
            if *required {
                out.push(n);
            }
        }
        *required = true;
    };

    for line in body.lines() {
        let indent = line.chars().take_while(|c| *c == ' ').count();
        let trimmed = line.trim_start();

        if !in_env {
            if trimmed.starts_with("env:") {
                in_env = true;
            }
            continue;
        }

        // Exit env: block when we hit a less-indented top-level key.
        if !trimmed.is_empty() && !trimmed.starts_with('-') && !trimmed.starts_with('#') {
            let is_kv = trimmed.contains(':');
            if is_kv && env_indent.map_or(true, |i| indent <= i.saturating_sub(2)) {
                flush(&mut out, &mut current_name, &mut current_required);
                break;
            }
        }

        if trimmed.starts_with("- ") {
            // New entry — flush previous.
            flush(&mut out, &mut current_name, &mut current_required);
            env_indent = Some(indent);
            let item = trimmed.trim_start_matches("- ").trim();
            if let Some((k, v)) = item.split_once(':') {
                let key = k.trim();
                let val = v.trim();
                if key == "name" {
                    current_name = Some(val.trim_matches('"').trim_matches('\'').to_string());
                } else if key == "required" && val.eq_ignore_ascii_case("false") {
                    current_required = false;
                }
            }
        } else if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if key == "name" {
                current_name = Some(val.trim_matches('"').trim_matches('\'').to_string());
            } else if key == "required" && val.eq_ignore_ascii_case("false") {
                current_required = false;
            }
        }
    }

    flush(&mut out, &mut current_name, &mut current_required);
    out
}

/// Parse a minimal YAML frontmatter block at the top of a SKILL.md file.
/// Recognizes the `---\nkey: value\n...\n---` shape and extracts simple
/// scalar key/value pairs. Lists like `tags: [a, b]` are kept as the raw
/// string (callers parse with `parse_yaml_list`). Not a full YAML parser —
/// covers the SKILL.md frontmatter convention used by ClawHub skills.
fn parse_yaml_frontmatter(content: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let trimmed = content.trim_start();
    let Some(rest) = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
    else {
        return out;
    };
    let Some(end) = rest.find("\n---") else {
        return out;
    };
    for line in rest[..end].lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let value = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if !key.is_empty() && !value.is_empty() {
                out.insert(key, value);
            }
        }
    }
    out
}

/// Test-only re-export of the private frontmatter parser so sibling modules
/// (e.g. `tools::author_skill`) can assert that what they generate actually
/// satisfies the real loader, not a copy of it.
#[cfg(test)]
pub(crate) fn test_parse_frontmatter(content: &str) -> std::collections::HashMap<String, String> {
    parse_yaml_frontmatter(content)
}

fn parse_yaml_list(raw: &str) -> Vec<String> {
    let s = raw.trim();
    let inner = s
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(s);
    inner
        .split(',')
        .map(|p| p.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn load_open_skill_md(path: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("open-skill")
        .to_string();

    Ok(Skill {
        name,
        description: extract_description(&content),
        version: "open-skills".to_string(),
        author: Some("besoeasy/open-skills".to_string()),
        tags: vec!["open-skills".to_string()],
        tools: Vec::new(),
        prompts: vec![content],
        location: Some(path.to_path_buf()),
        requires: SkillRequires::default(),
        install_recipes: Vec::new(),
    })
}

fn extract_description(content: &str) -> String {
    content
        .lines()
        .find(|line| !line.starts_with('#') && !line.trim().is_empty())
        .unwrap_or("No description")
        .trim()
        .to_string()
}

fn append_xml_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
}

fn write_xml_text_element(out: &mut String, indent: usize, tag: &str, value: &str) {
    for _ in 0..indent {
        out.push(' ');
    }
    out.push('<');
    out.push_str(tag);
    out.push('>');
    append_xml_escaped(out, value);
    out.push_str("</");
    out.push_str(tag);
    out.push_str(">\n");
}

fn resolve_skill_location(skill: &Skill, workspace_dir: &Path) -> PathBuf {
    skill.location.clone().unwrap_or_else(|| {
        workspace_dir
            .join("skills")
            .join(&skill.name)
            .join("SKILL.md")
    })
}

fn render_skill_location(skill: &Skill, workspace_dir: &Path, prefer_relative: bool) -> String {
    let location = resolve_skill_location(skill, workspace_dir);
    if prefer_relative {
        if let Ok(relative) = location.strip_prefix(workspace_dir) {
            return relative.display().to_string();
        }
    }
    location.display().to_string()
}

/// Build the "Available Skills" system prompt section with full skill instructions.
pub fn skills_to_prompt(skills: &[Skill], workspace_dir: &Path) -> String {
    skills_to_prompt_with_mode(
        skills,
        workspace_dir,
        crate::config::SkillsPromptInjectionMode::Full,
    )
}

/// Build the "Available Skills" system prompt section with configurable verbosity.
pub fn skills_to_prompt_with_mode(
    skills: &[Skill],
    workspace_dir: &Path,
    mode: crate::config::SkillsPromptInjectionMode,
) -> String {
    use std::fmt::Write;

    if skills.is_empty() {
        return String::new();
    }

    let mut prompt = match mode {
        crate::config::SkillsPromptInjectionMode::Full => String::from(
            "## Available Skills\n\n\
             Skill instructions and tool metadata are preloaded below.\n\
             Follow these instructions directly; do not read skill files at runtime unless the user asks.\n\n\
             <available_skills>\n",
        ),
        crate::config::SkillsPromptInjectionMode::Compact => String::from(
            "## Available Skills\n\n\
             Skill summaries are preloaded below to keep context compact.\n\
             Skill instructions are loaded on demand: read the skill file in `location` only when needed.\n\n\
             <available_skills>\n",
        ),
    };

    for skill in skills {
        let _ = writeln!(prompt, "  <skill>");
        write_xml_text_element(&mut prompt, 4, "name", &skill.name);
        write_xml_text_element(&mut prompt, 4, "description", &skill.description);
        let location = render_skill_location(
            skill,
            workspace_dir,
            matches!(mode, crate::config::SkillsPromptInjectionMode::Compact),
        );
        write_xml_text_element(&mut prompt, 4, "location", &location);

        if matches!(mode, crate::config::SkillsPromptInjectionMode::Full) {
            if !skill.prompts.is_empty() {
                let _ = writeln!(prompt, "    <instructions>");
                for instruction in &skill.prompts {
                    write_xml_text_element(&mut prompt, 6, "instruction", instruction);
                }
                let _ = writeln!(prompt, "    </instructions>");
            }

            if !skill.tools.is_empty() {
                let _ = writeln!(prompt, "    <tools>");
                for tool in &skill.tools {
                    let _ = writeln!(prompt, "      <tool>");
                    write_xml_text_element(&mut prompt, 8, "name", &tool.name);
                    write_xml_text_element(&mut prompt, 8, "description", &tool.description);
                    write_xml_text_element(&mut prompt, 8, "kind", &tool.kind);
                    let _ = writeln!(prompt, "      </tool>");
                }
                let _ = writeln!(prompt, "    </tools>");
            }
        }

        let _ = writeln!(prompt, "  </skill>");
    }

    prompt.push_str("</available_skills>");
    prompt
}

/// Get the skills directory path
pub fn skills_dir(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("skills")
}

/// Initialize the skills directory with a README
pub fn init_skills_dir(workspace_dir: &Path) -> Result<()> {
    let dir = skills_dir(workspace_dir);
    std::fs::create_dir_all(&dir)?;

    let readme = dir.join("README.md");
    if !readme.exists() {
        std::fs::write(
            &readme,
            "# RantaiClaw Skills\n\n\
             Each subdirectory is a skill. Create a `SKILL.toml` or `SKILL.md` file inside.\n\n\
             ## SKILL.toml format\n\n\
             ```toml\n\
             [skill]\n\
             name = \"my-skill\"\n\
             description = \"What this skill does\"\n\
             version = \"0.1.0\"\n\
             author = \"your-name\"\n\
             tags = [\"productivity\", \"automation\"]\n\n\
             [[tools]]\n\
             name = \"my_tool\"\n\
             description = \"What this tool does\"\n\
             kind = \"shell\"\n\
             command = \"echo hello\"\n\
             ```\n\n\
             ## SKILL.md format (simpler)\n\n\
             Just write a markdown file with instructions for the agent.\n\
             The agent will read it and follow the instructions.\n\n\
             ## Installing community skills\n\n\
             ```bash\n\
             rantaiclaw skills install <source>\n\
             rantaiclaw skills list\n\
             ```\n",
        )?;
    }

    Ok(())
}

fn is_git_source(source: &str) -> bool {
    is_git_scheme_source(source, "https://")
        || is_git_scheme_source(source, "http://")
        || is_git_scheme_source(source, "ssh://")
        || is_git_scheme_source(source, "git://")
        || is_git_scp_source(source)
}

fn is_git_scheme_source(source: &str, scheme: &str) -> bool {
    let Some(rest) = source.strip_prefix(scheme) else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('/') {
        return false;
    }

    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty()
}

fn is_git_scp_source(source: &str) -> bool {
    // SCP-like syntax accepted by git, e.g. git@host:owner/repo.git
    // Keep this strict enough to avoid treating local paths as git remotes.
    let Some((user_host, remote_path)) = source.split_once(':') else {
        return false;
    };
    if remote_path.is_empty() {
        return false;
    }
    if source.contains("://") {
        return false;
    }

    let Some((user, host)) = user_host.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && !host.is_empty()
        && !user.contains('/')
        && !user.contains('\\')
        && !host.contains('/')
        && !host.contains('\\')
}

/// Recursively copy a directory (used as fallback when symlinks aren't available)
#[cfg(any(windows, not(unix)))]
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

/// Whether `dir` (an existing skill directory named `slug`) is something
/// `skills update` may safely delete-and-refetch from ClawHub.
///
/// Skills carry no persisted origin marker, so this is a heuristic over the
/// two signals we do have (see plan 035 "Maintenance notes" — replace with a
/// durable origin field if one is ever added, and delete this inference):
///   - a **symlink** in `profile.skills_dir()` is a local-path install
///     (`skills install <path>`, `:1354-1376`) — its target lives elsewhere
///     and `update` has nothing to fetch from ClawHub for it.
///   - a directory matching a bundled `CORE_PACK`/`STARTER_PACK` slug has no
///     ClawHub origin and would 404 on fetch.
///
/// Both cases must be skipped, not deleted — `git pull`/reinstall those
/// manually per the `Update` help text (`src/main.rs:1261-1266`).
fn is_clawhub_managed_skill(dir: &Path, slug: &str) -> bool {
    let is_bundled = crate::skills::bundled::CORE_PACK
        .iter()
        .chain(crate::skills::bundled::STARTER_PACK.iter())
        .any(|entry| entry.slug == slug);
    if is_bundled {
        return false;
    }
    // symlink_metadata (not metadata) so we classify by the entry itself,
    // not by following it — a dangling symlink is still a local install.
    std::fs::symlink_metadata(dir)
        .map(|meta| !meta.file_type().is_symlink())
        .unwrap_or(false)
}

/// Handle the `skills` CLI command
#[allow(clippy::too_many_lines)]
pub(crate) fn handle_command(
    command: crate::SkillCommands,
    config: &crate::config::Config,
) -> Result<()> {
    let workspace_dir = &config.workspace_dir;
    match command {
        crate::SkillCommands::List => {
            let with_status = load_skills_with_status(workspace_dir, config);
            if with_status.is_empty() {
                println!("No skills installed.");
                println!();
                println!("  Create one: mkdir -p ~/.rantaiclaw/workspace/skills/my-skill");
                println!("              echo '# My Skill' > ~/.rantaiclaw/workspace/skills/my-skill/SKILL.md");
                println!();
                println!("  Or install: rantaiclaw skills install <source>");
            } else {
                let active_count = with_status.iter().filter(|(_, r)| r.is_empty()).count();
                let gated_count = with_status.len() - active_count;
                if gated_count == 0 {
                    println!("Installed skills ({active_count}):");
                } else {
                    println!(
                        "Installed skills ({} active, {} gated out):",
                        active_count, gated_count
                    );
                }
                println!();
                for (skill, reasons) in &with_status {
                    let active = reasons.is_empty();
                    let glyph = if active { "✓" } else { "✗" };
                    let glyph = if active {
                        console::style(glyph).green().to_string()
                    } else {
                        console::style(glyph).red().to_string()
                    };
                    println!(
                        "  {} {} {} — {}",
                        glyph,
                        console::style(&skill.name).white().bold(),
                        console::style(format!("v{}", skill.version)).dim(),
                        skill.description
                    );
                    if !skill.tools.is_empty() {
                        println!(
                            "    Tools: {}",
                            skill
                                .tools
                                .iter()
                                .map(|t| t.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    if !skill.tags.is_empty() {
                        println!("    Tags:  {}", skill.tags.join(", "));
                    }
                    if !reasons.is_empty() {
                        println!(
                            "    {}: {}",
                            console::style("gated").red(),
                            reasons.join("; ")
                        );
                        // Show the install-deps hint when the skill ships
                        // recipes that could fix a missing-binary
                        // gating. Mirrors OpenClaw's macOS-Skills-UI
                        // "one-tap install" affordance, just text-based.
                        let has_missing_bin =
                            reasons.iter().any(|r| r.starts_with("missing binary"));
                        if has_missing_bin && !skill.install_recipes.is_empty() {
                            println!(
                                "    {} run `rantaiclaw skills install-deps {}` to fix",
                                console::style("→").cyan(),
                                skill.name
                            );
                        }
                    }
                }
            }
            println!();
            Ok(())
        }
        crate::SkillCommands::Show { name } => {
            let skills = load_skills_with_config(workspace_dir, config);
            let found = skills.iter().find(|s| s.name.eq_ignore_ascii_case(&name));
            match found {
                Some(s) => {
                    println!(
                        "{} {}",
                        console::style(&s.name).white().bold(),
                        if s.version.is_empty() {
                            String::new()
                        } else {
                            console::style(format!("· v{}", s.version))
                                .dim()
                                .to_string()
                        }
                    );
                    if !s.description.is_empty() {
                        println!("  {}", s.description);
                    }
                    if !s.tags.is_empty() {
                        println!("  Tags:  {}", s.tags.join(", "));
                    }
                    if !s.tools.is_empty() {
                        println!(
                            "  Tools: {}",
                            s.tools
                                .iter()
                                .map(|t| t.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    Ok(())
                }
                None => anyhow::bail!("No skill named '{name}'. Run `rantaiclaw skills list`."),
            }
        }
        crate::SkillCommands::Enable { name } => set_enabled_and_report(config, &name, true),
        crate::SkillCommands::Disable { name } => set_enabled_and_report(config, &name, false),
        crate::SkillCommands::Install { source } => {
            println!("Installing skill from: {source}");

            let skills_path = skills_dir(workspace_dir);
            std::fs::create_dir_all(&skills_path)?;

            // Bare slug (e.g. "weather") with no path separator and no URL
            // scheme → try ClawHub. This mirrors the TUI `/skills install <slug>`
            // path and keeps CLI/TUI surfaces in parity.
            if !is_git_source(&source)
                && !source.contains('/')
                && !source.contains('\\')
                && !source.starts_with('.')
                && !source.starts_with('~')
            {
                let profile = crate::profile::ProfileManager::active()
                    .context("resolve active profile for ClawHub install")?;
                // Caller is already inside a tokio runtime (main is `#[tokio::main]`),
                // so `Runtime::new().block_on` would panic. Spawn a fresh OS thread
                // for an isolated runtime — the install is short, so the
                // synchronous wait is acceptable.
                let slug_for_thread = source.clone();
                let profile_for_thread = profile.clone();
                eprintln!(
                    "  → ClawHub install: profile={} skills_dir={}",
                    profile.name,
                    profile.skills_dir().display()
                );
                let join = std::thread::spawn(move || -> Result<()> {
                    let rt = tokio::runtime::Runtime::new()
                        .context("build tokio runtime for clawhub install")?;
                    rt.block_on(crate::skills::clawhub::install_one(
                        &profile_for_thread,
                        &slug_for_thread,
                    ))
                    .with_context(|| format!("install_one({slug_for_thread})"))?;
                    Ok(())
                });
                let inner_result = join
                    .join()
                    .map_err(|_| anyhow::anyhow!("ClawHub install thread panicked"))?;
                inner_result.with_context(|| format!("ClawHub install of `{source}` failed"))?;
                println!(
                    "  {} Installed `{source}` from ClawHub.",
                    console::style("✓").green().bold()
                );
                return Ok(());
            }

            if is_git_source(&source) {
                // Git clone
                let output = std::process::Command::new("git")
                    .args(["clone", "--depth", "1", &source])
                    .current_dir(&skills_path)
                    .output()?;

                if output.status.success() {
                    println!(
                        "  {} Skill installed successfully!",
                        console::style("✓").green().bold()
                    );
                    println!("  Restart `rantaiclaw channel start` to activate.");
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("Git clone failed: {stderr}");
                }
            } else {
                // Local path — symlink or copy
                let src = PathBuf::from(&source);
                if !src.exists() {
                    anyhow::bail!("Source path does not exist: {source}");
                }
                let name = src.file_name().unwrap_or_default();
                let dest = skills_path.join(name);

                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&src, &dest)?;
                    println!(
                        "  {} Skill linked: {}",
                        console::style("✓").green().bold(),
                        dest.display()
                    );
                }
                #[cfg(windows)]
                {
                    // On Windows, try symlink first (requires admin or developer mode),
                    // fall back to directory junction, then copy
                    use std::os::windows::fs::symlink_dir;
                    if symlink_dir(&src, &dest).is_ok() {
                        println!(
                            "  {} Skill linked: {}",
                            console::style("✓").green().bold(),
                            dest.display()
                        );
                    } else {
                        // Try junction as fallback (works without admin)
                        let junction_result = std::process::Command::new("cmd")
                            .args(["/C", "mklink", "/J"])
                            .arg(&dest)
                            .arg(&src)
                            .output();

                        if junction_result.is_ok() && junction_result.unwrap().status.success() {
                            println!(
                                "  {} Skill linked (junction): {}",
                                console::style("✓").green().bold(),
                                dest.display()
                            );
                        } else {
                            // Final fallback: copy the directory
                            copy_dir_recursive(&src, &dest)?;
                            println!(
                                "  {} Skill copied: {}",
                                console::style("✓").green().bold(),
                                dest.display()
                            );
                        }
                    }
                }
                #[cfg(not(any(unix, windows)))]
                {
                    // On other platforms, copy the directory
                    copy_dir_recursive(&src, &dest)?;
                    println!(
                        "  {} Skill copied: {}",
                        console::style("✓").green().bold(),
                        dest.display()
                    );
                }
            }

            Ok(())
        }
        crate::SkillCommands::Remove { name } => {
            let canonical = remove_skill(workspace_dir, config, &name)?;

            // Bundled/core skills (starter pack + owner-permissions) are
            // re-seeded on the next setup/channel-config run by design
            // (`install_core_skills`, `src/onboard/section/channels.rs:150`).
            // Warn, but do not block the removal.
            let is_bundled = crate::skills::bundled::CORE_PACK
                .iter()
                .chain(crate::skills::bundled::STARTER_PACK.iter())
                .any(|entry| entry.slug.eq_ignore_ascii_case(&canonical));
            if is_bundled {
                println!(
                    "  {} '{}' is a bundled skill and may be re-installed by a future `setup`/channel-config run.",
                    console::style("⚠").yellow().bold(),
                    canonical
                );
            }

            println!(
                "  {} Skill '{}' removed.",
                console::style("✓").green().bold(),
                canonical
            );
            Ok(())
        }
        crate::SkillCommands::Update { slug, all } => {
            let profile =
                crate::profile::ProfileManager::active().context("resolve active profile")?;

            let targets: Vec<String> = if all {
                // Enumerate real installed **slug directories** under
                // `profile.skills_dir()` — the only place `install_one`
                // writes to, and the same key the per-slug lookup below
                // joins against. (Previously this read the loaded
                // manifest's `skill.name`, which can differ from the
                // on-disk slug and made the "not installed locally" check
                // below false — silently skipping skills whose manifest
                // name didn't match their directory name.)
                let dir = profile.skills_dir();
                let mut slugs = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.join("SKILL.toml").exists() || path.join("SKILL.md").exists() {
                            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                                slugs.push(name.to_string());
                            }
                        }
                    }
                }
                slugs
            } else if let Some(s) = slug {
                vec![s]
            } else {
                anyhow::bail!("`skills update` needs either a slug or `--all`");
            };

            if targets.is_empty() {
                println!("Nothing to update — no installed skills.");
                return Ok(());
            }

            // Run the network-driven update inside an isolated tokio
            // runtime on a fresh OS thread. The outer thread is already
            // inside `#[tokio::main]`, so calling `block_on` here would
            // panic ("Cannot start a runtime from within a runtime").
            let result = std::thread::spawn(move || -> Result<(usize, usize, usize)> {
                let rt = tokio::runtime::Runtime::new().context("build tokio runtime")?;
                let (mut updated, mut skipped, mut failed) = (0usize, 0usize, 0usize);
                for slug in &targets {
                    let dir = profile.skills_dir().join(slug);
                    if !dir.exists() {
                        println!("  ⊘ {slug}: not installed locally — skipping");
                        skipped += 1;
                        continue;
                    }
                    if !is_clawhub_managed_skill(&dir, slug) {
                        println!("  ⊘ {slug}: not a ClawHub skill — skipping");
                        skipped += 1;
                        continue;
                    }

                    // Non-destructive swap. `install_one` skips-if-exists
                    // and only cleans up *its own* partial dir on failure
                    // (`clawhub.rs:368-386`) — it never restores what a
                    // caller deleted beforehand. So instead of removing
                    // `dir` up front (the old, data-losing sequence), stage
                    // it aside first: rename it out of the way (into a
                    // sibling of `skills/`, not inside it, so the skill
                    // loader — which only scans `profile.skills_dir()` and
                    // the two workspace-relative skill roots, see
                    // `load_workspace_skills`, `:298-346` — never sees the
                    // backup as a phantom skill mid-swap), let
                    // `install_one` write a fresh `dir` from scratch, and
                    // only discard the backup once that fetch has proven
                    // good. On any failure the backup is renamed back, so
                    // the prior install is never lost (Reversibility,
                    // CLAUDE.md §3.8) — the core fix for the "404 wipes the
                    // skill" data-loss bug.
                    let backup_root = profile.root.join(".skills-update-tmp");
                    if let Err(e) = std::fs::create_dir_all(&backup_root) {
                        println!("  ✗ {slug}: failed to prepare backup dir: {e}");
                        failed += 1;
                        continue;
                    }
                    let backup = backup_root.join(slug);
                    // Clear any stale backup left by a prior crashed run
                    // before staging this one.
                    let _ = std::fs::remove_dir_all(&backup);

                    if let Err(e) = std::fs::rename(&dir, &backup) {
                        println!("  ✗ {slug}: failed to stage update: {e}");
                        failed += 1;
                        continue;
                    }

                    match rt.block_on(crate::skills::clawhub::install_one(&profile, slug)) {
                        Ok(()) => {
                            let _ = std::fs::remove_dir_all(&backup);
                            println!("  {} {slug}: updated", console::style("✓").green().bold());
                            updated += 1;
                        }
                        Err(e) => {
                            // Restore the backup. `install_one` already
                            // cleans up its own partial dir on error, but
                            // guard against any leftover before swapping
                            // the backup back into place.
                            let _ = std::fs::remove_dir_all(&dir);
                            match std::fs::rename(&backup, &dir) {
                                Ok(()) => {
                                    println!("  ✗ {slug}: {e} (kept existing version)");
                                }
                                Err(re) => {
                                    println!(
                                        "  ✗ {slug}: {e} (restore failed: {re} — prior version backed up at {})",
                                        backup.display()
                                    );
                                }
                            }
                            failed += 1;
                        }
                    }
                }
                Ok((updated, skipped, failed))
            })
            .join()
            .map_err(|_| anyhow::anyhow!("update thread panicked"))??;

            let (updated, skipped, failed) = result;
            println!();
            println!("Update summary: {updated} updated, {skipped} skipped, {failed} failed");
            if failed > 0 {
                anyhow::bail!("{failed} skill(s) failed to update");
            }
            Ok(())
        }
        crate::SkillCommands::Inspect { slug } => {
            // Same pattern as Update — isolated runtime on a fresh thread.
            std::thread::spawn(move || -> Result<()> {
                let rt = tokio::runtime::Runtime::new().context("build tokio runtime")?;
                rt.block_on(crate::skills::clawhub::inspect_to_stdout(&slug))
            })
            .join()
            .map_err(|_| anyhow::anyhow!("inspect thread panicked"))?
        }
        crate::SkillCommands::InstallDeps { slug, all } => {
            let with_status = load_skills_with_status(workspace_dir, config);

            // Build the list of (skill, missing-bins) targets.
            let targets: Vec<&Skill> = if all {
                with_status
                    .iter()
                    .filter(|(s, _)| !s.requires.bins.is_empty())
                    .map(|(s, _)| s)
                    .collect()
            } else if let Some(s) = slug.as_ref() {
                let found = with_status
                    .iter()
                    .find(|(skill, _)| skill.name.eq_ignore_ascii_case(s))
                    .map(|(skill, _)| skill);
                match found {
                    Some(s) => vec![s],
                    None => anyhow::bail!("skill `{s}` not found"),
                }
            } else {
                anyhow::bail!("`skills install-deps` needs either a slug or --all");
            };

            if targets.is_empty() {
                println!("Nothing to do — no skills declare binary requirements.");
                return Ok(());
            }

            let mut had_failure = false;
            for skill in &targets {
                if skill.install_recipes.is_empty() {
                    if !skill.requires.bins.is_empty() {
                        let missing: Vec<&str> = skill
                            .requires
                            .bins
                            .iter()
                            .filter(|b| which::which(b).is_err())
                            .map(|b| b.as_str())
                            .collect();
                        if !missing.is_empty() {
                            println!(
                                "⊘ {}: missing {} but no install recipes declared — install manually",
                                skill.name,
                                missing.join(", ")
                            );
                        }
                    }
                    continue;
                }
                let prefs = install_deps::SelectorPrefs::from_config(&config.skills.install);
                match install_deps::install_deps_for_with_prefs(skill, &prefs) {
                    Ok(outcome) if outcome.success() => {
                        println!(
                            "  {} {}: installed {}",
                            console::style("✓").green().bold(),
                            outcome.skill,
                            outcome.bins_installed.join(", ")
                        );
                    }
                    Ok(outcome) if outcome.recipe_used.is_none() => {
                        println!("  · {}: deps already satisfied", outcome.skill);
                    }
                    Ok(outcome) => {
                        had_failure = true;
                        println!(
                            "  ✗ {}: recipe ran but {} still missing",
                            outcome.skill,
                            outcome.bins_still_missing.join(", ")
                        );
                    }
                    Err(e) => {
                        had_failure = true;
                        println!("  ✗ {}: {e}", skill.name);
                    }
                }
            }

            if had_failure {
                anyhow::bail!("one or more install-deps runs failed");
            }
            Ok(())
        }
    }
}

/// Resolves `name`, flips its `enabled` flag via [`set_skill_enabled`], and
/// persists the change through `Config::save()`.
///
/// `Config::save()` is async; `handle_command` is sync but called from
/// inside `#[tokio::main]`, so `Runtime::new().block_on` would panic here
/// ("Cannot start a runtime from within a runtime"). Persist on a fresh
/// OS-thread runtime instead — mirrors the `Install` arm above.
fn set_enabled_and_report(config: &crate::config::Config, name: &str, enabled: bool) -> Result<()> {
    let (updated, canonical) = set_skill_enabled(config, name, enabled)?;
    let join = std::thread::spawn(move || -> Result<()> {
        let rt = tokio::runtime::Runtime::new()
            .context("build tokio runtime for skills enable/disable save")?;
        rt.block_on(updated.save()).context("save config")?;
        Ok(())
    });
    join.join()
        .map_err(|_| anyhow::anyhow!("skills enable/disable save thread panicked"))??;
    let state = if enabled { "enabled" } else { "disabled" };
    println!("✓ {canonical} {state}. Restart the agent (or reload) for it to take effect.");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    fn open_skills_env_lock() -> &'static Mutex<()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let original = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, original }
        }

        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.original {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    /// Isolated fake-profile + workspace harness for tests that exercise
    /// `load_skills_with_config`/`load_skills_with_status`, which call
    /// `ProfileManager::active()` (root 1, see `load_workspace_skills`) and
    /// so read the process-global `HOME`/`RANTAICLAW_PROFILE` env vars.
    /// Holds `crate::test_env::ENV_LOCK` for its entire lifetime — keep the
    /// returned harness alive for the whole test body — so no other test
    /// thread can observe or clobber these vars concurrently; both vars are
    /// restored on drop (same save/restore pattern as `EnvVarGuard`).
    struct FakeProfileEnv {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        _home: EnvVarGuard,
        _profile: EnvVarGuard,
        // Kept alive only so the backing directory isn't deleted early;
        // never read again after `new()` computes `workspace_dir` from it.
        _tempdir: tempfile::TempDir,
    }

    impl FakeProfileEnv {
        /// Point `HOME` at a fresh, empty tempdir (so root 1,
        /// `~/.rantaiclaw/profiles/<name>/skills`, has nothing in it) and
        /// unset `RANTAICLAW_PROFILE` so profile resolution falls back to
        /// `"default"`. Returns the harness plus the (not yet created)
        /// `<tempdir>/workspace` dir to use as `config.workspace_dir`.
        fn new() -> (Self, PathBuf) {
            let lock = crate::test_env::ENV_LOCK.blocking_lock();
            let tempdir = tempfile::tempdir().unwrap();
            let home = EnvVarGuard::set("HOME", &tempdir.path().to_string_lossy());
            let profile = EnvVarGuard::unset("RANTAICLAW_PROFILE");
            let workspace_dir = tempdir.path().join("workspace");
            let harness = Self {
                _lock: lock,
                _home: home,
                _profile: profile,
                _tempdir: tempdir,
            };
            (harness, workspace_dir)
        }
    }

    /// Write `<root>/<dirname>/SKILL.md`. `dirname` is the on-disk slug;
    /// `frontmatter_name`, when given, becomes the manifest `name:` in the
    /// SKILL.md frontmatter — independently of `dirname` (`load_skill_md`
    /// falls back to `dirname` when frontmatter has no `name:`). Kept
    /// settable so tests can express `dirname != name`.
    fn write_fake_skill(root: &Path, dirname: &str, frontmatter_name: Option<&str>) {
        let skill_dir = root.join(dirname);
        fs::create_dir_all(&skill_dir).unwrap();
        let body = match frontmatter_name {
            Some(name) => format!("---\nname: {name}\n---\n# {dirname}\nA test skill.\n"),
            None => format!("# {dirname}\nA test skill.\n"),
        };
        fs::write(skill_dir.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn disable_filter_excludes_config_disabled_skill() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let skills_root = workspace_dir.join("skills");
        fs::create_dir_all(&skills_root).unwrap();

        write_fake_skill(&skills_root, "skill-a", None);
        write_fake_skill(&skills_root, "skill-b", None);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;
        config.skills.entries.insert(
            "skill-b".to_string(),
            crate::config::SkillEntryConfig {
                enabled: false,
                ..Default::default()
            },
        );

        let active = load_skills_with_config(&workspace_dir, &config);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "skill-a");

        let with_status = load_skills_with_status(&workspace_dir, &config);
        assert_eq!(with_status.len(), 2);
        let (_, b_reasons) = with_status
            .iter()
            .find(|(s, _)| s.name == "skill-b")
            .expect("skill-b present in load_skills_with_status output");
        assert_eq!(
            b_reasons.first().map(String::as_str),
            Some("disabled in config.toml")
        );
    }

    #[test]
    fn load_empty_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills = load_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_skill_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("test-skill");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            r#"
[skill]
name = "test-skill"
description = "A test skill"
version = "1.0.0"
tags = ["test"]

[[tools]]
name = "hello"
description = "Says hello"
kind = "shell"
command = "echo hello"
"#,
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "test-skill");
        assert_eq!(skills[0].tools.len(), 1);
        assert_eq!(skills[0].tools[0].name, "hello");
    }

    #[test]
    fn load_skill_from_md() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("md-skill");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.md"),
            "# My Skill\nThis skill does cool things.\n",
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "md-skill");
        assert!(skills[0].description.contains("cool things"));
    }

    #[test]
    fn skills_to_prompt_empty() {
        let prompt = skills_to_prompt(&[], Path::new("/tmp"));
        assert!(prompt.is_empty());
    }

    #[test]
    fn skills_to_prompt_with_skills() {
        let skills = vec![Skill {
            name: "test".to_string(),
            description: "A test".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![],
            prompts: vec!["Do the thing.".to_string()],
            location: None,
            requires: SkillRequires::default(),
            install_recipes: Vec::new(),
        }];
        let prompt = skills_to_prompt(&skills, Path::new("/tmp"));
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>test</name>"));
        assert!(prompt.contains("<instruction>Do the thing.</instruction>"));
    }

    #[test]
    fn skills_to_prompt_compact_mode_omits_instructions_and_tools() {
        let skills = vec![Skill {
            name: "test".to_string(),
            description: "A test".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![SkillTool {
                name: "run".to_string(),
                description: "Run task".to_string(),
                kind: "shell".to_string(),
                command: "echo hi".to_string(),
                args: HashMap::new(),
            }],
            prompts: vec!["Do the thing.".to_string()],
            location: Some(PathBuf::from("/tmp/workspace/skills/test/SKILL.md")),
            requires: SkillRequires::default(),
            install_recipes: Vec::new(),
        }];
        let prompt = skills_to_prompt_with_mode(
            &skills,
            Path::new("/tmp/workspace"),
            crate::config::SkillsPromptInjectionMode::Compact,
        );

        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>test</name>"));
        assert!(prompt.contains("<location>skills/test/SKILL.md</location>"));
        assert!(prompt.contains("loaded on demand"));
        assert!(!prompt.contains("<instructions>"));
        assert!(!prompt.contains("<instruction>Do the thing.</instruction>"));
        assert!(!prompt.contains("<tools>"));
    }

    #[test]
    fn init_skills_creates_readme() {
        let dir = tempfile::tempdir().unwrap();
        init_skills_dir(dir.path()).unwrap();
        assert!(dir.path().join("skills").join("README.md").exists());
    }

    #[test]
    fn init_skills_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        init_skills_dir(dir.path()).unwrap();
        init_skills_dir(dir.path()).unwrap(); // second call should not fail
        assert!(dir.path().join("skills").join("README.md").exists());
    }

    #[test]
    fn load_nonexistent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("nonexistent");
        let skills = load_skills(&fake);
        assert!(skills.is_empty());
    }

    #[test]
    fn load_ignores_files_in_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        // A file, not a directory — should be ignored
        fs::write(skills_dir.join("not-a-skill.txt"), "hello").unwrap();
        let skills = load_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_ignores_dir_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let empty_skill = skills_dir.join("empty-skill");
        fs::create_dir_all(&empty_skill).unwrap();
        // Directory exists but no SKILL.toml or SKILL.md
        let skills = load_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_multiple_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");

        for name in ["alpha", "beta", "gamma"] {
            let skill_dir = skills_dir.join(name);
            fs::create_dir_all(&skill_dir).unwrap();
            fs::write(
                skill_dir.join("SKILL.md"),
                format!("# {name}\nSkill {name} description.\n"),
            )
            .unwrap();
        }

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 3);
    }

    #[test]
    fn toml_skill_with_multiple_tools() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("multi-tool");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            r#"
[skill]
name = "multi-tool"
description = "Has many tools"
version = "2.0.0"
author = "tester"
tags = ["automation", "devops"]

[[tools]]
name = "build"
description = "Build the project"
kind = "shell"
command = "cargo build"

[[tools]]
name = "test"
description = "Run tests"
kind = "shell"
command = "cargo test"

[[tools]]
name = "deploy"
description = "Deploy via HTTP"
kind = "http"
command = "https://api.example.com/deploy"
"#,
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        let s = &skills[0];
        assert_eq!(s.name, "multi-tool");
        assert_eq!(s.version, "2.0.0");
        assert_eq!(s.author.as_deref(), Some("tester"));
        assert_eq!(s.tags, vec!["automation", "devops"]);
        assert_eq!(s.tools.len(), 3);
        assert_eq!(s.tools[0].name, "build");
        assert_eq!(s.tools[1].kind, "shell");
        assert_eq!(s.tools[2].kind, "http");
    }

    #[test]
    fn toml_skill_minimal() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("minimal");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            r#"
[skill]
name = "minimal"
description = "Bare minimum"
"#,
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].version, "0.1.0"); // default version
        assert!(skills[0].author.is_none());
        assert!(skills[0].tags.is_empty());
        assert!(skills[0].tools.is_empty());
    }

    #[test]
    fn toml_skill_invalid_syntax_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("broken");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(skill_dir.join("SKILL.toml"), "this is not valid toml {{{{").unwrap();

        let skills = load_skills(dir.path());
        assert!(skills.is_empty()); // broken skill is skipped
    }

    #[test]
    fn md_skill_heading_only() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("heading-only");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(skill_dir.join("SKILL.md"), "# Just a Heading\n").unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "No description");
    }

    #[test]
    fn skills_to_prompt_includes_tools() {
        let skills = vec![Skill {
            name: "weather".to_string(),
            description: "Get weather".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![SkillTool {
                name: "get_weather".to_string(),
                description: "Fetch forecast".to_string(),
                kind: "shell".to_string(),
                command: "curl wttr.in".to_string(),
                args: HashMap::new(),
            }],
            prompts: vec![],
            location: None,
            requires: SkillRequires::default(),
            install_recipes: Vec::new(),
        }];
        let prompt = skills_to_prompt(&skills, Path::new("/tmp"));
        assert!(prompt.contains("weather"));
        assert!(prompt.contains("<name>get_weather</name>"));
        assert!(prompt.contains("<description>Fetch forecast</description>"));
        assert!(prompt.contains("<kind>shell</kind>"));
    }

    #[test]
    fn skills_to_prompt_escapes_xml_content() {
        let skills = vec![Skill {
            name: "xml<skill>".to_string(),
            description: "A & B".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![],
            prompts: vec!["Use <tool> & check \"quotes\".".to_string()],
            location: None,
            requires: SkillRequires::default(),
            install_recipes: Vec::new(),
        }];

        let prompt = skills_to_prompt(&skills, Path::new("/tmp"));
        assert!(prompt.contains("<name>xml&lt;skill&gt;</name>"));
        assert!(prompt.contains("<description>A &amp; B</description>"));
        assert!(prompt.contains(
            "<instruction>Use &lt;tool&gt; &amp; check &quot;quotes&quot;.</instruction>"
        ));
    }

    #[test]
    fn git_source_detection_accepts_remote_protocols_and_scp_style() {
        let sources = [
            "https://github.com/some-org/some-skill.git",
            "http://github.com/some-org/some-skill.git",
            "ssh://git@github.com/some-org/some-skill.git",
            "git://github.com/some-org/some-skill.git",
            "git@github.com:some-org/some-skill.git",
            "git@localhost:skills/some-skill.git",
        ];

        for source in sources {
            assert!(
                is_git_source(source),
                "expected git source detection for '{source}'"
            );
        }
    }

    #[test]
    fn git_source_detection_rejects_local_paths_and_invalid_inputs() {
        let sources = [
            "./skills/local-skill",
            "/tmp/skills/local-skill",
            "C:\\skills\\local-skill",
            "git@github.com",
            "ssh://",
            "not-a-url",
            "dir/git@github.com:org/repo.git",
        ];

        for source in sources {
            assert!(
                !is_git_source(source),
                "expected local/invalid source detection for '{source}'"
            );
        }
    }

    #[test]
    fn skills_dir_path() {
        let base = std::path::Path::new("/home/user/.rantaiclaw");
        let dir = skills_dir(base);
        assert_eq!(dir, PathBuf::from("/home/user/.rantaiclaw/skills"));
    }

    #[test]
    fn toml_prefers_over_md() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("dual");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            "[skill]\nname = \"from-toml\"\ndescription = \"TOML wins\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("SKILL.md"), "# From MD\nMD description\n").unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "from-toml"); // TOML takes priority
    }

    #[test]
    fn open_skills_enabled_resolution_prefers_env_then_config_then_default_false() {
        assert!(!open_skills_enabled_from_sources(None, None));
        assert!(open_skills_enabled_from_sources(Some(true), None));
        assert!(!open_skills_enabled_from_sources(Some(true), Some("0")));
        assert!(open_skills_enabled_from_sources(Some(false), Some("yes")));
        // Invalid env values should fall back to config.
        assert!(open_skills_enabled_from_sources(
            Some(true),
            Some("invalid")
        ));
        assert!(!open_skills_enabled_from_sources(
            Some(false),
            Some("invalid")
        ));
    }

    #[test]
    fn resolve_open_skills_dir_resolution_prefers_env_then_config_then_home() {
        let home = Path::new("/tmp/home-dir");
        assert_eq!(
            resolve_open_skills_dir_from_sources(
                Some("/tmp/env-skills"),
                Some("/tmp/config"),
                Some(home)
            ),
            Some(PathBuf::from("/tmp/env-skills"))
        );
        assert_eq!(
            resolve_open_skills_dir_from_sources(
                Some("   "),
                Some("/tmp/config-skills"),
                Some(home)
            ),
            Some(PathBuf::from("/tmp/config-skills"))
        );
        assert_eq!(
            resolve_open_skills_dir_from_sources(None, None, Some(home)),
            Some(PathBuf::from("/tmp/home-dir/open-skills"))
        );
        assert_eq!(resolve_open_skills_dir_from_sources(None, None, None), None);
    }

    #[test]
    fn load_skills_with_config_reads_open_skills_dir_without_network() {
        let _env_guard = open_skills_env_lock().lock().unwrap();
        let _enabled_guard = EnvVarGuard::unset("RANTAICLAW_OPEN_SKILLS_ENABLED");
        let _dir_guard = EnvVarGuard::unset("RANTAICLAW_OPEN_SKILLS_DIR");

        let dir = tempfile::tempdir().unwrap();
        let workspace_dir = dir.path().join("workspace");
        fs::create_dir_all(workspace_dir.join("skills")).unwrap();

        let open_skills_dir = dir.path().join("open-skills-local");
        fs::create_dir_all(&open_skills_dir).unwrap();
        fs::write(open_skills_dir.join("README.md"), "# open skills\n").unwrap();
        fs::write(
            open_skills_dir.join("http_request.md"),
            "# HTTP request\nFetch API responses.\n",
        )
        .unwrap();

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = true;
        config.skills.open_skills_dir = Some(open_skills_dir.to_string_lossy().to_string());

        let skills = load_skills_with_config(&workspace_dir, &config);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "http_request");
    }

    // --- `skills remove` true-uninstall tests (plan 034) -----------------

    #[test]
    fn remove_found_in_profile_dir() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let profile = crate::profile::ProfileManager::active().unwrap();
        let profile_skills = profile.skills_dir();

        write_fake_skill(&profile_skills, "clawhub-skill", None);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;

        let result = handle_command(
            crate::SkillCommands::Remove {
                name: "clawhub-skill".to_string(),
            },
            &config,
        );
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(!profile_skills.join("clawhub-skill").exists());
    }

    #[test]
    fn remove_found_in_workspace_dir() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let workspace_skills = workspace_dir.join("skills");
        fs::create_dir_all(&workspace_skills).unwrap();
        write_fake_skill(&workspace_skills, "local-skill", None);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;

        let result = handle_command(
            crate::SkillCommands::Remove {
                name: "local-skill".to_string(),
            },
            &config,
        );
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(!workspace_skills.join("local-skill").exists());
    }

    #[test]
    fn remove_resolves_by_manifest_name_when_dir_differs() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let workspace_skills = workspace_dir.join("skills");
        fs::create_dir_all(&workspace_skills).unwrap();
        write_fake_skill(&workspace_skills, "pkg-dir", Some("cool-skill"));

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;

        let result = handle_command(
            crate::SkillCommands::Remove {
                name: "cool-skill".to_string(),
            },
            &config,
        );
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(!workspace_skills.join("pkg-dir").exists());
    }

    #[test]
    fn remove_not_found_reports_error() {
        let (_env, workspace_dir) = FakeProfileEnv::new();

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;

        let result = handle_command(
            crate::SkillCommands::Remove {
                name: "nonexistent".to_string(),
            },
            &config,
        );
        let err = result.expect_err("expected Err for a nonexistent skill");
        assert!(
            err.to_string().contains("Skill not found"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn remove_rejects_traversal() {
        let (_env, workspace_dir) = FakeProfileEnv::new();

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;

        for bad_name in ["../evil", "a/b", "a\\b"] {
            let result = handle_command(
                crate::SkillCommands::Remove {
                    name: bad_name.to_string(),
                },
                &config,
            );
            let err = match result {
                Ok(()) => panic!("expected Err for traversal attempt {bad_name:?}"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("Invalid skill name"),
                "unexpected error for {bad_name:?}: {err}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn remove_symlinked_skill_uses_remove_file() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let workspace_skills = workspace_dir.join("skills");
        fs::create_dir_all(&workspace_skills).unwrap();

        // Target lives outside the workspace entirely — the common
        // `skills install /tmp/foo` flow.
        let outside_dir = workspace_dir.parent().unwrap().join("outside-target");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(
            outside_dir.join("SKILL.md"),
            "# Outside Skill\nLives elsewhere.\n",
        )
        .unwrap();

        let link = workspace_skills.join("linked-skill");
        std::os::unix::fs::symlink(&outside_dir, &link).unwrap();

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;

        let result = handle_command(
            crate::SkillCommands::Remove {
                name: "linked-skill".to_string(),
            },
            &config,
        );
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "symlink should be gone"
        );
        assert!(
            outside_dir.join("SKILL.md").exists(),
            "symlink target must survive removal of the symlink"
        );
    }

    #[test]
    fn remove_out_of_root_path_is_rejected() {
        // Recover from poisoning rather than `.unwrap()`: this mutex is
        // shared with `load_skills_with_config_reads_open_skills_dir_without_network`,
        // a pre-existing test that is non-hermetic on this box (reads the
        // real `$HOME` profile) and can itself panic while holding the lock.
        // That test's brokenness is out of scope for this plan; this test
        // must not spuriously fail just because it ran second.
        let _env_guard = open_skills_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _enabled_guard = EnvVarGuard::unset("RANTAICLAW_OPEN_SKILLS_ENABLED");
        let _dir_guard = EnvVarGuard::unset("RANTAICLAW_OPEN_SKILLS_DIR");

        let (_env, workspace_dir) = FakeProfileEnv::new();
        fs::create_dir_all(workspace_dir.join("skills")).unwrap();

        // A skill loaded from the flat open-skills repo has `location` set
        // directly under the repo root (see `load_open_skill_md`), not
        // inside a per-skill directory under any of the three known
        // install roots. The containment gate must refuse to touch it
        // rather than guess.
        let open_skills_dir = workspace_dir.parent().unwrap().join("open-skills-repo");
        fs::create_dir_all(&open_skills_dir).unwrap();
        fs::write(
            open_skills_dir.join("custom-skill.md"),
            "# Custom Skill\nA flat open-skills entry.\n",
        )
        .unwrap();

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = true;
        config.skills.open_skills_dir = Some(open_skills_dir.to_string_lossy().to_string());

        let result = handle_command(
            crate::SkillCommands::Remove {
                name: "custom-skill".to_string(),
            },
            &config,
        );
        let err = result.expect_err("expected Err — out-of-root delete must be refused");
        assert!(
            err.to_string().contains("escapes known skills roots"),
            "unexpected error: {err}"
        );
        assert!(
            open_skills_dir.join("custom-skill.md").exists(),
            "nothing should have been deleted"
        );
    }

    // --- `skills update` non-destructive swap tests (plan 035) -----------

    /// Minimal blocking mock ClawHub server for `update_*` tests. Serves the
    /// three endpoints `install_one` walks (`GET /skills/<slug>`, `GET
    /// /skills/<slug>/versions/<v>`, `GET /skills/<slug>/file?...`, modeled
    /// on `tests/onboard_skills_section.rs`'s `spawn_mock_clawhub_full`).
    /// When `fail_detail` is set, the detail endpoint always 404s (models a
    /// failed fetch) so `install_one` never gets past step 1.
    ///
    /// Runs on its own dedicated OS thread with a plain blocking
    /// `std::net::TcpListener` rather than a tokio task on the test's own
    /// runtime: the `Update` arm under test spawns *its own* fresh tokio
    /// runtime on a *different* OS thread (see `handle_command`'s `Update`
    /// arm), and these tests call `handle_command` directly from a plain
    /// `#[test]` with no runtime of its own — a std thread with a blocking
    /// listener sidesteps any risk of one runtime starving another's accept
    /// loop.
    fn spawn_mock_clawhub(body: Vec<u8>, body_sha: String, fail_detail: bool) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming() {
                let Ok(mut sock) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = match sock.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();

                let (status, ctype, resp_body): (&str, &str, Vec<u8>) = if fail_detail {
                    ("404 Not Found", "text/plain", b"not found".to_vec())
                } else if path.contains("/file?") {
                    ("200 OK", "text/markdown", body.clone())
                } else if path.contains("/versions/") {
                    let manifest = serde_json::json!({
                        "version": {
                            "version": "1.0.0",
                            "files": [{
                                "path": "SKILL.md",
                                "size": body.len(),
                                "sha256": body_sha,
                                "contentType": "text/markdown",
                            }]
                        }
                    });
                    (
                        "200 OK",
                        "application/json",
                        manifest.to_string().into_bytes(),
                    )
                } else {
                    let detail = serde_json::json!({
                        "skill": {"slug": "demo"},
                        "latestVersion": {"version": "1.0.0"}
                    });
                    (
                        "200 OK",
                        "application/json",
                        detail.to_string().into_bytes(),
                    )
                };

                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    resp_body.len()
                );
                let _ = sock.write_all(header.as_bytes());
                let _ = sock.write_all(&resp_body);
            }
        });
        format!("http://{addr}")
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }

    #[test]
    fn update_success_swaps_new_version() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let profile = crate::profile::ProfileManager::active().unwrap();
        write_fake_skill(&profile.skills_dir(), "demo", None);

        let new_body = b"# demo skill\n\nUpdated content from ClawHub.\n".to_vec();
        let new_sha = sha256_hex(&new_body);
        let base = spawn_mock_clawhub(new_body.clone(), new_sha, false);
        let _clawhub = EnvVarGuard::set(crate::skills::clawhub::CLAWHUB_BASE_URL_ENV, &base);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;

        let result = handle_command(
            crate::SkillCommands::Update {
                slug: Some("demo".to_string()),
                all: false,
            },
            &config,
        );
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        let content = fs::read(profile.skills_dir().join("demo").join("SKILL.md")).unwrap();
        assert_eq!(
            content, new_body,
            "SKILL.md must be swapped to the newly fetched content"
        );
        assert!(
            !profile
                .root
                .join(".skills-update-tmp")
                .join("demo")
                .exists(),
            "backup dir must be cleaned up after a successful update"
        );
    }

    #[test]
    fn update_failure_preserves_old_dir() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let profile = crate::profile::ProfileManager::active().unwrap();
        let profile_skills = profile.skills_dir();
        write_fake_skill(&profile_skills, "demo", None);
        let original_content = fs::read(profile_skills.join("demo").join("SKILL.md")).unwrap();

        // 404s the detail endpoint — models a failed fetch (upstream gone,
        // transient network error, or a bundled/non-ClawHub slug that
        // somehow slipped past the origin guard).
        let base = spawn_mock_clawhub(Vec::new(), String::new(), true);
        let _clawhub = EnvVarGuard::set(crate::skills::clawhub::CLAWHUB_BASE_URL_ENV, &base);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;

        let result = handle_command(
            crate::SkillCommands::Update {
                slug: Some("demo".to_string()),
                all: false,
            },
            &config,
        );
        assert!(
            result.is_err(),
            "a failed fetch must be reported as a failure, not silently swallowed"
        );

        // The regression this plan fixes: the original directory must be
        // present and byte-identical to before the update attempt — never
        // deleted before a successful fetch.
        assert!(
            profile_skills.join("demo").exists(),
            "the original skill directory must survive a failed update"
        );
        let restored = fs::read(profile_skills.join("demo").join("SKILL.md")).unwrap();
        assert_eq!(
            restored, original_content,
            "original skill content must be byte-identical after a failed update"
        );
        assert!(
            !profile
                .root
                .join(".skills-update-tmp")
                .join("demo")
                .exists(),
            "backup dir must be restored, not left behind, after a failed update"
        );
    }

    #[test]
    fn update_all_skips_non_clawhub() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let profile = crate::profile::ProfileManager::active().unwrap();
        let profile_skills = profile.skills_dir();

        // Bundled skill — a CORE_PACK slug, no ClawHub origin, would 404.
        let bundled_slug = crate::skills::bundled::CORE_PACK[0].slug;
        write_fake_skill(&profile_skills, bundled_slug, None);
        let bundled_content = fs::read(profile_skills.join(bundled_slug).join("SKILL.md")).unwrap();

        // Local-path install — a real symlink into an external dir, as
        // `skills install <path>` creates (`:1354-1376`).
        #[cfg(unix)]
        let outside_dir = {
            let outside_dir = workspace_dir.parent().unwrap().join("outside-local-skill");
            fs::create_dir_all(&outside_dir).unwrap();
            fs::write(outside_dir.join("SKILL.md"), "# Local\nLocal skill.\n").unwrap();
            std::os::unix::fs::symlink(&outside_dir, profile_skills.join("local-skill")).unwrap();
            outside_dir
        };

        // Point CLAWHUB_BASE_URL_ENV at a dead port — if the origin guard
        // is ever bypassed, the resulting network attempt fails fast and
        // loud instead of silently succeeding or hanging.
        let _clawhub = EnvVarGuard::set(
            crate::skills::clawhub::CLAWHUB_BASE_URL_ENV,
            "http://127.0.0.1:1",
        );

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;

        let result = handle_command(
            crate::SkillCommands::Update {
                slug: None,
                all: true,
            },
            &config,
        );
        assert!(
            result.is_ok(),
            "skipped non-ClawHub skills must not count as failures, got {result:?}"
        );

        let after_bundled = fs::read(profile_skills.join(bundled_slug).join("SKILL.md")).unwrap();
        assert_eq!(
            after_bundled, bundled_content,
            "bundled skill must not be modified by `update --all`"
        );
        #[cfg(unix)]
        {
            assert!(
                std::fs::symlink_metadata(profile_skills.join("local-skill"))
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false),
                "local-path symlink must survive `update --all`"
            );
            assert!(
                outside_dir.join("SKILL.md").exists(),
                "local-path symlink target must be untouched"
            );
        }
    }

    #[test]
    fn update_all_keys_by_slug_dir() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let profile = crate::profile::ProfileManager::active().unwrap();
        let profile_skills = profile.skills_dir();

        // On-disk slug differs from the manifest `name:` — the keying bug
        // this fixes: `--all` used to build its target list from
        // `skill.name` ("Weather Reporter"), which never matched the
        // `profile.skills_dir().join(slug)` lookup keyed on the directory
        // name, so the skill was falsely reported "not installed locally"
        // and silently skipped.
        write_fake_skill(&profile_skills, "weather-dir", Some("Weather Reporter"));

        let new_body = b"# weather-dir\n\nUpdated via --all.\n".to_vec();
        let new_sha = sha256_hex(&new_body);
        let base = spawn_mock_clawhub(new_body.clone(), new_sha, false);
        let _clawhub = EnvVarGuard::set(crate::skills::clawhub::CLAWHUB_BASE_URL_ENV, &base);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;

        let result = handle_command(
            crate::SkillCommands::Update {
                slug: None,
                all: true,
            },
            &config,
        );
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        let content = fs::read(profile_skills.join("weather-dir").join("SKILL.md")).unwrap();
        assert_eq!(
            content, new_body,
            "`--all` must target the on-disk slug dir, not the manifest name"
        );
    }

    // --- `skills enable`/`skills disable` tests (plan 037) ---------------

    /// Re-parse a config previously written by `Config::save()`, restoring
    /// the `#[serde(skip)]` `workspace_dir`/`config_path` fields (which
    /// deserialize to empty `PathBuf`s otherwise) from the values the test
    /// used to write it. Mirrors what `Config::load_or_init()` does for
    /// real — resolve paths out-of-band, then deserialize the rest — without
    /// re-resolving from process-global env vars.
    fn reload_config(config_path: &Path, workspace_dir: &Path) -> crate::config::Config {
        let raw = fs::read_to_string(config_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", config_path.display()));
        let mut reloaded: crate::config::Config =
            toml::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", config_path.display()));
        reloaded.workspace_dir = workspace_dir.to_path_buf();
        reloaded.config_path = config_path.to_path_buf();
        reloaded
    }

    #[test]
    fn disable_writes_entry_enabled_false_and_persists() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let skills_root = workspace_dir.join("skills");
        fs::create_dir_all(&skills_root).unwrap();
        write_fake_skill(&skills_root, "weather", None);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.config_path = workspace_dir.parent().unwrap().join("config.toml");
        config.skills.open_skills_enabled = false;

        let result = handle_command(
            crate::SkillCommands::Disable {
                name: "weather".to_string(),
            },
            &config,
        );
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        let reloaded = reload_config(&config.config_path, &workspace_dir);
        let entry = reloaded
            .skills
            .entries
            .get("weather")
            .expect("weather entry persisted");
        assert!(!entry.enabled);
    }

    #[test]
    fn enable_reverses_a_disabled_entry_and_persists() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let skills_root = workspace_dir.join("skills");
        fs::create_dir_all(&skills_root).unwrap();
        write_fake_skill(&skills_root, "weather", None);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.config_path = workspace_dir.parent().unwrap().join("config.toml");
        config.skills.open_skills_enabled = false;

        handle_command(
            crate::SkillCommands::Disable {
                name: "weather".to_string(),
            },
            &config,
        )
        .expect("disable should succeed");

        let disabled = reload_config(&config.config_path, &workspace_dir);
        assert!(
            !disabled.skills.entries.get("weather").unwrap().enabled,
            "sanity: weather must be disabled before testing re-enable"
        );

        // Regression: `set_skill_enabled` must resolve against
        // `load_skills_with_status`, not `load_skills_with_config` — the
        // latter filters out already-disabled skills, which would make
        // `enable` unable to find (and thus unable to re-enable) the very
        // skill it's being asked to enable.
        let result = handle_command(
            crate::SkillCommands::Enable {
                name: "weather".to_string(),
            },
            &disabled,
        );
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        let reenabled = reload_config(&config.config_path, &workspace_dir);
        let entry = reenabled
            .skills
            .entries
            .get("weather")
            .expect("weather entry persisted");
        assert!(entry.enabled);
    }

    #[test]
    fn disable_then_load_excludes_skill() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let skills_root = workspace_dir.join("skills");
        fs::create_dir_all(&skills_root).unwrap();
        write_fake_skill(&skills_root, "weather", None);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.config_path = workspace_dir.parent().unwrap().join("config.toml");
        config.skills.open_skills_enabled = false;

        handle_command(
            crate::SkillCommands::Disable {
                name: "weather".to_string(),
            },
            &config,
        )
        .expect("disable should succeed");

        let reloaded = reload_config(&config.config_path, &workspace_dir);
        let active = load_skills_with_config(&reloaded.workspace_dir, &reloaded);
        assert!(
            active.iter().all(|s| s.name != "weather"),
            "disabled skill must be excluded from the active skill set"
        );
    }

    #[test]
    fn case_insensitive_entry_disables_skill() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let skills_root = workspace_dir.join("skills");
        fs::create_dir_all(&skills_root).unwrap();
        write_fake_skill(&skills_root, "weather", None);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = false;
        // Mixed-case key — the bug: the pre-fix exact-match lookup never
        // matched the canonical lowercase `skill.name`, so this silently
        // failed to disable the skill.
        config.skills.entries.insert(
            "Weather".to_string(),
            crate::config::SkillEntryConfig {
                enabled: false,
                ..Default::default()
            },
        );

        let active = load_skills_with_config(&workspace_dir, &config);
        assert!(
            active.iter().all(|s| s.name != "weather"),
            "mixed-case `[skills.entries.Weather]` must disable a skill named `weather`"
        );
    }

    #[test]
    fn disable_unknown_name_errors() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        fs::create_dir_all(workspace_dir.join("skills")).unwrap();

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.config_path = workspace_dir.parent().unwrap().join("config.toml");
        config.skills.open_skills_enabled = false;

        let result = handle_command(
            crate::SkillCommands::Disable {
                name: "nope".to_string(),
            },
            &config,
        );
        let err = result.expect_err("expected Err for a nonexistent skill");
        assert!(err.to_string().contains("nope"), "unexpected error: {err}");
    }

    #[test]
    fn disable_preserves_existing_entry_fields() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let skills_root = workspace_dir.join("skills");
        fs::create_dir_all(&skills_root).unwrap();
        write_fake_skill(&skills_root, "weather", None);

        let mut env_map = std::collections::HashMap::new();
        env_map.insert("API_REGION".to_string(), "us-east".to_string());
        let mut config_map = std::collections::HashMap::new();
        config_map.insert(
            "endpoint".to_string(),
            serde_json::json!("https://example.com"),
        );

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.config_path = workspace_dir.parent().unwrap().join("config.toml");
        config.skills.open_skills_enabled = false;
        config.skills.entries.insert(
            "weather".to_string(),
            crate::config::SkillEntryConfig {
                enabled: true,
                api_key: None,
                env: env_map.clone(),
                config: config_map.clone(),
            },
        );

        handle_command(
            crate::SkillCommands::Disable {
                name: "weather".to_string(),
            },
            &config,
        )
        .expect("disable should succeed");

        let reloaded = reload_config(&config.config_path, &workspace_dir);
        let entry = reloaded.skills.entries.get("weather").unwrap();
        assert!(!entry.enabled);
        assert_eq!(entry.env, env_map);
        assert_eq!(entry.config, config_map);
    }

    #[test]
    fn disable_save_round_trip_preserves_config_secret() {
        let (_env, workspace_dir) = FakeProfileEnv::new();
        let skills_root = workspace_dir.join("skills");
        fs::create_dir_all(&skills_root).unwrap();
        write_fake_skill(&skills_root, "weather", None);

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.config_path = workspace_dir.parent().unwrap().join("config.toml");
        config.skills.open_skills_enabled = false;
        config.api_key = Some("plaintext-test-secret".to_string());

        handle_command(
            crate::SkillCommands::Disable {
                name: "weather".to_string(),
            },
            &config,
        )
        .expect("disable should succeed");

        let raw = fs::read_to_string(&config.config_path).unwrap();
        let reloaded: crate::config::Config = toml::from_str(&raw).unwrap();

        let entry = reloaded.skills.entries.get("weather").unwrap();
        assert!(
            !entry.enabled,
            "disable must still take effect alongside a config-level secret"
        );

        let stored = reloaded
            .api_key
            .clone()
            .expect("api_key must round-trip through save(), not be dropped");
        assert!(
            crate::security::SecretStore::is_encrypted(&stored),
            "config-level secret must be encrypted at rest, got: {stored}"
        );
        let store = crate::security::SecretStore::new(config.config_path.parent().unwrap(), true);
        let decrypted = store.decrypt(&stored).unwrap();
        assert_eq!(
            decrypted, "plaintext-test-secret",
            "secret must decrypt back to its original plaintext"
        );
    }
}

#[cfg(test)]
mod symlink_tests;
