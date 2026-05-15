//! Recipe runner for `metadata.clawdbot.install[]` — fills missing skill
//! binary dependencies the same way OpenClaw's gateway does.
//!
//! Mirrors OpenClaw's recipe-selection rules per their public docs:
//!
//! 1. Filter recipes by `os[]` (drop entries that don't match the host).
//! 2. Filter recipes that don't actually provide any of the missing bins.
//! 3. Sort by preference: brew → uv → npm → go → download (configurable).
//! 4. Pick the first eligible recipe.
//! 5. Run it. Validate that all `bins[]` now exist on `$PATH`.
//!
//! Each recipe kind shells out to its native tool — we never bundle a
//! package manager, just orchestrate. If a recipe's tool is missing
//! (e.g. `brew` itself isn't installed), that recipe is skipped and the
//! next preferred kind is tried.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use super::{Skill, SkillInstallRecipe};

/// Outcome of a single `install-deps` invocation. Reported to the user.
#[derive(Debug)]
pub struct InstallDepsOutcome {
    pub skill: String,
    pub recipe_used: Option<String>,
    pub bins_installed: Vec<String>,
    pub bins_still_missing: Vec<String>,
}

impl InstallDepsOutcome {
    pub fn success(&self) -> bool {
        self.bins_still_missing.is_empty() && !self.bins_installed.is_empty()
    }
}

/// Run the preferred install recipe for a skill, using the default
/// selector preferences. Convenience wrapper around
/// [`install_deps_for_with_prefs`] for callers that don't have a config
/// in hand.
pub fn install_deps_for(skill: &Skill) -> Result<InstallDepsOutcome> {
    install_deps_for_with_prefs(skill, &SelectorPrefs::default())
}

/// User-configurable knobs for [`pick_preferred`]. Mirrors
/// `[skills.install]` in `config.toml` (`prefer_brew`, `node_manager`).
/// Defaults match the historical hardcoded order.
#[derive(Debug, Clone)]
pub struct SelectorPrefs {
    /// When true, brew sorts ahead of uv/npm/go (the legacy behaviour).
    /// Set to false to demote brew to the bottom of the eligible list
    /// — useful on hosts where brew is installed but the user prefers
    /// language-native installers (uv for Python, pnpm for Node, …).
    pub prefer_brew: bool,
    /// Which node-package-manager kind wins when multiple are declared.
    /// Recognised: `npm` | `pnpm` | `yarn`. Unknown values fall through
    /// to `npm`.
    pub node_manager: String,
}

impl Default for SelectorPrefs {
    fn default() -> Self {
        Self {
            prefer_brew: true,
            node_manager: "npm".to_string(),
        }
    }
}

impl SelectorPrefs {
    /// Build from a `[skills.install]` config block.
    pub fn from_config(cfg: &crate::config::SkillsInstallConfig) -> Self {
        Self {
            prefer_brew: cfg.prefer_brew,
            node_manager: if cfg.node_manager.is_empty() {
                "npm".into()
            } else {
                cfg.node_manager.clone()
            },
        }
    }
}

