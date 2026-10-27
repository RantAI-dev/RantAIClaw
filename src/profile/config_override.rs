//! The config-directory override in effect, and the profile it keeps in
//! effect.
//!
//! Three things can pin the config directory away from the active profile:
//! `RANTAICLAW_CONFIG_DIR`, `RANTAICLAW_WORKSPACE` and the
//! `active_workspace.toml` marker. A marker that names a profile directory
//! of this root is not one of them — it is a profile pointer, and it yields
//! to the active profile (see [`profile_pointer_name`]).
//!
//! While an override is in effect, a profile switch moves the per-profile
//! stores (sessions, knowledge base, policy, persona, skills, secrets) but
//! not the config, the workspace, the memory database or the cron store: the
//! command would leave one install pointing at two profiles. So a switch that
//! names a profile other than the one the override keeps in effect is
//! refused, and the refusal names the override and how to clear it.

use std::path::{Path, PathBuf};

/// The marker file that can pin the config directory.
pub const ACTIVE_WORKSPACE_MARKER: &str = "active_workspace.toml";

/// Where an in-effect override comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideSource {
    /// `RANTAICLAW_CONFIG_DIR` — the whole config directory.
    EnvConfigDir,
    /// `RANTAICLAW_WORKSPACE` — the workspace directory.
    EnvWorkspace,
    /// The `active_workspace.toml` marker in the profile root.
    Marker,
}

impl OverrideSource {
    /// The name of the thing that pins the config directory.
    pub fn label(self) -> &'static str {
        match self {
            Self::EnvConfigDir => "RANTAICLAW_CONFIG_DIR",
            Self::EnvWorkspace => "RANTAICLAW_WORKSPACE",
            Self::Marker => ACTIVE_WORKSPACE_MARKER,
        }
    }

    /// How the operator takes the override away.
    pub fn how_to_clear(self) -> &'static str {
        match self {
            Self::EnvConfigDir => "unset RANTAICLAW_CONFIG_DIR",
            Self::EnvWorkspace => "unset RANTAICLAW_WORKSPACE",
            Self::Marker => "remove ~/.rantaiclaw/active_workspace.toml",
        }
    }
}

/// A config-directory override in effect right now.
#[derive(Debug, Clone)]
pub struct ConfigDirOverride {
    /// What pins the config directory.
    pub source: OverrideSource,
    /// The path the override names: the config directory for
    /// `RANTAICLAW_CONFIG_DIR` and for the marker, the workspace directory
    /// for `RANTAICLAW_WORKSPACE`.
    pub path: PathBuf,
    /// The profile the override pins, when it names a profile directory of
    /// this root (`<root>/profiles/<name>`).
    pub profile: Option<String>,
}

/// The profile name a config directory names, when it is a profile directory
/// of `root` — `<root>/profiles/<name>`.
///
/// A marker that names one of those is a PROFILE POINTER: it says "the
/// active profile", not "this directory", so the config loader yields to the
/// active profile instead of pinning the directory. Every other marker
/// target stays an override.
pub fn profile_pointer_name(config_dir: &Path, root: &Path) -> Option<String> {
    let name = config_dir.file_name()?.to_str()?;
    let profiles_dir = config_dir.parent()?;
    if profiles_dir.file_name()? != "profiles" {
        return None;
    }
    if profiles_dir.parent()? != root {
        return None;
    }
    if super::validate_profile_name(name).is_err() {
        return None;
    }
    Some(name.to_string())
}

/// The override in effect right now, if any.
///
/// Precedence mirrors the config loader: `RANTAICLAW_CONFIG_DIR`, then
/// `RANTAICLAW_WORKSPACE`, then the `active_workspace.toml` marker.
pub fn override_in_effect() -> Option<ConfigDirOverride> {
    if let Some(dir) = non_empty_env("RANTAICLAW_CONFIG_DIR") {
        let path = PathBuf::from(dir);
        let profile = profile_pointer_name(&path, &super::paths::rantaiclaw_root());
        return Some(ConfigDirOverride {
            source: OverrideSource::EnvConfigDir,
            path,
            profile,
        });
    }
    if let Some(dir) = non_empty_env("RANTAICLAW_WORKSPACE") {
        // A workspace override names a workspace, not a profile: the config
        // directory it resolves to is derived by the loader, and the
        // per-profile stores keep following the active profile.
        return Some(ConfigDirOverride {
            source: OverrideSource::EnvWorkspace,
            path: PathBuf::from(dir),
            profile: None,
        });
    }
    marker_override()
}

/// The marker override, when the marker names something other than a profile
/// directory of this root.
fn marker_override() -> Option<ConfigDirOverride> {
    let root = super::paths::rantaiclaw_root();
    let contents = std::fs::read_to_string(root.join(ACTIVE_WORKSPACE_MARKER)).ok()?;
    // The marker is a one-key TOML file. Read it as a value so the shape
    // stays the loader's, whatever it holds.
    let value: toml::Value = toml::from_str(&contents).ok()?;
    let raw = value.get("config_dir")?.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    // A profile pointer is not an override: the loader yields to the active
    // profile for it.
    if profile_pointer_name(&path, &root).is_some() {
        return None;
    }
    Some(ConfigDirOverride {
        source: OverrideSource::Marker,
        path,
        profile: None,
    })
}

/// The override in effect and the profile it keeps in effect.
///
/// `None` when no override is in effect — every profile switch is allowed
/// then. Otherwise the profile is the one the override pins, or the active
/// profile (the marker), whose per-profile stores keep following it.
pub fn in_effect() -> Option<(ConfigDirOverride, String)> {
    let over = override_in_effect()?;
    let profile = match &over.profile {
        Some(name) => name.clone(),
        None => super::ProfileManager::marker_profile(),
    };
    Some((over, profile))
}

/// The one-sentence refusal a profile switch gets while an override is in
/// effect. Names the override, what it would split, and how to clear it.
pub fn refusal(name: &str, over: &ConfigDirOverride, in_effect: &str) -> String {
    format!(
        "refusing to switch to profile {name:?}: {} pins the config to {}, so a switch would \
         leave the config on one profile and the per-profile stores on another; {} to switch \
         profiles (the profile in effect is {in_effect:?})",
        over.source.label(),
        over.path.display(),
        over.source.how_to_clear(),
    )
}

/// Refuse a switch to `name` while an override keeps another profile in
/// effect. `Ok(())` when no override is in effect, or when `name` IS the
/// profile in effect (a switch that changes nothing).
pub fn refuse_switch(name: &str) -> anyhow::Result<()> {
    if let Some((over, in_effect)) = in_effect() {
        if name != in_effect {
            anyhow::bail!("{}", refusal(name, &over, &in_effect));
        }
    }
    Ok(())
}

/// A non-empty environment variable, trimmed.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
