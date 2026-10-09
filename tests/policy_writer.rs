//! Wave 4A — `policy_writer` integration tests.
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §6 "Approval runtime" + §"Preset bundles" (Manual / Smart / Strict / Off).
//!
//! `write_policy_files(profile, preset, force)` materialises two TOML
//! files under `<profile>/policy/`:
//!   * `autonomy.toml`          — mode + preset metadata
//!   * `command_allowlist.toml` — glob patterns for pre-approved commands
//!
//! The credential and system directories are not a file: they are the
//! built-in floor in `SecurityPolicy`.
//!
//! Bundles ship as `include_str!` resources in `src/approval/presets/`.
//!
//! These tests redirect `$HOME` to a `tempfile::TempDir` and serialize on
//! a global `Mutex` (`std::env::set_var` is process-global; cargo runs
//! integration tests in parallel by default).

use std::sync::Mutex;

use rantaiclaw::approval::policy_writer::{self, PolicyPreset};
use rantaiclaw::profile::ProfileManager;
use tempfile::TempDir;

static HOME_LOCK: Mutex<()> = Mutex::new(());

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

/// A preset writes no `forbidden_paths.toml`: the credential and system
/// directories are the built-in floor, not a file an operator can edit.
fn assert_no_forbidden_paths_file(profile: &rantaiclaw::profile::Profile) {
    assert!(
        !profile.policy_dir().join("forbidden_paths.toml").exists(),
        "a preset must not write forbidden_paths.toml"
    );
}

fn assert_policy_files_exist(profile: &rantaiclaw::profile::Profile) {
    let dir = profile.policy_dir();
    for f in ["autonomy.toml", "command_allowlist.toml"] {
        assert!(
            dir.join(f).exists(),
            "expected {} under {}",
            f,
            dir.display()
        );
    }
}

#[test]
fn manual_writes_two_policy_files_with_manual_mode_and_empty_allowlist() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        policy_writer::write_policy_files(&profile, PolicyPreset::Manual, false)
            .expect("Manual write should succeed");

        assert_policy_files_exist(&profile);

        let autonomy = std::fs::read_to_string(profile.policy_dir().join("autonomy.toml")).unwrap();
        assert!(autonomy.contains("preset = \"manual\""));
        assert!(autonomy.contains("mode = \"manual\""));

        let allowlist =
            std::fs::read_to_string(profile.policy_dir().join("command_allowlist.toml")).unwrap();
        // Manual ships with no preallowed patterns.
        assert!(
            allowlist.contains("patterns = []"),
            "Manual allowlist should be empty, got:\n{}",
            allowlist
        );

        assert_no_forbidden_paths_file(&profile);
    });
}

#[test]
fn smart_seeds_safe_read_only_commands() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        policy_writer::write_policy_files(&profile, PolicyPreset::Smart, false)
            .expect("Smart write should succeed");
        assert_policy_files_exist(&profile);

        let autonomy = std::fs::read_to_string(profile.policy_dir().join("autonomy.toml")).unwrap();
        assert!(autonomy.contains("preset = \"smart\""));

        let allowlist =
            std::fs::read_to_string(profile.policy_dir().join("command_allowlist.toml")).unwrap();
        for needle in ["\"ls\"", "\"cat *\"", "\"git status\"", "\"grep *\""] {
            assert!(
                allowlist.contains(needle),
                "Smart allowlist should contain {needle}, got:\n{allowlist}"
            );
        }

        assert_no_forbidden_paths_file(&profile);
    });
}

#[test]
fn strict_uses_strict_mode_with_safe_write_seeds() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        policy_writer::write_policy_files(&profile, PolicyPreset::Strict, false)
            .expect("Strict write should succeed");
        assert_policy_files_exist(&profile);

        let autonomy = std::fs::read_to_string(profile.policy_dir().join("autonomy.toml")).unwrap();
        assert!(autonomy.contains("preset = \"strict\""));
        assert!(
            autonomy.contains("mode = \"strict\""),
            "Strict must run in strict mode, got:\n{autonomy}"
        );

        let allowlist =
            std::fs::read_to_string(profile.policy_dir().join("command_allowlist.toml")).unwrap();
        // Strict keeps the read-only seeds *and* adds safe-write entries.
        // `memory_*` already covers `memory_store`, so it must not get its own
        // line; `skill_install` is not a tool and must not be auto-approved.
        assert!(allowlist.contains("\"memory_*\""));
        assert!(allowlist.contains("\"cron_*\""));
        assert!(allowlist.contains("\"session_*\""));
        assert!(!allowlist.contains("\"memory_store *\""));
        assert!(!allowlist.contains("\"skill_install *\""));
    });
}

#[test]
fn off_disables_gating_and_keeps_secret_floor() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        policy_writer::write_policy_files(&profile, PolicyPreset::Off, false)
            .expect("Off write should succeed");
        assert_policy_files_exist(&profile);

        let autonomy = std::fs::read_to_string(profile.policy_dir().join("autonomy.toml")).unwrap();
        assert!(autonomy.contains("preset = \"off\""));
        assert!(
            autonomy.contains("mode = \"off\""),
            "Off must set mode=off, got:\n{autonomy}"
        );

        // The Off preset keeps the credential floor in code, not in a file.
        assert_no_forbidden_paths_file(&profile);
    });
}

#[test]
fn write_is_idempotent_without_force() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        policy_writer::write_policy_files(&profile, PolicyPreset::Smart, false).unwrap();

        // Mutate the autonomy file as the user would.
        let path = profile.policy_dir().join("autonomy.toml");
        std::fs::write(&path, "# user-edited\npreset = \"custom\"\n").unwrap();

        // Re-run without force — must NOT clobber the user edit.
        policy_writer::write_policy_files(&profile, PolicyPreset::Manual, false).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("# user-edited"),
            "non-force write must preserve user-edited content, got:\n{after}"
        );
    });
}

#[test]
fn force_overwrites_existing_files() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        policy_writer::write_policy_files(&profile, PolicyPreset::Manual, false).unwrap();

        // Force-rewrite with Off. autonomy.toml must now reflect Off.
        policy_writer::write_policy_files(&profile, PolicyPreset::Off, true).unwrap();

        let autonomy = std::fs::read_to_string(profile.policy_dir().join("autonomy.toml")).unwrap();
        assert!(autonomy.contains("preset = \"off\""));
        assert!(autonomy.contains("mode = \"off\""));
    });
}

#[test]
fn preset_str_round_trips() {
    // Quick sanity over the four variants — mostly to catch a future
    // accidental rename without the test breaking compile-time.
    let cases: [(&str, PolicyPreset); 4] = [
        ("manual", PolicyPreset::Manual),
        ("smart", PolicyPreset::Smart),
        ("strict", PolicyPreset::Strict),
        ("off", PolicyPreset::Off),
    ];
    for (name, expected) in cases {
        assert_eq!(PolicyPreset::from_str_ci(name).unwrap(), expected);
        assert_eq!(expected.id(), name);
    }
    // Legacy ids still parse for backward compat with old configs.
    assert_eq!(
        PolicyPreset::from_str_ci("L1").unwrap(),
        PolicyPreset::Manual
    );
    assert_eq!(PolicyPreset::from_str_ci("L4").unwrap(), PolicyPreset::Off);
    assert!(PolicyPreset::from_str_ci("L99").is_err());
    assert!(PolicyPreset::from_str_ci("paranoid").is_err());
}
