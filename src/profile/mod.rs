//! Profile manager — multi-profile storage layout for v0.5.0.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Profile manager" and §"Storage layout".
//!
//! A `Profile` is a self-contained directory under
//! `~/.rantaiclaw/profiles/<name>/` that holds config, persona, workspace,
//! memory, skills, sessions, policy, secrets and runtime state. The default
//! profile is auto-created on first run; users may create more via
//! `rantaiclaw profile create <name> [--clone <src>]`.
//!
//! Active-profile resolution precedence (first match wins):
//!   1. `RANTAICLAW_PROFILE` env var (set by the `-p, --profile` CLI flag).
//!   2. `~/.rantaiclaw/active_profile` file contents.
//!   3. The literal string `"default"`.

pub mod commands;
pub mod migration;
pub mod paths;
pub mod sentinel;

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

/// A live profile on disk. Construction implies the directory tree exists
/// (callers must go through `ProfileManager::ensure*`).
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub root: PathBuf,
}

impl Profile {
    pub fn config_toml(&self) -> PathBuf {
        paths::config_toml(&self.name)
    }
    pub fn config_staging(&self) -> PathBuf {
        paths::config_staging(&self.name)
    }
    pub fn workspace_dir(&self) -> PathBuf {
        paths::workspace_dir(&self.name)
    }
    pub fn memory_dir(&self) -> PathBuf {
        paths::memory_dir(&self.name)
    }
    pub fn sessions_dir(&self) -> PathBuf {
        paths::sessions_dir(&self.name)
    }
    pub fn skills_dir(&self) -> PathBuf {
        paths::skills_dir(&self.name)
    }
    pub fn persona_dir(&self) -> PathBuf {
        paths::persona_dir(&self.name)
    }
    pub fn policy_dir(&self) -> PathBuf {
        paths::policy_dir(&self.name)
    }
    pub fn secrets_dir(&self) -> PathBuf {
        paths::secrets_dir(&self.name)
    }
    pub fn runtime_dir(&self) -> PathBuf {
        paths::runtime_dir(&self.name)
    }
    pub fn audit_log(&self) -> PathBuf {
        paths::audit_log(&self.name)
    }
}

/// Optional knobs for `ProfileManager::create` when `--clone` is set.
///
/// Defaults are conservative: do NOT copy memory or secrets across profiles.
/// Users opt in explicitly per spec §"Storage layout / Profile clone semantics".
#[derive(Debug, Clone, Copy, Default)]
pub struct CloneOpts {
    pub include_secrets: bool,
    pub include_memory: bool,
}

/// Stateless façade over the profiles directory.
///
/// All entry points are static — there is one process-wide profile root
/// (computed via `paths`) and we never cache it.
pub struct ProfileManager;

impl ProfileManager {
    /// Idempotently ensure the `default` profile exists; return its handle.
    pub fn ensure_default() -> Result<Profile> {
        Self::ensure("default")
    }

