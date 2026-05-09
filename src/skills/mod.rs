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
            if let Some(entry) = config.skills.entries.get(&s.name) {
                if !entry.enabled {
                    tracing::info!(skill = %s.name, "skipped: disabled in config.toml");
                    return false;
                }
            }
            let unmet = s.requires.unmet();
            if !unmet.is_empty() {
                tracing::info!(
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
            if let Some(entry) = config.skills.entries.get(&s.name) {
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
                            req.bins.extend(
                                bins.iter().filter_map(|v| v.as_str().map(String::from)),
                            );
                        }
                        if let Some(env) = requires.get("env").and_then(|v| v.as_array()) {
                            req.env.extend(
                                env.iter().filter_map(|v| v.as_str().map(String::from)),
                            );
                        }
                    }
                    if let Some(os) = scoped.get("os").and_then(|v| v.as_array()) {
                        req.os.extend(os.iter().filter_map(|v| v.as_str().map(String::from)));
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
                            recipe.url = entry
                                .get("url")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            recipe.archive = entry
                                .get("archive")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            recipe.strip_components =
                                entry.get("stripComponents").and_then(|v| v.as_u64()).map(
                                    |n| n as usize,
                                );
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

    let flush =
        |out: &mut Vec<String>, name: &mut Option<String>, required: &mut bool| {
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
    let Some(rest) = trimmed.strip_prefix("---\n").or_else(|| trimmed.strip_prefix("---\r\n"))
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

fn parse_yaml_list(raw: &str) -> Vec<String> {
    let s = raw.trim();
    let inner = s.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(s);
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
                        let has_missing_bin = reasons
                            .iter()
                            .any(|r| r.starts_with("missing binary"));
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
            let found = skills
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(&name));
            match found {
                Some(s) => {
                    println!(
                        "{} {}",
                        console::style(&s.name).white().bold(),
                        if s.version.is_empty() {
                            String::new()
                        } else {
                            console::style(format!("· v{}", s.version)).dim().to_string()
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
            // Reject path traversal attempts
            if name.contains("..") || name.contains('/') || name.contains('\\') {
                anyhow::bail!("Invalid skill name: {name}");
            }

            let skills_root = skills_dir(workspace_dir);
            let skill_path = skills_root.join(&name);

            // Verify the path *itself* (not the symlink target) lives directly
            // under <skills_root>. Pre-fix code canonicalized the symlink target
            // and rejected legit installs whose source was outside the workspace
            // (the common `skills install /tmp/foo` flow).
            let canonical_skills = skills_root
                .canonicalize()
                .unwrap_or_else(|_| skills_root.clone());
            // Use `parent().canonicalize()` to verify containment without
            // resolving a symlink target.
            if let Some(parent) = skill_path.parent() {
                if let Ok(canonical_parent) = parent.canonicalize() {
                    if canonical_parent != canonical_skills {
                        anyhow::bail!("Skill path escapes skills directory: {name}");
                    }
                }
            }

            // Use symlink_metadata so we don't fail on dangling symlinks.
            let meta = std::fs::symlink_metadata(&skill_path)
                .map_err(|_| anyhow::anyhow!("Skill not found: {name}"))?;

            if meta.file_type().is_symlink() {
                std::fs::remove_file(&skill_path)?;
            } else {
                std::fs::remove_dir_all(&skill_path)?;
            }
            println!(
                "  {} Skill '{}' removed.",
                console::style("✓").green().bold(),
                name
            );
            Ok(())
        }
        crate::SkillCommands::Update { slug, all } => {
            let profile = crate::profile::ProfileManager::active()
                .context("resolve active profile")?;
            let skills = load_skills_with_config(workspace_dir, config);

            let targets: Vec<String> = if all {
                skills.iter().map(|s| s.name.clone()).collect()
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
                let rt = tokio::runtime::Runtime::new()
                    .context("build tokio runtime")?;
                let (mut updated, mut skipped, mut failed) = (0usize, 0usize, 0usize);
                for slug in &targets {
                    let dir = profile.skills_dir().join(slug);
                    if !dir.exists() {
                        println!("  ⊘ {slug}: not installed locally — skipping");
                        skipped += 1;
                        continue;
                    }
                    if let Err(e) = std::fs::remove_dir_all(&dir) {
                        println!("  ✗ {slug}: failed to clear old install: {e}");
                        failed += 1;
                        continue;
                    }
                    match rt.block_on(crate::skills::clawhub::install_one(&profile, slug)) {
                        Ok(()) => {
                            println!(
                                "  {} {slug}: updated",
                                console::style("✓").green().bold()
                            );
                            updated += 1;
                        }
                        Err(e) => {
                            println!("  ✗ {slug}: {e}");
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
            println!(
                "Update summary: {updated} updated, {skipped} skipped, {failed} failed"
            );
            if failed > 0 {
                anyhow::bail!("{failed} skill(s) failed to update");
            }
            Ok(())
        }
        crate::SkillCommands::Inspect { slug } => {
            // Same pattern as Update — isolated runtime on a fresh thread.
            std::thread::spawn(move || -> Result<()> {
                let rt = tokio::runtime::Runtime::new()
                    .context("build tokio runtime")?;
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
                match install_deps::install_deps_for(skill) {
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
}

#[cfg(test)]
mod symlink_tests;
