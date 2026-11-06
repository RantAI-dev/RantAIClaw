//! The login files an earlier RantaiClaw wrote, and the sweep that removes them.
//!
//! RantaiClaw used to keep subscription logins on disk: the Anthropic auth
//! profile store beside `config.toml`, and the Copilot token pair under the
//! platform config directory. Both logins are gone (plans 640 and 641), so
//! nothing reads those files any more — but an install that ran an older
//! build still has them, and a credential left on disk is a credential the
//! operator did not ask to keep.
//!
//! The sweep runs on every start and removes what it finds. It is idempotent:
//! a file that is not there produces no line and no error, so only the first
//! start after an upgrade has anything to remove.
//!
//! Each file here holds LOGIN DATA and nothing else:
//!
//! - `auth-profiles.json` — a JSON map of provider profiles, each with a
//!   token or an OAuth token set.
//! - `auth-profiles.lock` — the lock file beside it.
//! - `auth-openai-pending.json` — the pending-login state of the OpenAI
//!   OAuth flow. Nothing writes it any more; the entry stays so a leftover
//!   file is swept.
//! - `access-token` / `api-key.json` — the Copilot GitHub OAuth token pair.
//!
//! Nothing outside this list is touched: `~/.gemini` and `~/.qwen` belong to
//! other CLIs, and the platform config directory is only read for the two
//! Copilot files named above.

use std::path::{Path, PathBuf};

use crate::config::Config;

/// The auth-profile store an earlier RantaiClaw wrote beside `config.toml`.
const AUTH_PROFILES_FILE: &str = "auth-profiles.json";

/// The lock file beside the auth-profile store.
const AUTH_PROFILES_LOCK_FILE: &str = "auth-profiles.lock";

/// The pending-login state of the removed OpenAI OAuth flow. Nothing writes it
/// any more; the entry stays so a leftover file is swept.
const AUTH_OPENAI_PENDING_FILE: &str = "auth-openai-pending.json";

/// The GitHub OAuth token the removed Copilot login wrote.
const COPILOT_ACCESS_TOKEN_FILE: &str = "access-token";

/// The Copilot API key file the removed Copilot login wrote.
const COPILOT_API_KEY_FILE: &str = "api-key.json";

/// The directory the Copilot files live in, under the OS platform config
/// directory: `$XDG_CONFIG_HOME/rantaiclaw/copilot` on Linux,
/// `~/Library/Application Support/rantaiclaw/copilot` on macOS,
/// `%APPDATA%\rantaiclaw\copilot` on Windows.
///
/// This is NOT the active profile's config directory. The old Copilot login
/// wrote beside the platform config dir, so that is where its files are.
///
/// `None` when the platform has no config directory at all — a container with
/// no home, say. Then there is nothing to remove.
fn platform_copilot_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "rantaiclaw")
        .map(|dirs| dirs.config_dir().join("copilot"))
}

/// The paths the legacy login files live at, whether or not they exist.
///
/// The order is stable: the active profile's config directory first, then the
/// platform config directory.
pub fn legacy_login_file_paths(config: &Config) -> Vec<PathBuf> {
    let state_dir = crate::config::state_dir_from_config(config);
    let mut paths = vec![
        state_dir.join(AUTH_PROFILES_FILE),
        state_dir.join(AUTH_PROFILES_LOCK_FILE),
        state_dir.join(AUTH_OPENAI_PENDING_FILE),
    ];
    if let Some(copilot_dir) = platform_copilot_dir() {
        paths.push(copilot_dir.join(COPILOT_ACCESS_TOKEN_FILE));
        paths.push(copilot_dir.join(COPILOT_API_KEY_FILE));
    }
    paths
}

/// The log line for a removed legacy login file.
///
/// The line names the path and NOTHING else: the file held a credential, and
/// a log is the wrong place for one.
pub fn removal_log_line(path: &Path) -> String {
    format!("removed legacy login file: {}", path.display())
}

/// Remove the login files an earlier RantaiClaw left on disk, and log one line
/// per removed path.
///
/// Returns the paths it actually removed, so a caller can report them. A file
/// that is not there is not an error and produces no line: the sweep runs on
/// every start, and only the first one has something to remove.
pub fn remove_legacy_login_files(config: &Config) -> Vec<PathBuf> {
    let removed = remove_paths(&legacy_login_file_paths(config));
    for path in &removed {
        tracing::info!("{}", removal_log_line(path));
    }
    removed
}

