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
//!
//! The refusal belongs to the doors that switch or select a profile — `profile
//! use`, the `--profile` flag, the `RANTAICLAW_PROFILE` environment variable —
//! not to the resolution of the active profile: a command that only reads a
//! profile is not stopped by it.
//!
//! The marker has ONE decision, [`marker_decision`], and both sides use it:
//! the config loader, which honors the marker as an override or sets it
//! aside, and the override check below, which refuses a switch while an
//! override is in effect. They must agree: a door that refuses a pin the
//! loader never applies locks the operator out of the profile they are on.
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
/// active profile instead of pinning the directory. Whether a marker is a
/// pointer, an override or set aside is decided by [`marker_decision`].
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

/// Why a marker cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerProblem {
    /// There is no marker file.
    Missing,
    /// The marker is not parsable TOML, or holds no `config_dir` string.
    Unparseable(String),
    /// The marker's `config_dir` is empty.
    Empty,
}

/// What the `active_workspace.toml` marker means.
///
/// The loader honors a marker as an override or sets it aside; the override
/// check reports an override exactly when the loader honors one. One
/// decision for both, so they cannot drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerDecision {
    /// The marker cannot be used.
    Unusable(MarkerProblem),
    /// The marker names a profile directory of this root
    /// (`<root>/profiles/<name>`): it says "the active profile", so the
    /// loader yields to the active profile.
    ProfilePointer {
        /// The directory the marker names.
        config_dir: PathBuf,
    },
    /// The marker names another root's profile (`<other>/profiles/<name>`)
    /// while this root has its own `config.toml` for that profile: the tree
    /// was copied or moved along with the marker, so the marker is set
    /// aside and the local profile wins.
    AnotherRootsProfile {
        /// The directory the marker names.
        config_dir: PathBuf,
        /// The profile of THIS root that takes the marker's place.
        local_profile: PathBuf,
    },
    /// The marker names a directory under the OS temp dir while the root is
    /// not under the temp dir: a leaked ephemeral workspace.
    TempLeak {
        /// The directory the marker names.
        config_dir: PathBuf,
    },
    /// The marker names a directory that is honored as an override.
    Override(PathBuf),
}

/// Decide what the `active_workspace.toml` marker of `root` means.
///
/// `contents` is the content of the marker file, `None` when there is no
/// marker file. The two sides read the file with their own filesystem — the
/// loader with the async one, the override check with the synchronous one —
/// but the DECISION is this one function, so the two cannot drift.
///
/// The rules, in the order they are applied:
///
/// 1. A marker that is missing, unparseable or empty cannot be used: the
///    loader sets it aside, and no override is in effect.
/// 2. A marker naming a profile directory of THIS root
///    (`<root>/profiles/<name>`) is a PROFILE POINTER: it says "the active
///    profile", so the loader yields to the active profile and no override
///    is in effect.
/// 3. A marker naming another root's profile (`<other>/profiles/<name>`)
///    while this root has its own `config.toml` for that profile was copied
///    or moved along with the tree: the loader sets it aside, with a
///    warning, and no override is in effect. When this root has no config
///    of its own for that profile, the marker is the only way to reach it
///    and is honored.
/// 4. A marker naming a directory under the OS temp dir while the root is
///    not under the temp dir points at a leaked ephemeral workspace: the
///    loader sets it aside and no override is in effect.
/// 5. Every other directory is honored as an override.
pub fn marker_decision(root: &Path, contents: Option<&str>) -> MarkerDecision {
    let Some(contents) = contents else {
        return MarkerDecision::Unusable(MarkerProblem::Missing);
    };
    // The marker is a one-key TOML file. Read it as a value so the shape
    // stays the loader's, whatever it holds.
    let value: toml::Value = match toml::from_str(contents) {
        Ok(value) => value,
        Err(error) => {
            return MarkerDecision::Unusable(MarkerProblem::Unparseable(error.to_string()));
        }
    };
    let Some(raw) = value.get("config_dir").and_then(|value| value.as_str()) else {
        return MarkerDecision::Unusable(MarkerProblem::Unparseable(
            "the marker holds no config_dir string".to_string(),
        ));
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return MarkerDecision::Unusable(MarkerProblem::Empty);
    }
    let parsed = PathBuf::from(raw);
    let config_dir = if parsed.is_absolute() {
        parsed
    } else {
        root.join(parsed)
    };
    // A profile pointer is not an override: the loader yields to the active
    // profile for it.
    if profile_pointer_name(&config_dir, root).is_some() {
        return MarkerDecision::ProfilePointer { config_dir };
    }
    // The marker was faithfully copied or moved along with the tree.
    if let Some(local_profile) = names_another_roots_profile(root, &config_dir) {
        return MarkerDecision::AnotherRootsProfile {
            config_dir,
            local_profile,
        };
    }
    // The marker points at a leaked ephemeral workspace.
    if is_temp_leak(&config_dir, root, &std::env::temp_dir()) {
        return MarkerDecision::TempLeak { config_dir };
    }
    // Everything else is honored.
    MarkerDecision::Override(config_dir)
}

