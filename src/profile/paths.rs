//! Pure path computation for the profile-aware storage layout.
//!
//! No I/O lives here — every helper just builds a `PathBuf`. This is the
//! single source of truth for the on-disk shape introduced in v0.5.0 (see
//! `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Storage layout"). All path concatenation in `src/profile/` and
//! consumers in other modules should go through these helpers — there is no
//! reason for any other call site to hand-build `~/.rantaiclaw/...`.

use std::path::PathBuf;

/// User home directory.
///
/// We prefer the `directories` crate (already a dependency) over `dirs` so
/// the rest of the codebase stays consistent. `directories::UserDirs` reads
/// `$HOME` on Linux/macOS at construction time, which makes
/// `std::env::set_var("HOME", tmp.path())` test patterns work.
pub fn home_dir() -> PathBuf {
    directories::UserDirs::new()
        .map(|u| u.home_dir().to_path_buf())
        .expect("HOME must be set")
}

/// `~/.rantaiclaw` — the global root for everything RantaiClaw owns on disk.
pub fn rantaiclaw_root() -> PathBuf {
    home_dir().join(".rantaiclaw")
}

/// `~/.rantaiclaw/profiles/<name>` — the per-profile root.
pub fn profile_dir(name: &str) -> PathBuf {
    rantaiclaw_root().join("profiles").join(name)
}

/// `~/.rantaiclaw/active_profile` — plain-text file containing the active
/// profile name. Resolution order: CLI flag → env var → this file → "default".
pub fn active_profile_file() -> PathBuf {
    rantaiclaw_root().join("active_profile")
}

/// `~/.rantaiclaw/version` — installed binary version stamp written on
/// migration / first-run.
pub fn version_file() -> PathBuf {
    rantaiclaw_root().join("version")
}

/// `~/.rantaiclaw/migrate.lock` — flock target so concurrent invocations
/// cannot race the legacy-layout migration.
pub fn migration_lock_file() -> PathBuf {
    rantaiclaw_root().join("migrate.lock")
}

// Per-profile sub-paths. All callers go through these; no string
// concatenation elsewhere.

pub fn config_toml(profile: &str) -> PathBuf {
    profile_dir(profile).join("config.toml")
}

pub fn config_staging(profile: &str) -> PathBuf {
    profile_dir(profile).join("config.toml.staging")
}

pub fn workspace_dir(profile: &str) -> PathBuf {
    profile_dir(profile).join("workspace")
}

pub fn memory_dir(profile: &str) -> PathBuf {
    profile_dir(profile).join("memory")
}

pub fn sessions_dir(profile: &str) -> PathBuf {
    profile_dir(profile).join("sessions")
}

pub fn skills_dir(profile: &str) -> PathBuf {
    profile_dir(profile).join("skills")
}

pub fn persona_dir(profile: &str) -> PathBuf {
    profile_dir(profile).join("persona")
}

pub fn policy_dir(profile: &str) -> PathBuf {
    profile_dir(profile).join("policy")
}

pub fn secrets_dir(profile: &str) -> PathBuf {
    profile_dir(profile).join("secrets")
}

pub fn runtime_dir(profile: &str) -> PathBuf {
    profile_dir(profile).join("runtime")
}

pub fn audit_log(profile: &str) -> PathBuf {
    profile_dir(profile).join("audit.log")
}

pub fn onboard_progress(profile: &str) -> PathBuf {
    profile_dir(profile).join(".onboard_progress")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_dir_includes_profiles_subdir() {
        let p = profile_dir("alpha");
        assert!(p.ends_with("profiles/alpha"));
    }

    #[test]
    fn config_toml_inside_profile_dir() {
        let cfg = config_toml("alpha");
        assert!(cfg.ends_with("profiles/alpha/config.toml"));
    }

    #[test]
    fn audit_log_inside_profile_dir() {
        let log = audit_log("alpha");
        assert!(log.ends_with("profiles/alpha/audit.log"));
    }

    #[test]
    fn active_profile_file_at_root() {
        let p = active_profile_file();
        assert!(p.ends_with(".rantaiclaw/active_profile"));
    }
}
