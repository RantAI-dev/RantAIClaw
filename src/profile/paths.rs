//! Path computation for the profile-aware storage layout.
//!
//! Release builds do no I/O here: every helper just builds a `PathBuf`. A
//! debug build (`cfg(any(test, debug_assertions))`) may canonicalize `HOME` and
//! create the temp redirect root when `HOME` is outside the temp dir; see
//! [`root_for_home`]. This is the
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
///
/// A debug build whose `HOME` is outside the temp dir gets a per-process temp
/// root instead, so it cannot touch the real tree; see [`root_for_home`].
pub fn rantaiclaw_root() -> PathBuf {
    root_for_home(&home_dir())
}

/// `home/.rantaiclaw`, the one place every root is derived from. Under
/// `cfg(any(test, debug_assertions))` a `home` outside the temp dir is
/// redirected to a per-process temp root (see `dev_guard::redirected_root`);
/// a release build always returns `home/.rantaiclaw`.
pub fn root_for_home(home: &std::path::Path) -> PathBuf {
    #[cfg(any(test, debug_assertions))]
    if let Some(root) = super::dev_guard::redirected_root(home) {
        return root;
    }
    home.join(".rantaiclaw")
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

/// `~/.rantaiclaw/profiles/<name>/sessions/sessions.db` — the per-profile
/// SQLite session history. Before v0.7.x this leaked to a single global XDG
/// data dir shared by every profile; see `migration::maybe_migrate_global_sessions_db`.
pub fn sessions_db(profile: &str) -> PathBuf {
    sessions_dir(profile).join("sessions.db")
}

/// `~/.rantaiclaw/profiles/<name>/kb.db` — the per-profile knowledge-base
/// SQLite database. Like `sessions_db`, this leaked to a single global XDG
/// data dir pre-v0.7.x; see `migration::maybe_migrate_global_kb_db`. Kept at
/// the profile root (not a subdir) — it is a single file, not a tree.
pub fn kb_db(profile: &str) -> PathBuf {
    profile_dir(profile).join("kb.db")
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

    /// A directory that is deliberately NOT under `std::env::temp_dir()`: a
    /// throwaway spot in this crate's `target/`, standing in for a real home.
    fn home_outside_the_temp_dir() -> PathBuf {
        let home = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "rantaiclaw_test_home_outside_{}",
                uuid::Uuid::new_v4()
            ));
        std::fs::create_dir_all(&home).expect("scratch home");
        assert!(
            !crate::profile::dev_guard::is_under_temp_dir(&home),
            "test setup bug: a target dir under the OS temp dir would defeat this test"
        );
        home
    }

    #[test]
    fn the_root_leaves_a_home_outside_the_temp_dir_untouched() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let home = home_outside_the_temp_dir();
        let _g_home = crate::test_env::EnvGuard::set("HOME", &home);
        let _g_allow = crate::test_env::EnvGuard::unset("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");

        let root = rantaiclaw_root();

        assert!(
            crate::profile::dev_guard::is_under_temp_dir(&root),
            "a debug build must not resolve the root under a home outside the temp dir: {root:?}"
        );
        assert!(!root.starts_with(&home));
        assert_eq!(
            root,
            rantaiclaw_root(),
            "the redirect must be stable in one process"
        );
        assert!(
            profile_dir("alpha").starts_with(&root),
            "every derived path must follow the redirected root"
        );
        assert_eq!(
            std::fs::read_dir(&home).expect("read scratch home").count(),
            0,
            "resolving the root must not create anything under the real home"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_root_follows_home_when_the_opt_out_is_set() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let home = home_outside_the_temp_dir();
        let _g_home = crate::test_env::EnvGuard::set("HOME", &home);
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");

        assert_eq!(rantaiclaw_root(), home.join(".rantaiclaw"));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_root_follows_a_home_under_the_temp_dir() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let home = tempfile::tempdir().expect("temp home");
        let _g_home = crate::test_env::EnvGuard::set("HOME", home.path());
        let _g_allow = crate::test_env::EnvGuard::unset("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");

        assert_eq!(rantaiclaw_root(), home.path().join(".rantaiclaw"));
    }
}
