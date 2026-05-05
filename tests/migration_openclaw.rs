//! Integration tests for the OpenClaw / ZeroClaw → RantaiClaw profile
//! migration. Spec §7.2 of the onboarding-depth-v2 design doc.
//!
//! Each test redirects `$HOME` to a tempdir before touching anything that
//! resolves through `paths::rantaiclaw_root()`. A global `Mutex` serialises
//! tests because `std::env::set_var` is process-global. The pattern matches
//! `tests/migrate_legacy.rs` and `tests/profile_lifecycle.rs`.
//!
//! Fixture configs (under `tests/fixtures/`):
//!
//! * `openclaw_v0.3/` — minimal v0.3-shaped layout. `[provider]` (singular)
//!   block, modest `[gateway]`, one skill, two API keys. Locks down the
//!   "translate happy path on the simplest realistic input" case.
//! * `openclaw_v0.4/` — last-pre-rename layout. `[providers.openrouter]` /
//!   `[providers.anthropic]` (plural-table form), richer `[gateway]` with
//!   `paired_tokens`, two skill directories, three API keys. Locks down
//!   variant detection on a `~/.zeroclaw`-shaped directory and verifies
//!   N-skill copy.

use std::path::Path;
use std::sync::Mutex;

use rantaiclaw::migration::{self, openclaw, MigrationSource};
use rantaiclaw::profile::paths;
use tempfile::TempDir;

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with `$HOME` pointed at a tempdir. Restores the previous value
/// (or removes it) before returning, even on panic.
fn with_home<F: FnOnce(&Path)>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().expect("tempdir");
    let prev_home = std::env::var_os("HOME");
    let prev_profile = std::env::var_os("RANTAICLAW_PROFILE");
    std::env::set_var("HOME", tmp.path());
    std::env::remove_var("RANTAICLAW_PROFILE");
    let home = tmp.path().to_path_buf();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&home)));
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

/// Path to a fixture relative to the cargo manifest dir.
fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Copy a fixture tree into `dest` (mkdir + recursive copy). Used so each
/// test gets its own writable mirror of the immutable fixture.
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Detection
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn detect_finds_openclaw_in_dot_openclaw() {
    with_home(|home| {
        let root = home.join(".openclaw");
        copy_tree(&fixture("openclaw_v0.3"), &root);

        let detected = openclaw::detect().expect("auto-detect should find ~/.openclaw");
        assert_eq!(detected.root, root);
        assert_eq!(detected.variant, openclaw::SourceVariant::OpenClaw);
    });
}

#[test]
fn detect_recognises_zeroclaw_under_dot_zeroclaw() {
    with_home(|home| {
        let root = home.join(".zeroclaw");
        copy_tree(&fixture("openclaw_v0.4"), &root);

        let detected = openclaw::detect().expect("auto-detect should find ~/.zeroclaw");
        assert_eq!(detected.variant, openclaw::SourceVariant::ZeroClaw);
    });
}

#[test]
fn detect_prefers_dot_openclaw_over_dot_zeroclaw() {
    with_home(|home| {
        copy_tree(&fixture("openclaw_v0.3"), &home.join(".openclaw"));
        copy_tree(&fixture("openclaw_v0.4"), &home.join(".zeroclaw"));

        let detected = openclaw::detect().expect("auto-detect should succeed");
        // Per detection order: ~/.openclaw before ~/.zeroclaw.
        assert!(detected.root.ends_with(".openclaw"));
        assert_eq!(detected.variant, openclaw::SourceVariant::OpenClaw);
    });
}

#[test]
fn detect_falls_back_to_xdg_config_path() {
    with_home(|home| {
        let root = home.join(".config").join("openclaw");
        copy_tree(&fixture("openclaw_v0.3"), &root);
        let detected = openclaw::detect().expect("auto-detect should find ~/.config/openclaw");
        assert!(detected.root.ends_with(".config/openclaw"));
    });
}

#[test]
fn detect_returns_none_when_nothing_present() {
    with_home(|_home| {
        assert!(openclaw::detect().is_none());
    });
}

// ─────────────────────────────────────────────────────────────────────────
// Translate config (no I/O)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn translate_v03_fixture_preserves_provider_and_gateway() {
    let body = std::fs::read_to_string(fixture("openclaw_v0.3").join("config.toml")).unwrap();
    let (out, blocks) = openclaw::translate_config(&body);
    assert!(out.contains("[provider]"), "[provider] block must survive");
    assert!(out.contains("openrouter_base_url"));
    assert!(out.contains("[gateway]"));
    assert!(out.contains("port = 8765"));
    // synthesised blocks added
    assert!(
        out.contains("level = \"smart\""),
        "L2-Smart autonomy injected"
    );
    assert!(out.contains("[approvals]"));
    // 3 source blocks (preamble, [provider], [gateway]) + 2 synthesised
    assert_eq!(blocks, 5);
}