    /// Idempotently ensure a named profile and its full sub-tree exist.
    pub fn ensure(name: &str) -> Result<Profile> {
        validate_profile_name(name)?;
        let dir = paths::profile_dir(name);
        if !dir.exists() {
            fs::create_dir_all(&dir)
                .with_context(|| format!("create profile dir {}", dir.display()))?;
        }
        // Subdirectories — create lazily but unconditionally so partial
        // trees from earlier crashed runs are healed on next launch.
        for sub in &[
            "workspace",
            "memory",
            "sessions",
            "skills",
            "persona",
            "policy",
            "secrets",
            "runtime",
        ] {
            let p = dir.join(sub);
            if !p.exists() {
                fs::create_dir_all(&p)
                    .with_context(|| format!("create profile subdir {}", p.display()))?;
            }
        }
        // Best-effort tighten on secrets/.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let secrets = dir.join("secrets");
            if let Ok(meta) = fs::metadata(&secrets) {
                let mut perms = meta.permissions();
                perms.set_mode(0o700);
                let _ = fs::set_permissions(&secrets, perms);
            }
        }
        Ok(Profile {
            name: name.to_string(),
            root: dir,
        })
    }

    /// Resolve + ensure the active profile in one call. The hot path for
    /// every config-reading entry point.
    pub fn active() -> Result<Profile> {
        let name = Self::resolve_active_name();
        Self::ensure(&name)
    }

    /// Pure resolution — no filesystem mutation. See module docstring for
    /// precedence.
    pub fn resolve_active_name() -> String {
        // 1. Env var (set by --profile flag in main.rs)
        if let Ok(name) = std::env::var("RANTAICLAW_PROFILE") {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        // 2. ~/.rantaiclaw/active_profile file
        if let Ok(s) = fs::read_to_string(paths::active_profile_file()) {
            let trimmed = s.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
        // 3. Hard default
        "default".to_string()
    }

    /// Sorted list of existing profile names. Returns empty vec if the
    /// `profiles/` directory does not yet exist.
    pub fn list() -> Result<Vec<String>> {
        let root = paths::rantaiclaw_root().join("profiles");
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut names: Vec<String> = vec![];
        for entry in fs::read_dir(&root).with_context(|| format!("readdir {}", root.display()))? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(s) = entry.file_name().to_str() {
                    names.push(s.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// Create a new profile, optionally cloning persona/skills/forbidden-paths
    /// from a source profile. See spec §"Profile clone semantics".
    pub fn create(name: &str, clone_from: Option<&str>, opts: CloneOpts) -> Result<Profile> {
        validate_profile_name(name)?;
        let dst_dir = paths::profile_dir(name);
        if dst_dir.exists() {
            bail!("profile {name:?} already exists at {}", dst_dir.display());
        }
        let dst = Self::ensure(name)?;
        if let Some(src_name) = clone_from {
            let src = Self::ensure(src_name)?;
            clone_into(&src, &dst, opts)?;
        }
        Ok(dst)
    }

    /// Create a new profile by importing from an external on-disk source
    /// (e.g. `~/.openclaw/`). Mirrors `create` but the source is a raw
    /// filesystem path, not another profile. See `crate::migration::openclaw`.
    ///
    /// `force=true` overwrites an existing profile of the same name (the
    /// existing directory is removed first); without `force`, an
    /// already-existing profile is an error — never silently merged.
    ///
    /// Memory and sessions are intentionally NOT copied (matches the
    /// `clone_into` policy).
    pub fn create_clone_from_path(
        name: &str,
        source_root: &std::path::Path,
        force: bool,
    ) -> Result<Profile> {
        validate_profile_name(name)?;
        if !source_root.is_dir() {
            bail!("source root {} is not a directory", source_root.display());
        }
        let dst_dir = paths::profile_dir(name);
        if dst_dir.exists() {
            if !force {
                bail!(
                    "profile {name:?} already exists at {} — pass --force to overwrite",
                    dst_dir.display()
                );
            }
            fs::remove_dir_all(&dst_dir)
                .with_context(|| format!("remove existing profile dir {}", dst_dir.display()))?;
        }
        let dst = Self::ensure(name)?;

        // 1. Translate config.toml using the OpenClaw migration helper.
        let src_config = source_root.join("config.toml");
        if src_config.is_file() {
            let body = fs::read_to_string(&src_config)
                .with_context(|| format!("read {}", src_config.display()))?;
            let (translated, _) = crate::migration::openclaw::translate_config(&body);
            let dst_config = dst.config_toml();
            if let Some(parent) = dst_config.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dst_config, translated)
                .with_context(|| format!("write {}", dst_config.display()))?;
        }

        // 2. Copy skills/ verbatim.
        let src_skills = source_root.join("skills");
        if src_skills.is_dir() {
            copy_dir_all(&src_skills, &dst.skills_dir())?;
        }

        // 3. Copy secrets/ verbatim, retaining 0700 perms on the dest dir.
        let src_secrets = source_root.join("secrets");
        if src_secrets.is_dir() {
            copy_dir_all(&src_secrets, &dst.secrets_dir())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(dst.secrets_dir()) {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o700);
                    let _ = fs::set_permissions(dst.secrets_dir(), perms);
                }
            }
        }

        Ok(dst)
    }

    /// Set the active profile. Refuses unknown profile names so users get a
    /// clear error instead of a silent typo.
    pub fn use_profile(name: &str) -> Result<()> {
        validate_profile_name(name)?;
        let dir = paths::profile_dir(name);
        if !dir.exists() {
            bail!("profile {name:?} does not exist at {}", dir.display());
        }
        let path = paths::active_profile_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Atomic-ish write: tempfile + rename in same dir.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, format!("{name}\n"))?;
        fs::rename(&tmp, &path)?;

        // If a daemon is registered for this profile, ask the init system to
        // restart it so it picks up the new active config. Errors here are
        // downgraded to warnings — a stale daemon must never block the
        // profile switch itself.
        if let Err(e) = crate::daemon::handoff::restart_daemon_for_profile(name) {
            eprintln!("Warning: daemon handoff for profile {name:?} failed: {e}");
        }

        Ok(())
    }

    /// Delete a profile. Refuses to delete the active profile unless
    /// `force` is true (caller has acknowledged the consequences).
    pub fn delete(name: &str, force: bool) -> Result<()> {
        validate_profile_name(name)?;
        let dir = paths::profile_dir(name);
        if !dir.exists() {
            bail!("profile {name:?} does not exist at {}", dir.display());
        }
        let active = Self::resolve_active_name();
        if active == name && !force {
            bail!(
                "refusing to delete active profile {name:?}; pass --force or `rantaiclaw profile use <other>` first"
            );
        }
        fs::remove_dir_all(&dir)
            .with_context(|| format!("remove profile dir {}", dir.display()))?;
        // If we just deleted the active profile, clear the marker so the
        // next launch falls back to "default" (and re-creates it).
        if active == name {
            let marker = paths::active_profile_file();
            let _ = fs::remove_file(&marker);
        }
        Ok(())
    }
}