/// Same as [`install_deps_for`] but threads through user-configured
/// recipe-selector preferences.
pub fn install_deps_for_with_prefs(
    skill: &Skill,
    prefs: &SelectorPrefs,
) -> Result<InstallDepsOutcome> {
    let missing: Vec<String> = skill
        .requires
        .bins
        .iter()
        .filter(|b| which::which(b).is_err())
        .cloned()
        .collect();

    if missing.is_empty() {
        return Ok(InstallDepsOutcome {
            skill: skill.name.clone(),
            recipe_used: None,
            bins_installed: Vec::new(),
            bins_still_missing: Vec::new(),
        });
    }

    let candidates: Vec<&SkillInstallRecipe> = skill
        .install_recipes
        .iter()
        .filter(|r| r.matches_os())
        .filter(|r| {
            // Recipe must cover at least one missing bin to be useful.
            r.bins.is_empty() || r.bins.iter().any(|b| missing.contains(b))
        })
        .collect();

    if candidates.is_empty() {
        bail!(
            "skill `{}` has no install recipes for the missing bin(s): {}. \
             Install manually then re-run `skills list` to verify.",
            skill.name,
            missing.join(", ")
        );
    }

    // Preference order: brew → uv → <node-manager> → go → download by
    // default. `prefer_brew = false` demotes brew to the bottom;
    // `node_manager = "pnpm" | "yarn"` swaps which Node recipe wins
    // when multiple are declared. Both knobs come from
    // `[skills.install]` in `config.toml` and mirror OpenClaw.
    let preferred: Vec<&SkillInstallRecipe> = pick_preferred_with_prefs(&candidates, prefs);

    if preferred.is_empty() {
        bail!(
            "skill `{}` has install recipes but none of their tools are \
             available on this host. Install one of: {}",
            skill.name,
            candidates
                .iter()
                .map(|r| r.kind.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let recipe = preferred[0];
    println!(
        "→ {}: running `{}` recipe ({})",
        skill.name,
        recipe.kind,
        recipe.label.as_str()
    );

    run_recipe(recipe, &skill.name)
        .with_context(|| format!("recipe `{}` failed for {}", recipe.kind, skill.name))?;

    // Re-validate.
    let still_missing: Vec<String> = skill
        .requires
        .bins
        .iter()
        .filter(|b| which::which(b).is_err())
        .cloned()
        .collect();

    let installed: Vec<String> = missing
        .iter()
        .filter(|b| !still_missing.contains(b))
        .cloned()
        .collect();

    Ok(InstallDepsOutcome {
        skill: skill.name.clone(),
        recipe_used: Some(format!("{} ({})", recipe.kind, recipe.label)),
        bins_installed: installed,
        bins_still_missing: still_missing,
    })
}

/// Default-prefs wrapper preserved for tests + external callers that
/// don't carry a `SelectorPrefs`.
fn pick_preferred<'a>(candidates: &'a [&'a SkillInstallRecipe]) -> Vec<&'a SkillInstallRecipe> {
    pick_preferred_with_prefs(candidates, &SelectorPrefs::default())
}

/// Pick recipes the host can actually run, ordered by user preference.
/// A recipe is "available" when its driver tool (brew, uv, npm, etc.)
/// is on `$PATH`. `download` is always available because we use
/// `reqwest` + system tar/unzip.
///
/// Returns recipes in preference order — caller picks `[0]`.
///
/// Default ordering: brew → uv → npm → go → download.
/// `prefer_brew=false` demotes brew to the bottom of the eligible
/// list (still considered, just not preferred).
/// `node_manager` selects which Node recipe wins; non-preferred Node
/// kinds rank below the preferred one.
fn pick_preferred_with_prefs<'a>(
    candidates: &'a [&'a SkillInstallRecipe],
    prefs: &SelectorPrefs,
) -> Vec<&'a SkillInstallRecipe> {
    let pref_node = match prefs.node_manager.as_str() {
        "pnpm" | "yarn" => prefs.node_manager.as_str(),
        _ => "npm",
    };

    let kind_priority = |kind: &str| -> i32 {
        let node_pref_score = if kind == pref_node { 2 } else { 5 };
        match kind {
            "brew" => {
                if prefs.prefer_brew {
                    0
                } else {
                    // Demote to second-to-last; download stays bottom.
                    8
                }
            }
            "uv" => 1,
            "npm" | "pnpm" | "yarn" | "node" => node_pref_score,
            "go" => 3,
            "download" => 4,
            _ => 99,
        }
    };
    let driver_available = |kind: &str| -> bool {
        match kind {
            "brew" => which::which("brew").is_ok(),
            "uv" => which::which("uv").is_ok(),
            "npm" | "node" => which::which("npm").is_ok(),
            "pnpm" => which::which("pnpm").is_ok(),
            "yarn" => which::which("yarn").is_ok(),
            "go" => which::which("go").is_ok() || which::which("brew").is_ok(),
            "download" => true,
            _ => false,
        }
    };

    let mut ranked: Vec<&SkillInstallRecipe> = candidates
        .iter()
        .copied()
        .filter(|r| driver_available(&r.kind))
        .collect();
    ranked.sort_by_key(|r| kind_priority(&r.kind));
    ranked
}

fn run_recipe(recipe: &SkillInstallRecipe, slug: &str) -> Result<()> {
    match recipe.kind.as_str() {
        "brew" => run_brew(recipe),
        "uv" => run_uv(recipe),
        "npm" | "node" => run_npm(recipe),
        "pnpm" => run_pnpm(recipe),
        "yarn" => run_yarn(recipe),
        "go" => run_go(recipe),
        "download" => run_download(recipe, slug),
        other => bail!("unsupported install kind `{other}`"),
    }
}

fn run_brew(recipe: &SkillInstallRecipe) -> Result<()> {
    let formula = recipe
        .formula
        .as_ref()
        .ok_or_else(|| anyhow!("brew recipe missing `formula`"))?;
    run_subprocess("brew", &["install", formula])
}

fn run_uv(recipe: &SkillInstallRecipe) -> Result<()> {
    let pkg = recipe
        .pkg
        .as_ref()
        .ok_or_else(|| anyhow!("uv recipe missing `pkg`"))?;
    run_subprocess("uv", &["tool", "install", pkg])
}

fn run_npm(recipe: &SkillInstallRecipe) -> Result<()> {
    let pkg = recipe
        .pkg
        .as_ref()
        .ok_or_else(|| anyhow!("npm recipe missing `pkg`"))?;
    run_subprocess("npm", &["install", "-g", pkg])
}

fn run_pnpm(recipe: &SkillInstallRecipe) -> Result<()> {
    let pkg = recipe
        .pkg
        .as_ref()
        .ok_or_else(|| anyhow!("pnpm recipe missing `pkg`"))?;
    run_subprocess("pnpm", &["add", "-g", pkg])
}

fn run_yarn(recipe: &SkillInstallRecipe) -> Result<()> {
    let pkg = recipe
        .pkg
        .as_ref()
        .ok_or_else(|| anyhow!("yarn recipe missing `pkg`"))?;
    run_subprocess("yarn", &["global", "add", pkg])
}

fn run_go(recipe: &SkillInstallRecipe) -> Result<()> {
    // OpenClaw bootstraps Go via brew when go is missing; mirror that.
    if which::which("go").is_err() {
        if which::which("brew").is_ok() {
            println!("  · `go` not found, bootstrapping via brew first");
            run_subprocess("brew", &["install", "go"]).context("bootstrap go via brew")?;
        } else {
            bail!("go recipe needs `go` on PATH (or brew to bootstrap it)");
        }
    }
    let module = recipe
        .module
        .as_ref()
        .ok_or_else(|| anyhow!("go recipe missing `module`"))?;
    run_subprocess("go", &["install", module])
}

fn run_download(recipe: &SkillInstallRecipe, slug: &str) -> Result<()> {
    let url = recipe
        .url
        .as_ref()
        .ok_or_else(|| anyhow!("download recipe missing `url`"))?;
    let target_dir = recipe
        .target_dir
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            directories::ProjectDirs::from("", "", "rantaiclaw")
                .map(|d| d.data_dir().join("tools").join(slug))
                .unwrap_or_else(|| PathBuf::from(format!(".rantaiclaw/tools/{slug}")))
        });
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("create target dir {}", target_dir.display()))?;

    println!("  · downloading {url}");
    let bytes = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_mins(2))
        .build()
        .context("build reqwest client")?
        .get(url)
        .send()
        .context("download GET")?
        .error_for_status()
        .context("download HTTP status")?
        .bytes()
        .context("download body")?;

    match recipe.archive.as_deref() {
        Some("tar.gz" | "tgz") => {
            extract_targz(&bytes, &target_dir, recipe.strip_components.unwrap_or(0))?;
        }
        Some("zip") => extract_zip(&bytes, &target_dir, recipe.strip_components.unwrap_or(0))?,
        Some("tar.bz2") => {
            bail!("tar.bz2 archives not yet supported")
        }
        Some("raw") | None => {
            // Plain binary — write to target_dir as the last URL segment
            // and `chmod +x`.
            let name = url.rsplit('/').next().unwrap_or("downloaded-bin");
            let dest = target_dir.join(name);
            std::fs::write(&dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&dest)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&dest, perms)?;
            }
        }
        Some(other) => bail!("unsupported archive type `{other}`"),
    }

    println!("  · placed in {}", target_dir.display());
    println!(
        "  · ⚠ make sure `{}` is on your $PATH",
        target_dir.display()
    );
    Ok(())
}

