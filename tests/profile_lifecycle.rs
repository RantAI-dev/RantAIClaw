//! Integration tests for `src/profile/` — Profile struct, ProfileManager,
//! and clone semantics. See spec §"Storage layout / Profile clone semantics".
//!
//! All tests redirect `$HOME` to a `tempfile::TempDir` so they never touch
//! the real `~/.rantaiclaw`. We serialize the suite via a global `Mutex`
//! because `std::env::set_var("HOME", ...)` is process-global and Cargo
//! runs tests in parallel by default.

use std::path::Path;
use std::sync::Mutex;

use rantaiclaw::profile::{paths, CloneOpts, ProfileManager};
use tempfile::TempDir;

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with `$HOME` pointed at a fresh tempdir. Restores afterwards.
fn with_home<F: FnOnce()>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().expect("tempdir");
    let prev_home = std::env::var_os("HOME");
    let prev_profile = std::env::var_os("RANTAICLAW_PROFILE");
    std::env::set_var("HOME", tmp.path());
    std::env::remove_var("RANTAICLAW_PROFILE");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    if let Some(h) = prev_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(p) = prev_profile {
        std::env::set_var("RANTAICLAW_PROFILE", p);
    } else {
        std::env::remove_var("RANTAICLAW_PROFILE");
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn write(p: &Path, contents: &str) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, contents).unwrap();
}

#[test]
fn ensure_default_creates_full_tree() {
    with_home(|| {
        let p = ProfileManager::ensure_default().unwrap();
        assert_eq!(p.name, "default");
        assert!(p.workspace_dir().exists(), "workspace/");
        assert!(p.memory_dir().exists(), "memory/");
        assert!(p.sessions_dir().exists(), "sessions/");
        assert!(p.skills_dir().exists(), "skills/");
        assert!(p.persona_dir().exists(), "persona/");
        assert!(p.policy_dir().exists(), "policy/");
        assert!(p.secrets_dir().exists(), "secrets/");
        assert!(p.runtime_dir().exists(), "runtime/");
    });
}

#[test]
fn ensure_is_idempotent() {
    with_home(|| {
        let _ = ProfileManager::ensure("alpha").unwrap();
        // Second call must not error and must still return the same dirs.
        let p = ProfileManager::ensure("alpha").unwrap();
        assert!(p.workspace_dir().exists());
    });
}

#[test]
fn create_clone_does_not_copy_memory_by_default() {
    with_home(|| {
        let src = ProfileManager::ensure("src").unwrap();
        write(&src.memory_dir().join("MEMORY.md"), "secret note");

        let dst = ProfileManager::create(
            "dst",
            Some("src"),
            CloneOpts {
                include_secrets: false,
                include_memory: false,
            },
        )
        .unwrap();
        assert!(!dst.memory_dir().join("MEMORY.md").exists());
    });
}

#[test]
fn create_clone_with_include_memory_copies_memory() {
    with_home(|| {
        let src = ProfileManager::ensure("src").unwrap();
        write(&src.memory_dir().join("MEMORY.md"), "carry me");

        let dst = ProfileManager::create(
            "dst",
            Some("src"),
            CloneOpts {
                include_secrets: false,
                include_memory: true,
            },
        )
        .unwrap();
        let copied = dst.memory_dir().join("MEMORY.md");
        assert!(copied.exists());
        assert_eq!(std::fs::read_to_string(&copied).unwrap(), "carry me");
    });
}

#[test]
fn create_clone_does_not_copy_secrets_by_default() {
    with_home(|| {
        let src = ProfileManager::ensure("src").unwrap();
        write(&src.secrets_dir().join("api_keys.toml"), "k = \"v\"\n");

        let dst = ProfileManager::create(
            "dst",
            Some("src"),
            CloneOpts {
                include_secrets: false,
                include_memory: false,
            },
        )
        .unwrap();
        assert!(!dst.secrets_dir().join("api_keys.toml").exists());
    });
}

#[test]
fn create_clone_with_include_secrets_copies_secrets() {
    with_home(|| {
        let src = ProfileManager::ensure("src").unwrap();
        write(&src.secrets_dir().join("api_keys.toml"), "k = \"v\"\n");

        let dst = ProfileManager::create(
            "dst",
            Some("src"),
            CloneOpts {
                include_secrets: true,
                include_memory: false,
            },
        )
        .unwrap();
        assert!(dst.secrets_dir().join("api_keys.toml").exists());
    });
}