/// Whether an active-workspace marker's `config_dir` looks like a leaked
/// ephemeral workspace: it points under the OS temp dir while the real
/// default config dir does not. A genuine install never keeps its workspace
/// under the temp dir, but a non-hermetic test can leave a marker in the real
/// `~/.rantaiclaw` pointing at a since-deleted tempdir — honoring it silently
/// shadows the real config (a split-brain where owner/config edits "don't
/// apply" until the marker is removed). Test setups (default dir also under
/// temp) are exempt.
fn is_temp_leak(config_dir: &Path, root: &Path, temp_dir: &Path) -> bool {
    config_dir.starts_with(temp_dir) && !root.starts_with(temp_dir)
}

/// Whether an active-workspace marker names the `profiles/<name>` directory
/// of a *different* config root, while the root that holds the marker has
/// its own `config.toml` for that profile. That is the shape of a marker
/// that was faithfully copied or moved along with the tree (a backup
/// archive, a home directory that moved to another machine, a `rollback`
/// snapshot restored under a different path). Honoring it would have the
/// run follow the marker back to the original location and migrate a
/// config the operator never asked to touch; setting it aside lets the
/// local profile win.
///
/// Returns `Some(local_profile_dir)` when the guard fires, where
/// `local_profile_dir` is `root/profiles/<name>`.
///
/// Three cases must not fire (they keep today's behavior):
/// - marker naming a directory inside its own root (case (a));
/// - marker naming a custom directory that is not shaped like
///   `<some root>/profiles/<name>` (case (b));
/// - marker naming another root's profile when the local root has no
///   `config.toml` for that profile (case (c)).
///
/// Takes only paths and consults the filesystem once, for the profile
/// `config.toml` existence check.
fn names_another_roots_profile(root: &Path, marker_config_dir: &Path) -> Option<PathBuf> {
    let profile_name = marker_config_dir.file_name()?;
    let profiles_dir = marker_config_dir.parent()?.file_name()?;
    if profiles_dir != "profiles" {
        return None;
    }
    // Case (a): the marker points inside the default config root (a relative
    // marker dir resolved earlier, or an absolute path under the default
    // config dir). Honor it as today.
    if marker_config_dir.starts_with(root) {
        return None;
    }
    let local_profile = root.join("profiles").join(profile_name);
    // Case (c): the local root has no config of its own for that profile,
    // so the marker is the only way to reach it. Honor it.
    if !local_profile.join("config.toml").exists() {
        return None;
    }
    Some(local_profile)
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

/// The marker override, when the marker names a directory the loader
/// honors as an override.
fn marker_override() -> Option<ConfigDirOverride> {
    let root = super::paths::rantaiclaw_root();
    // A marker that cannot be read names no override, the same answer the
    // loader gives for a marker it cannot read.
    let contents = std::fs::read_to_string(root.join(ACTIVE_WORKSPACE_MARKER)).ok()?;
    match marker_decision(&root, Some(&contents)) {
        MarkerDecision::Override(path) => Some(ConfigDirOverride {
            source: OverrideSource::Marker,
            path,
            profile: None,
        }),
        // Every other shape is set aside by the loader too, so no override is
        // in effect and nothing is refused.
        _ => None,
    }
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
///
/// This is THE rule for "is this override variable set?". The config loader
/// ([`crate::config::schema::resolve_runtime_config_dirs`]) and
/// [`apply_env_overrides`] use it too, so a value of spaces is unset for the
/// loader and for the door alike. A loader that read the variable untrimmed
/// would resolve a config directory nobody named, while the door — which
/// trims — would let a profile switch through that the loader then ignores.
pub fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    /// A root that is deliberately NOT under `std::env::temp_dir()`: a
    /// throwaway spot in this crate's `target/`, standing in for a real
    /// install. The temp-leak rule only fires for such a root.
    fn root_outside_the_temp_dir() -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("rantaiclaw_test_root_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("scratch root");
        assert!(
            !crate::profile::dev_guard::is_under_temp_dir(&root),
            "test setup bug: a root under the OS temp dir would defeat these tests"
        );
        root
    }

    /// A second root outside the temp dir, standing in for the tree a marker
    /// was copied or moved from.
    fn other_root() -> PathBuf {
        let other = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "rantaiclaw_test_root_other_{}",
                uuid::Uuid::new_v4()
            ));
        std::fs::create_dir_all(&other).expect("scratch other root");
        other
    }

    /// The contents of a marker naming `config_dir`.
    fn marker_contents(config_dir: &Path) -> String {
        format!("config_dir = {:?}\n", config_dir.to_string_lossy())
    }

    /// A marker naming a profile directory of THIS root says "the active
    /// profile": it is a pointer, not an override.
    #[test]
    fn a_marker_naming_this_roots_profile_is_a_pointer_not_an_override() {
        let root = root_outside_the_temp_dir();
        let profile_dir = root.join("profiles/work");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let contents = marker_contents(&profile_dir);

        assert_eq!(
            marker_decision(&root, Some(&contents)),
            MarkerDecision::ProfilePointer {
                config_dir: profile_dir
            }
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A marker that was copied or moved along with the tree names another
    /// root's profile. When this root has its own config for that profile,
    /// the marker is set aside.
    #[test]
    fn a_marker_naming_another_roots_profile_is_set_aside_when_this_root_has_its_own_config() {
        let root = root_outside_the_temp_dir();
        let local_profile = root.join("profiles/work");
        std::fs::create_dir_all(&local_profile).unwrap();
        std::fs::write(local_profile.join("config.toml"), "").unwrap();
        let other_tree = other_root();
        let other = other_tree.join(".rantaiclaw/profiles/work");
        std::fs::create_dir_all(&other).unwrap();
        let contents = marker_contents(&other);

        assert_eq!(
            marker_decision(&root, Some(&contents)),
            MarkerDecision::AnotherRootsProfile {
                config_dir: other,
                local_profile,
            }
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other_tree);
    }

    /// The same marker, but this root has NO config of its own for that
    /// profile: the marker is the only way to reach it, so it is honored.
    #[test]
    fn a_marker_naming_another_roots_profile_is_honored_without_a_local_config() {
        let root = root_outside_the_temp_dir();
        let other_tree = other_root();
        let other = other_tree.join(".rantaiclaw/profiles/lonely");
        std::fs::create_dir_all(&other).unwrap();
        let contents = marker_contents(&other);

        assert_eq!(
            marker_decision(&root, Some(&contents)),
            MarkerDecision::Override(other.clone())
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other_tree);
    }

    /// A marker naming a custom directory — not shaped like
    /// `<some root>/profiles/<name>` — stays an override.
    #[test]
    fn a_marker_naming_a_custom_directory_is_honored() {
        let root = root_outside_the_temp_dir();
        let custom = root.join("workspaces/custom-thing");
        std::fs::create_dir_all(&custom).unwrap();
        let contents = marker_contents(&custom);

        assert_eq!(
            marker_decision(&root, Some(&contents)),
            MarkerDecision::Override(custom)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A marker pointing into the OS temp dir is a leaked ephemeral
    /// workspace when the root itself is not under the temp dir.
    #[test]
    fn a_marker_under_the_temp_dir_is_set_aside_for_a_real_install() {
        let root = root_outside_the_temp_dir();
        let leaked = std::env::temp_dir().join("rantaiclaw-leaked-workspace");
        let contents = marker_contents(&leaked);

        assert_eq!(
            marker_decision(&root, Some(&contents)),
            MarkerDecision::TempLeak { config_dir: leaked }
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The temp-leak rule exempts a test setup whose root is under the temp
    /// dir too: such a marker is honored.
    #[test]
    fn a_marker_under_the_temp_dir_is_honored_when_the_root_is_under_it_too() {
        let root = tempfile::tempdir().expect("temp root");
        let marker_dir = root.path().join("profiles/work");
        std::fs::create_dir_all(&marker_dir).unwrap();
        let contents = marker_contents(&marker_dir);

        assert_eq!(
            marker_decision(root.path(), Some(&contents)),
            MarkerDecision::ProfilePointer {
                config_dir: marker_dir
            }
        );
    }

    /// A marker that cannot be used — empty, unparsable, or absent — is no
    /// override.
    #[test]
    fn a_marker_that_cannot_be_used_is_no_override() {
        let root = root_outside_the_temp_dir();

        assert_eq!(
            marker_decision(&root, None),
            MarkerDecision::Unusable(MarkerProblem::Missing)
        );

        assert_eq!(
            marker_decision(&root, Some("config_dir = \"\"\n")),
            MarkerDecision::Unusable(MarkerProblem::Empty)
        );

        assert!(matches!(
            marker_decision(&root, Some("config_dir = \n")),
            MarkerDecision::Unusable(MarkerProblem::Unparseable(_))
        ));

        assert!(matches!(
            marker_decision(&root, Some("config_dir = 42\n")),
            MarkerDecision::Unusable(MarkerProblem::Unparseable(_))
        ));

        let _ = std::fs::remove_dir_all(&root);
    }
}