/// Reject profile names that would escape the profiles dir or land on weird
/// filesystem entries. Restrict to a Unix-friendly subset.
fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("profile name cannot be empty");
    }
    if name == "." || name == ".." {
        bail!("profile name cannot be '.' or '..'");
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        bail!("profile name {name:?} contains forbidden characters");
    }
    Ok(())
}

fn clone_into(src: &Profile, dst: &Profile, opts: CloneOpts) -> Result<()> {
    // Files cloned at root: config.toml (non-secret keys are simply the file as-is;
    // secret encryption is opt-in per spec).
    let cfg_src = src.config_toml();
    if cfg_src.exists() {
        let cfg_dst = dst.config_toml();
        if let Some(parent) = cfg_dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&cfg_src, &cfg_dst)?;
    }

    // Directories cloned by default.
    copy_dir_all(&src.persona_dir(), &dst.persona_dir())?;

    // policy/forbidden_paths.toml — yes; command_allowlist.toml — fresh start (skip).
    let forbidden_src = src.policy_dir().join("forbidden_paths.toml");
    if forbidden_src.exists() {
        let forbidden_dst = dst.policy_dir().join("forbidden_paths.toml");
        if let Some(parent) = forbidden_dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&forbidden_src, &forbidden_dst)?;
    }

    copy_dir_all(&src.skills_dir(), &dst.skills_dir())?;

    if opts.include_memory {
        copy_dir_all(&src.memory_dir(), &dst.memory_dir())?;
    }
    if opts.include_secrets {
        copy_dir_all(&src.secrets_dir(), &dst.secrets_dir())?;
    }
    // workspace/, sessions/, audit.log, runtime/ never copied — see spec table.
    Ok(())
}

fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else if ft.is_file() {
            fs::copy(entry.path(), &target)?;
        }
        // symlinks intentionally skipped — clone semantics are file-data only.
    }
    Ok(())
}