/// Remove the files at `paths` that exist, and return the ones removed.
///
/// A file that cannot be removed — a read-only directory, a file owned by
/// someone else — is left alone and reported by `doctor`, which is the only
/// place that can say so without failing a start.
fn remove_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|path| path.is_file())
        .filter(|path| std::fs::remove_file(path).is_ok())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::ENV_LOCK;

    /// A config whose state dir is a scratch directory, so the sweep never
    /// touches the operator's own install.
    fn scratch_config(dir: &Path) -> Config {
        let mut config = Config::default();
        config.config_path = dir.join("config.toml");
        config
    }

    /// The legacy files are removed from the active profile's config
    /// directory and from the platform config directory, and nothing else in
    /// those directories is touched.
    #[test]
    fn the_sweep_removes_the_legacy_login_files_and_nothing_else() {
        let _env = ENV_LOCK.blocking_lock();
        let _xdg = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", "/nonexistent-xdg-root");
        let scratch = tempfile::tempdir().expect("scratch dir");
        let state_dir = scratch.path().join("profile");
        std::fs::create_dir_all(&state_dir).expect("state dir");

        // The files an earlier RantaiClaw left behind.
        for name in [
            AUTH_PROFILES_FILE,
            AUTH_PROFILES_LOCK_FILE,
            AUTH_OPENAI_PENDING_FILE,
        ] {
            std::fs::write(state_dir.join(name), "legacy-login-data").expect("write legacy file");
        }
        // A file that is none of our business, in the same directory.
        std::fs::write(state_dir.join("config.toml"), "keep me").expect("write config");

        let config = scratch_config(&state_dir);
        let removed = remove_legacy_login_files(&config);

        assert_eq!(removed.len(), 3, "removed: {removed:?}");
        for name in [
            AUTH_PROFILES_FILE,
            AUTH_PROFILES_LOCK_FILE,
            AUTH_OPENAI_PENDING_FILE,
        ] {
            assert!(!state_dir.join(name).exists(), "{name} must be gone");
            assert!(
                removed.contains(&state_dir.join(name)),
                "{name} must be reported as removed: {removed:?}"
            );
        }
        assert!(
            state_dir.join("config.toml").exists(),
            "a file that is not a legacy login file must survive"
        );
    }

    /// The second sweep has nothing to do: it removes nothing, and it logs
    /// nothing either.
    #[test]
    fn the_sweep_is_idempotent() {
        let _env = ENV_LOCK.blocking_lock();
        let _xdg = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", "/nonexistent-xdg-root");
        let scratch = tempfile::tempdir().expect("scratch dir");
        let state_dir = scratch.path().join("profile");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        std::fs::write(state_dir.join(AUTH_PROFILES_FILE), "legacy").expect("write legacy file");

        let config = scratch_config(&state_dir);
        let first = remove_legacy_login_files(&config);
        let second = remove_legacy_login_files(&config);

        assert_eq!(first.len(), 1, "first sweep: {first:?}");
        assert!(second.is_empty(), "second sweep: {second:?}");
    }

    /// The Copilot files live in the PLATFORM config directory, not beside
    /// `config.toml`, and they are removed from there.
    #[test]
    fn the_copilot_files_are_removed_from_the_platform_config_directory() {
        let _env = ENV_LOCK.blocking_lock();
        let scratch = tempfile::tempdir().expect("scratch dir");
        let xdg = scratch.path().join("xdg");
        let copilot_dir = xdg.join("rantaiclaw").join("copilot");
        std::fs::create_dir_all(&copilot_dir).expect("copilot dir");
        std::fs::write(copilot_dir.join(COPILOT_ACCESS_TOKEN_FILE), "token").expect("write token");
        std::fs::write(copilot_dir.join(COPILOT_API_KEY_FILE), "token").expect("write key");
        let _xdg = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", &xdg);

        let state_dir = scratch.path().join("profile");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let config = scratch_config(&state_dir);
        let removed = remove_legacy_login_files(&config);

        assert!(!copilot_dir.join(COPILOT_ACCESS_TOKEN_FILE).exists());
        assert!(!copilot_dir.join(COPILOT_API_KEY_FILE).exists());
        assert_eq!(removed.len(), 2, "removed: {removed:?}");
        assert!(removed.contains(&copilot_dir.join(COPILOT_ACCESS_TOKEN_FILE)));
        assert!(removed.contains(&copilot_dir.join(COPILOT_API_KEY_FILE)));
    }

    /// The log line names the path and nothing else. The file held a
    /// credential: a log must not become a second copy of it.
    #[test]
    fn the_log_line_names_the_path_and_not_the_contents() {
        let secret = "sk-ant-oat01-SECRET-MARKER";
        let line = removal_log_line(Path::new("/tmp/rantaiclaw/auth-profiles.json"));
        assert!(line.contains("auth-profiles.json"), "{line}");
        assert!(!line.contains(secret), "{line}");
        assert!(!line.contains("token"), "{line}");
    }

    /// The sweep never touches the other CLIs' directories.
    #[test]
    fn the_sweep_never_touches_the_other_clis_directories() {
        let _env = ENV_LOCK.blocking_lock();
        let _xdg = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", "/nonexistent-xdg-root");
        let scratch = tempfile::tempdir().expect("scratch dir");
        let home = scratch.path().join("home");
        for name in [".gemini", ".qwen"] {
            let dir = home.join(name);
            std::fs::create_dir_all(&dir).expect("other cli dir");
            std::fs::write(dir.join("marker"), "keep me").expect("write marker");
        }
        let _home = crate::test_env::EnvGuard::set("HOME", &home);

        let state_dir = scratch.path().join("profile");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let config = scratch_config(&state_dir);
        remove_legacy_login_files(&config);

        for name in [".gemini", ".qwen"] {
            assert!(
                home.join(name).join("marker").exists(),
                "{name} belongs to another CLI and must survive"
            );
        }
    }
}