#[test]
fn create_clone_copies_persona_and_skills_by_default() {
    with_home(|| {
        let src = ProfileManager::ensure("src").unwrap();
        write(&src.persona_dir().join("SYSTEM.md"), "# persona\n");
        write(&src.skills_dir().join("a/SKILL.md"), "# skill A\n");

        let dst = ProfileManager::create("dst", Some("src"), CloneOpts::default()).unwrap();
        assert!(dst.persona_dir().join("SYSTEM.md").exists());
        assert!(dst.skills_dir().join("a/SKILL.md").exists());
    });
}

#[test]
fn create_clone_does_not_copy_command_allowlist() {
    with_home(|| {
        let src = ProfileManager::ensure("src").unwrap();
        write(
            &src.policy_dir().join("command_allowlist.toml"),
            "patterns = []\n",
        );
        write(
            &src.policy_dir().join("forbidden_paths.toml"),
            "forbidden = []\n",
        );

        let dst = ProfileManager::create("dst", Some("src"), CloneOpts::default()).unwrap();
        // command_allowlist intentionally not copied (fresh start safer).
        assert!(!dst.policy_dir().join("command_allowlist.toml").exists());
        // forbidden_paths copied — same defaults still apply.
        assert!(dst.policy_dir().join("forbidden_paths.toml").exists());
    });
}

#[test]
fn list_returns_sorted_profiles() {
    with_home(|| {
        ProfileManager::ensure("zulu").unwrap();
        ProfileManager::ensure("alpha").unwrap();
        ProfileManager::ensure("mike").unwrap();
        let list = ProfileManager::list().unwrap();
        assert_eq!(list, vec!["alpha", "mike", "zulu"]);
    });
}

#[test]
fn list_returns_empty_when_no_profiles_dir() {
    with_home(|| {
        let list = ProfileManager::list().unwrap();
        assert!(list.is_empty());
    });
}

#[test]
fn use_profile_writes_active_profile_file() {
    with_home(|| {
        ProfileManager::ensure("work").unwrap();
        ProfileManager::use_profile("work").unwrap();
        let contents = std::fs::read_to_string(paths::active_profile_file()).unwrap();
        assert_eq!(contents.trim(), "work");
    });
}

#[test]
fn use_profile_refuses_unknown_profile() {
    with_home(|| {
        let err = ProfileManager::use_profile("ghost").unwrap_err();
        assert!(err.to_string().contains("ghost"));
    });
}

#[test]
fn delete_refuses_active_profile_without_force() {
    with_home(|| {
        ProfileManager::ensure("work").unwrap();
        ProfileManager::use_profile("work").unwrap();
        let err = ProfileManager::delete("work", false).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("active"),
            "expected refusal mentioning 'active', got: {err}"
        );
        // Still exists.
        assert!(paths::profile_dir("work").exists());
    });
}

#[test]
fn delete_force_clears_active_marker() {
    with_home(|| {
        ProfileManager::ensure("work").unwrap();
        ProfileManager::use_profile("work").unwrap();
        ProfileManager::delete("work", true).unwrap();
        assert!(!paths::profile_dir("work").exists());
        assert!(!paths::active_profile_file().exists());
    });
}

#[test]
fn resolve_active_name_priority_env_over_file() {
    with_home(|| {
        ProfileManager::ensure("from_file").unwrap();
        ProfileManager::use_profile("from_file").unwrap();
        std::env::set_var("RANTAICLAW_PROFILE", "from_env");
        assert_eq!(ProfileManager::resolve_active_name(), "from_env");
        std::env::remove_var("RANTAICLAW_PROFILE");
        // Without env var, falls back to file.
        assert_eq!(ProfileManager::resolve_active_name(), "from_file");
    });
}

#[test]
fn resolve_active_name_default_fallback() {
    with_home(|| {
        assert_eq!(ProfileManager::resolve_active_name(), "default");
    });
}

#[test]
fn validate_rejects_path_traversal_names() {
    with_home(|| {
        for bad in &["..", "a/b", "a\\b", ""] {
            let err = ProfileManager::ensure(bad).unwrap_err();
            assert!(
                err.to_string().to_lowercase().contains("profile name"),
                "bad name {bad:?} should be rejected, got: {err}"
            );
        }
    });
}