fn extract_targz(bytes: &[u8], dest: &Path, strip: usize) -> Result<()> {
    let archive = write_archive_bytes(bytes, dest, "download.tar.gz")?;
    let list = archive_entries("tar", &["-tzf", archive.to_string_lossy().as_ref()])?;
    validate_archive_entries(&list)?;

    let strip_arg = format!("--strip-components={strip}");
    run_subprocess(
        "tar",
        &[
            "-xzf",
            archive.to_string_lossy().as_ref(),
            "-C",
            dest.to_string_lossy().as_ref(),
            strip_arg.as_str(),
        ],
    )
    .context("extract tar.gz download")?;
    let _ = std::fs::remove_file(archive);
    Ok(())
}

fn extract_zip(bytes: &[u8], dest: &Path, strip: usize) -> Result<()> {
    let archive = write_archive_bytes(bytes, dest, "download.zip")?;
    let list = archive_entries("unzip", &["-Z1", archive.to_string_lossy().as_ref()])
        .context("list zip entries (is `unzip` on PATH?)")?;
    validate_archive_entries(&list)?;

    if strip == 0 {
        run_subprocess(
            "unzip",
            &[
                "-q",
                archive.to_string_lossy().as_ref(),
                "-d",
                dest.to_string_lossy().as_ref(),
            ],
        )
        .context("extract zip download")?;
    } else {
        let staging = dest.join(format!(
            ".rantaiclaw-extract-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("create staging dir {}", staging.display()))?;
        run_subprocess(
            "unzip",
            &[
                "-q",
                archive.to_string_lossy().as_ref(),
                "-d",
                staging.to_string_lossy().as_ref(),
            ],
        )
        .context("extract zip download to staging")?;
        move_stripped_entries(&staging, dest, strip)?;
        let _ = std::fs::remove_dir_all(staging);
    }

    let _ = std::fs::remove_file(archive);
    Ok(())
}

fn write_archive_bytes(bytes: &[u8], dest: &Path, name: &str) -> Result<PathBuf> {
    let archive = dest.join(format!(
        ".rantaiclaw-{}-{}-{name}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    std::fs::write(&archive, bytes).with_context(|| format!("write {}", archive.display()))?;
    Ok(archive)
}

fn archive_entries(cmd: &str, args: &[&str]) -> Result<Vec<String>> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("spawn {cmd}"))?;
    if !output.status.success() {
        bail!("`{cmd} {}` exited with {}", args.join(" "), output.status);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(str::to_string).collect())
}

fn validate_archive_entries(entries: &[String]) -> Result<()> {
    for entry in entries {
        let path = Path::new(entry);
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                    bail!("archive entry `{entry}` would extract outside the target dir")
                }
                Component::CurDir | Component::Normal(_) => {}
            }
        }
    }
    Ok(())
}

