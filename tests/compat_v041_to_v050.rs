//! Backward-compatibility test: a synthetic v0.4.1 `~/.rantaiclaw` tree
//! survives the upgrade to v0.5.0 with all data accounted for at the new
//! profile-aware paths. The fixture lives at
//! `tests/fixtures/legacy_layout/` so it can be inspected, edited, and
//! re-used by future regression tests.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`
//! §"Backward compatibility" and §7.1.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rantaiclaw::profile::{migration, paths, ProfileManager};
use tempfile::TempDir;

static HOME_LOCK: Mutex<()> = Mutex::new(());

fn fixture_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.join("tests/fixtures/legacy_layout")
}

fn copy_dir_into(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_into(&entry.path(), &target);
        } else if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Plant the v0.4.1 fixture under `<home>/.rantaiclaw/` and run `f`.
fn with_legacy_fixture<F: FnOnce(&Path)>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().expect("tempdir");
    let prev_home = std::env::var_os("HOME");
    let prev_profile = std::env::var_os("RANTAICLAW_PROFILE");
    std::env::set_var("HOME", tmp.path());
    std::env::remove_var("RANTAICLAW_PROFILE");

    let dest = tmp.path().join(".rantaiclaw");
    copy_dir_into(&fixture_root(), &dest);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(tmp.path())));
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

#[test]
fn v041_fixture_migrates_into_default_profile() {
    with_legacy_fixture(|home| {
        // Sanity: fixture planted with expected files.
        assert!(home.join(".rantaiclaw/config.toml").exists());
        assert!(home.join(".rantaiclaw/workspace/AGENTS.md").exists());
        assert!(home.join(".rantaiclaw/memory/MEMORY.md").exists());
        assert!(home
            .join(".rantaiclaw/skills/example-skill/SKILL.md")
            .exists());
        assert!(migration::needs_migration());

        // Run the migration the way load_or_init() will.
        let did = migration::maybe_migrate_legacy_layout().unwrap();
        assert!(did);

        let default = paths::profile_dir("default");

        // Every fixture file must have moved into profiles/default/ unchanged.
        let pairs: &[(PathBuf, PathBuf)] = &[
            (
                fixture_root().join("config.toml"),
                default.join("config.toml"),
            ),
            (
                fixture_root().join("workspace/AGENTS.md"),
                default.join("workspace/AGENTS.md"),
            ),
            (
                fixture_root().join("workspace/TOOLS.md"),
                default.join("workspace/TOOLS.md"),
            ),
            (
                fixture_root().join("memory/MEMORY.md"),
                default.join("memory/MEMORY.md"),
            ),
            (
                fixture_root().join("skills/example-skill/SKILL.md"),
                default.join("skills/example-skill/SKILL.md"),
            ),
        ];
        for (src_fixture, post_path) in pairs {
            assert!(
                post_path.exists(),
                "expected {} to exist post-migration",
                post_path.display()
            );
            let original = std::fs::read_to_string(src_fixture).unwrap();
            let migrated = std::fs::read_to_string(post_path).unwrap();
            assert_eq!(
                original,
                migrated,
                "byte-for-byte mismatch for {}",
                post_path.display()
            );
        }

        // Active profile marker exists and resolves to "default".
        assert_eq!(ProfileManager::resolve_active_name(), "default");

        // Migration is a one-way ratchet: re-running is a no-op.
        let again = migration::maybe_migrate_legacy_layout().unwrap();
        assert!(!again);
    });
}

#[test]
fn v041_fixture_load_or_init_resolves_to_profile_path() {
    with_legacy_fixture(|home| {
        // Drive the migration via the real Config::load_or_init() entry point.
        // This is the call site that production code goes through, so we
        // verify it is profile-aware end-to-end.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let config = rt.block_on(rantaiclaw::Config::load_or_init()).unwrap();

        let expected_cfg = home.join(".rantaiclaw/profiles/default/config.toml");
        assert_eq!(config.config_path, expected_cfg);

        // The legacy file at ~/.rantaiclaw/config.toml is now a symlink
        // (Unix) or absent (Windows). Either way, content reads identical
        // to the migrated copy.
        #[cfg(unix)]
        {
            let legacy_link = home.join(".rantaiclaw/config.toml");
            let meta = std::fs::symlink_metadata(&legacy_link).unwrap();
            assert!(
                meta.file_type().is_symlink(),
                "legacy config.toml should be a symlink after migration"
            );
        }
    });
}