#[test]
fn translate_v04_fixture_preserves_plural_providers_table() {
    let body = std::fs::read_to_string(fixture("openclaw_v0.4").join("config.toml")).unwrap();
    let (out, _) = openclaw::translate_config(&body);
    assert!(out.contains("[providers.openrouter]"));
    assert!(out.contains("[providers.anthropic]"));
    assert!(out.contains("paired_tokens = [\"fake-token-A\", \"fake-token-B\"]"));
}

// ─────────────────────────────────────────────────────────────────────────
// Full migration via migrate_from_external
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn migrate_from_external_creates_profile_and_copies_skills_and_secrets() {
    with_home(|home| {
        let root = home.join(".openclaw");
        copy_tree(&fixture("openclaw_v0.4"), &root);

        let summary =
            migration::migrate_from_external(MigrationSource::Auto, "imported", false, None)
                .expect("auto-detect + migrate should succeed")
                .expect("Auto must return Some when a candidate is present");

        assert_eq!(summary.profile_name, "imported");
        assert_eq!(summary.skills_migrated, 2);
        assert_eq!(summary.secrets_migrated, 1);
        assert!(summary.config_blocks_migrated >= 3);

        // Verify on-disk shape under the new profile.
        let dest = paths::profile_dir("imported");
        assert!(dest.exists(), "profile dir created");
        assert!(dest.join("config.toml").exists(), "config.toml written");
        assert!(
            dest.join("skills/recipe-helper/SKILL.md").exists(),
            "first skill copied"
        );
        assert!(
            dest.join("skills/code-reviewer/SKILL.md").exists(),
            "second skill copied"
        );
        assert!(
            dest.join("secrets/api_keys.toml").exists(),
            "secrets copied"
        );

        // Translated config retains source content + synthesised defaults.
        let cfg = std::fs::read_to_string(dest.join("config.toml")).unwrap();
        assert!(cfg.contains("[providers.openrouter]"));
        assert!(cfg.contains("level = \"smart\""));
    });
}

#[test]
fn migrate_from_external_auto_returns_none_when_no_source_present() {
    with_home(|_home| {
        let result = migration::migrate_from_external(
            MigrationSource::Auto,
            "should-not-be-created",
            false,
            None,
        )
        .expect("Auto with nothing detected should be Ok(None), not Err");
        assert!(
            result.is_none(),
            "Auto + no candidate ⇒ Ok(None) so the CLI prints a friendly hint instead of panicking"
        );
        // No profile created.
        assert!(!paths::profile_dir("should-not-be-created").exists());
    });
}

#[test]
fn migrate_from_external_explicit_openclaw_errors_when_only_zeroclaw_present() {
    with_home(|home| {
        copy_tree(&fixture("openclaw_v0.3"), &home.join(".zeroclaw"));
        let err =
            migration::migrate_from_external(MigrationSource::OpenClaw, "imported", false, None)
                .expect_err("--from openclaw must not match a ~/.zeroclaw install");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("OpenClaw"),
            "error mentions the requested source: {msg}"
        );
    });
}

#[test]
fn migrate_from_external_refuses_existing_profile_without_force() {
    with_home(|home| {
        let root = home.join(".openclaw");
        copy_tree(&fixture("openclaw_v0.3"), &root);

        // First migration: succeeds.
        migration::migrate_from_external(MigrationSource::Auto, "alpha", false, None)
            .unwrap()
            .unwrap();

        // Second migration into the same name: must error without --force.
        let err = migration::migrate_from_external(MigrationSource::Auto, "alpha", false, None)
            .expect_err("re-migrating into existing profile must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("already exists"), "error: {msg}");

        // With --force=true, succeeds.
        migration::migrate_from_external(MigrationSource::Auto, "alpha", true, None)
            .expect("--force should overwrite")
            .unwrap();
    });
}

#[test]
fn migrate_from_external_accepts_explicit_source_root_override() {
    with_home(|home| {
        // Fixture lives in a non-standard location — only the override path
        // can find it.
        let root = home.join("custom-spot").join("legacy");
        copy_tree(&fixture("openclaw_v0.3"), &root);

        // Auto-detect would see nothing here.
        assert!(openclaw::detect().is_none());

        let summary = migration::migrate_from_external(
            MigrationSource::OpenClaw,
            "from-override",
            false,
            Some(&root),
        )
        .expect("override path should be honoured")
        .expect("override path returned a detected source");
        assert_eq!(summary.skills_migrated, 1);
    });
}
