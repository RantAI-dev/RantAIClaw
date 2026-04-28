//! Integration tests for the v0.4.x → v0.5.0 legacy-layout migration.
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md` §7.1.
//!
//! As with `profile_lifecycle.rs`, every test redirects `$HOME` to a tempdir
//! and the suite serializes via a global `Mutex` so concurrent tests don't
//! race on `std::env::set_var("HOME", ...)`.

use std::path::Path;
use std::sync::Mutex;

use rantaiclaw::profile::{migration, paths, ProfileManager};
use tempfile::TempDir;

static HOME_LOCK: Mutex<()> = Mutex::new(());

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

/// Build a synthetic flat `~/.rantaiclaw/` matching the v0.4.x layout.
fn create_legacy_fixture(home: &Path) {
    let root = home.join(".rantaiclaw");
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    std::fs::create_dir_all(root.join("memory")).unwrap();
    std::fs::create_dir_all(root.join("skills/example")).unwrap();
    std::fs::write(
        root.join("config.toml"),
        "default_provider = \"openrouter\"\n",
    )
    .unwrap();
    std::fs::write(root.join("workspace/AGENTS.md"), "# agents\n").unwrap();
    std::fs::write(root.join("memory/MEMORY.md"), "# memories\n").unwrap();
    std::fs::write(root.join("skills/example/SKILL.md"), "# skill\n").unwrap();
}

#[test]
fn migration_moves_files_into_default_profile() {
    with_home(|home| {
        create_legacy_fixture(home);

        let did = migration::maybe_migrate_legacy_layout().unwrap();
        assert!(did, "migration should have fired");

        let default = paths::profile_dir("default");
        assert!(default.join("config.toml").exists());
        assert!(default.join("workspace/AGENTS.md").exists());
        assert!(default.join("memory/MEMORY.md").exists());
        assert!(default.join("skills/example/SKILL.md").exists());
    });
}

#[test]
fn migration_creates_active_profile_file_set_to_default() {
    with_home(|home| {
        create_legacy_fixture(home);
        let _ = migration::maybe_migrate_legacy_layout().unwrap();

        let marker = paths::active_profile_file();
        assert!(marker.exists());
        let contents = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(contents.trim(), "default");
    });
}

#[test]
fn migration_writes_version_and_notice() {
    with_home(|home| {
        create_legacy_fixture(home);
        let _ = migration::maybe_migrate_legacy_layout().unwrap();

        let version = paths::version_file();
        assert!(version.exists());
        let v = std::fs::read_to_string(&version).unwrap();
        assert_eq!(v.trim(), env!("CARGO_PKG_VERSION"));

        let notice = home.join(".rantaiclaw/MIGRATION_NOTICE.md");
        assert!(notice.exists());
        let body = std::fs::read_to_string(&notice).unwrap();
        assert!(body.contains("profile-aware"));
        assert!(body.contains("Storage layout migrated"));
    });
}

#[cfg(unix)]
#[test]
fn migration_creates_transitional_symlinks() {
    with_home(|home| {
        create_legacy_fixture(home);
        let _ = migration::maybe_migrate_legacy_layout().unwrap();

        let cfg_link = home.join(".rantaiclaw/config.toml");
        let ws_link = home.join(".rantaiclaw/workspace");
        // Both should exist as symlinks pointing into profiles/default/
        assert!(cfg_link.exists(), "config.toml symlink missing");
        assert!(ws_link.exists(), "workspace symlink missing");
        assert!(
            std::fs::symlink_metadata(&cfg_link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "config.toml not a symlink"
        );
        let target = std::fs::read_link(&cfg_link).unwrap();
        assert!(
            target
                .to_string_lossy()
                .ends_with("profiles/default/config.toml"),
            "unexpected symlink target: {}",
            target.display()
        );
    });
}

#[test]
fn migration_is_idempotent_when_no_legacy_state() {
    with_home(|_home| {
        // Fresh layout: profiles/default already created by ensure_default.
        let _ = ProfileManager::ensure_default().unwrap();
        let did = migration::maybe_migrate_legacy_layout().unwrap();
        assert!(!did, "no migration should fire on a fresh install");
    });
}

#[test]
fn migration_skipped_when_already_migrated() {
    with_home(|home| {
        create_legacy_fixture(home);
        // First run: actually migrates.
        let did1 = migration::maybe_migrate_legacy_layout().unwrap();
        assert!(did1);
        // Second run: detect predicate flips false because active_profile
        // exists, so no work happens.
        let did2 = migration::maybe_migrate_legacy_layout().unwrap();
        assert!(!did2);
    });
}

#[test]
fn migration_lock_prevents_concurrent_runs() {
    use fs2::FileExt;
    use std::fs::OpenOptions;

    with_home(|home| {
        create_legacy_fixture(home);

        // Prepare lock file path (rantaiclaw root must exist).
        let root = home.join(".rantaiclaw");
        std::fs::create_dir_all(&root).unwrap();
        let lock_path = paths::migration_lock_file();
        let lf = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        FileExt::lock_exclusive(&lf).unwrap();

        // Now try to migrate while the lock is held by us. The migration
        // call must give up silently and report did=false.
        let did = migration::maybe_migrate_legacy_layout().unwrap();
        assert!(!did, "migration must not run when lock is held elsewhere");

        // Sanity: legacy state is still there because the migration was skipped.
        assert!(home.join(".rantaiclaw/config.toml").exists());
        assert!(!paths::active_profile_file().exists());

        // Release and confirm migration now succeeds.
        FileExt::unlock(&lf).unwrap();
        drop(lf);
        let did2 = migration::maybe_migrate_legacy_layout().unwrap();
        assert!(did2);
    });
}

#[test]
fn needs_migration_predicate() {
    with_home(|home| {
        // Empty home: no migration.
        assert!(!migration::needs_migration());
        create_legacy_fixture(home);
        assert!(migration::needs_migration());
        let _ = migration::maybe_migrate_legacy_layout().unwrap();
        assert!(!migration::needs_migration());
    });
}

/// Regression: `.secret_key` must move with `config.toml` so api_keys
/// encrypted in the v0.4.x install can still be decrypted post-migration.
///
/// Found by KubeVirt smoke test on Ubuntu 22.04: legacy `onboard
/// --api-key …` produced encrypted `config.toml` + `.secret_key` at the
/// flat root. After migration only `config.toml` moved, leaving the
/// loader to spawn a fresh `.secret_key` in the profile dir → next load
/// failed with `Decryption failed — wrong key or tampered data`.
#[test]
fn migration_preserves_secret_key_so_encrypted_api_key_still_decrypts() {
    use rantaiclaw::security::SecretStore;
    with_home(|home| {
        let root = home.join(".rantaiclaw");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("config.toml"),
            "default_provider = \"openrouter\"\n",
        )
        .unwrap();

        // Pre-encrypt a secret with a SecretStore rooted at the legacy
        // path — this writes `.secret_key` next to the legacy config.
        let pre_store = SecretStore::new(&root, true);
        let ciphertext = pre_store.encrypt("sk-test-legacy-1234").unwrap();
        assert!(SecretStore::is_encrypted(&ciphertext));
        assert!(root.join(".secret_key").exists());

        let did = migration::maybe_migrate_legacy_layout().unwrap();
        assert!(did, "migration should fire on this fixture");

        // Post-migration: the loader uses `config.toml.parent()` to build
        // the SecretStore, which now points at the profile dir. That
        // SecretStore must decrypt the same ciphertext — meaning the
        // legacy `.secret_key` was carried into the profile dir.
        let post_root = paths::profile_dir("default");
        let post_store = SecretStore::new(&post_root, true);
        let recovered = post_store
            .decrypt(&ciphertext)
            .expect("api_key must still decrypt after migration");
        assert_eq!(recovered, "sk-test-legacy-1234");

        // The legacy `.secret_key` should no longer sit at root — moved,
        // not copied — to avoid confusion / future double-keys.
        assert!(
            !root.join(".secret_key").is_file(),
            ".secret_key must be moved out of legacy root, not duplicated",
        );
    });
}

/// Defensive: a legacy `secrets/` directory (used by some out-of-band
/// onboard variants for token caches) should follow the config into the
/// profile dir.
#[test]
fn migration_carries_secrets_directory() {
    with_home(|home| {
        create_legacy_fixture(home);
        let root = home.join(".rantaiclaw");
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::write(root.join("secrets/api_keys.toml"), "openrouter = \"x\"\n").unwrap();

        let did = migration::maybe_migrate_legacy_layout().unwrap();
        assert!(did);

        let dest = paths::secrets_dir("default").join("api_keys.toml");
        assert!(dest.exists(), "secrets/api_keys.toml should move into profile dir");
        assert!(
            !root.join("secrets").is_dir(),
            "legacy secrets/ should be gone after migration",
        );
    });
}