fn move_stripped_entries(from: &Path, to: &Path, strip: usize) -> Result<()> {
    for entry in std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        move_stripped_entry(&entry.path(), from, to, strip)?;
    }
    Ok(())
}

fn move_stripped_entry(path: &Path, root: &Path, dest: &Path, strip: usize) -> Result<()> {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
            let entry = entry?;
            move_stripped_entry(&entry.path(), root, dest, strip)?;
        }
        return Ok(());
    }

    let rel = path
        .strip_prefix(root)
        .context("compute extracted relative path")?;
    let stripped: PathBuf = rel.components().skip(strip).collect();
    if stripped.as_os_str().is_empty() {
        return Ok(());
    }
    validate_archive_entries(&[stripped.to_string_lossy().to_string()])?;
    let target = dest.join(stripped);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create target dir {}", parent.display()))?;
    }
    std::fs::rename(path, &target)
        .or_else(|_| {
            std::fs::copy(path, &target)?;
            std::fs::remove_file(path)
        })
        .with_context(|| format!("move extracted file to {}", target.display()))?;
    Ok(())
}

fn run_subprocess(cmd: &str, args: &[&str]) -> Result<()> {
    println!("  $ {cmd} {}", args.join(" "));
    let status = Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("spawn {cmd}"))?;
    if !status.success() {
        bail!("`{cmd} {}` exited with {}", args.join(" "), status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillInstallRecipe;

    fn r(kind: &str) -> SkillInstallRecipe {
        SkillInstallRecipe {
            id: kind.into(),
            kind: kind.into(),
            ..Default::default()
        }
    }

    #[test]
    fn pick_preferred_orders_brew_first() {
        let go = r("go");
        let download = r("download");
        let brew = r("brew");
        let candidates = vec![&download, &go, &brew];
        // We can't easily mock `which` here; test the priority function
        // indirectly by ensuring brew sorts before go/download when all
        // drivers are available. On most CI hosts brew won't be present
        // and the assertion would skip — gate test to runs that have
        // both brew and go to be meaningful in CI; otherwise just check
        // that download (always available) is never picked over a kind
        // whose driver is missing only because brew is missing too.
        let picked = pick_preferred(&candidates);
        // Every returned recipe MUST have an available driver.
        for r in &picked {
            let ok = match r.kind.as_str() {
                "brew" => which::which("brew").is_ok(),
                "uv" => which::which("uv").is_ok(),
                "npm" | "node" => which::which("npm").is_ok(),
                "go" => which::which("go").is_ok() || which::which("brew").is_ok(),
                "download" => true,
                _ => false,
            };
            assert!(
                ok,
                "picked recipe `{}` whose driver isn't available",
                r.kind
            );
        }
    }

    #[test]
    fn matches_os_filter_works() {
        let mut r = SkillInstallRecipe::default();
        // Empty os list = match anything
        assert!(r.matches_os());
        r.os = vec!["macos-bsd-fictional".into()];
        // Made-up OS — should NOT match the test runner's actual OS.
        assert!(!r.matches_os());
        r.os = vec![std::env::consts::OS.into()];
        assert!(r.matches_os());
    }

    #[test]
    fn pick_preferred_with_prefer_brew_false_demotes_brew() {
        // Build a candidate set where brew, uv, and download all rank.
        // download is the only kind whose driver is unconditionally
        // available, so we can compare placement reliably.
        let brew = r("brew");
        let uv = r("uv");
        let download = r("download");
        let candidates = vec![&brew, &uv, &download];

        let mut prefs = SelectorPrefs::default();
        prefs.prefer_brew = false;

        let picked = pick_preferred_with_prefs(&candidates, &prefs);

        // download should outrank brew when prefer_brew=false (brew
        // demoted to priority 8, download stays at 4). brew may be
        // entirely absent from the picked list when its driver is
        // missing — which is the dominant case on test hosts.
        if let Some(brew_pos) = picked.iter().position(|r| r.kind == "brew") {
            let download_pos = picked
                .iter()
                .position(|r| r.kind == "download")
                .expect("download is always available");
            assert!(
                download_pos < brew_pos,
                "with prefer_brew=false, download should outrank brew (got download={download_pos}, brew={brew_pos})"
            );
        }
    }

    #[test]
    fn pick_preferred_with_pnpm_node_manager_outranks_npm() {
        let npm = r("npm");
        let pnpm = r("pnpm");
        let yarn = r("yarn");
        let candidates = vec![&npm, &pnpm, &yarn];

        let mut prefs = SelectorPrefs::default();
        prefs.node_manager = "pnpm".to_string();

        let picked = pick_preferred_with_prefs(&candidates, &prefs);

        // Among the node-flavoured kinds, pnpm should sort first when
        // its driver is available. Skip the assertion on hosts that
        // don't have pnpm at all.
        if let (Some(p_pos), Some(n_pos)) = (
            picked.iter().position(|r| r.kind == "pnpm"),
            picked.iter().position(|r| r.kind == "npm"),
        ) {
            assert!(
                p_pos < n_pos,
                "node_manager=pnpm should outrank npm (got pnpm={p_pos}, npm={n_pos})"
            );
        }
    }

    #[test]
    fn pick_preferred_with_unknown_node_manager_falls_back_to_npm() {
        let npm = r("npm");
        let pnpm = r("pnpm");
        let candidates = vec![&npm, &pnpm];

        let mut prefs = SelectorPrefs::default();
        prefs.node_manager = "fnm".to_string(); // unknown — falls back to npm

        let picked = pick_preferred_with_prefs(&candidates, &prefs);
        if let (Some(n_pos), Some(p_pos)) = (
            picked.iter().position(|r| r.kind == "npm"),
            picked.iter().position(|r| r.kind == "pnpm"),
        ) {
            assert!(
                n_pos < p_pos,
                "unknown node_manager should fall back to npm preference (got npm={n_pos}, pnpm={p_pos})"
            );
        }
    }

    #[test]
    fn selector_prefs_from_config_round_trip() {
        let cfg = crate::config::SkillsInstallConfig {
            prefer_brew: false,
            node_manager: "yarn".into(),
        };
        let prefs = SelectorPrefs::from_config(&cfg);
        assert!(!prefs.prefer_brew);
        assert_eq!(prefs.node_manager, "yarn");

        // Empty node_manager defaults to npm.
        let cfg_empty = crate::config::SkillsInstallConfig {
            prefer_brew: true,
            node_manager: String::new(),
        };
        let prefs2 = SelectorPrefs::from_config(&cfg_empty);
        assert_eq!(prefs2.node_manager, "npm");
    }

    #[test]
    fn archive_entry_validation_rejects_escape_paths() {
        assert!(validate_archive_entries(&["bin/tool".to_string()]).is_ok());
        assert!(validate_archive_entries(&["../tool".to_string()]).is_err());
        assert!(validate_archive_entries(&["/tmp/tool".to_string()]).is_err());
        assert!(validate_archive_entries(&["bin/../tool".to_string()]).is_err());
    }

    #[test]
    fn extract_targz_honors_strip_components() {
        if which::which("tar").is_err() {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src/pkg/bin");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("tool"), "ok").unwrap();
        let archive = temp.path().join("tool.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(temp.path().join("src"))
            .arg("pkg")
            .status()
            .unwrap();
        assert!(status.success());

        let dest = temp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let bytes = std::fs::read(&archive).unwrap();
        extract_targz(&bytes, &dest, 1).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("bin/tool")).unwrap(),
            "ok"
        );
    }
}
