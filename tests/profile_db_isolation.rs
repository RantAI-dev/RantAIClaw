//! Per-profile isolation of `sessions.db` and `kb.db`.
//!
//! Before this fix every profile shared one global XDG `sessions.db`
//! (`~/.local/share/rantaiclaw/sessions.db`) and `kb.db`, so `--profile work`
//! saw `--profile personal`'s chat history and knowledge base. These tests pin
//! the isolation: each profile resolves its own
//! `profiles/<name>/sessions/sessions.db` and `profiles/<name>/kb.db`, and the
//! one-shot migrations move the legacy global files into `default`.
//!
//! As with `migrate_legacy.rs`, `$HOME` is redirected to a tempdir and the
//! suite serializes on a global `Mutex` so concurrent tests don't race on
//! `std::env::set_var`. We also neutralize `XDG_*` overrides so the
//! `directories`-derived global data dir stays inside the tempdir.

use std::path::Path;
use std::sync::Mutex;

use rantaiclaw::profile::{migration, paths, ProfileManager};
use rantaiclaw::sessions::SessionStore;
use tempfile::TempDir;

static HOME_LOCK: Mutex<()> = Mutex::new(());

fn with_home<F: FnOnce(&Path)>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().expect("tempdir");

    let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
        "HOME",
        "RANTAICLAW_PROFILE",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
    ]
    .iter()
    .map(|k| (*k, std::env::var_os(k)))
    .collect();

    std::env::set_var("HOME", tmp.path());
    std::env::remove_var("RANTAICLAW_PROFILE");
    // Force the `directories` global data dir back onto HOME so the legacy
    // global sessions.db lands inside the tempdir, not the developer's ~.
    std::env::remove_var("XDG_DATA_HOME");
    std::env::remove_var("XDG_CONFIG_HOME");

    let home = tmp.path().to_path_buf();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&home)));

    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// The legacy global data dir the `directories` crate resolves to under the
/// redirected HOME (Linux: `$HOME/.local/share/rantaiclaw`).
fn global_data_dir(home: &Path) -> std::path::PathBuf {
    home.join(".local/share/rantaiclaw")
}

/// Headline guard: two profiles must not share session history. Exercises the
/// real derive site (`sessions::cli::open_store` → `ProfileManager::active`),
/// so a revert to the old global path fails this test.
#[test]
fn sessions_are_isolated_per_profile() {
    with_home(|_home| {
        std::env::set_var("RANTAICLAW_PROFILE", "work");
        let work_store = rantaiclaw::sessions::cli::open_store().expect("open work store");
        work_store
            .new_session("test:model", "test")
            .expect("create work session");
        let work_path = ProfileManager::active().unwrap().sessions_db_path();
        drop(work_store);

        std::env::set_var("RANTAICLAW_PROFILE", "personal");
        let personal_store = rantaiclaw::sessions::cli::open_store().expect("open personal store");
        let personal_path = ProfileManager::active().unwrap().sessions_db_path();

        assert_ne!(
            work_path, personal_path,
            "each profile must resolve its own sessions.db"
        );
        assert_eq!(
            personal_store.list_sessions(10).unwrap().len(),
            0,
            "personal profile must not see the work profile's session"
        );
        assert_eq!(
            SessionStore::open(&work_path)
                .unwrap()
                .list_sessions(10)
                .unwrap()
                .len(),
            1,
            "work profile keeps its own session",
        );
    });
}

/// The one-shot migration moves the global sessions.db into `default` and
/// leaves the source gone.
#[test]
fn global_sessions_db_moves_into_default_profile() {
    with_home(|home| {
        let global = global_data_dir(home);
        std::fs::create_dir_all(&global).unwrap();
        let legacy = global.join("sessions.db");
        {
            let store = SessionStore::open(&legacy).unwrap();
            store.new_session("test:model", "legacy").unwrap();
        }

        let did = migration::maybe_migrate_global_sessions_db().unwrap();
        assert!(
            did,
            "migration should fire when a global sessions.db exists"
        );

        let dest = paths::sessions_db("default");
        assert!(dest.exists(), "sessions.db should now live under default");
        assert_eq!(
            SessionStore::open(&dest)
                .unwrap()
                .list_sessions(10)
                .unwrap()
                .len(),
            1,
            "migrated db retains its session",
        );
        assert!(
            !legacy.exists(),
            "legacy global sessions.db must be moved, not copied"
        );
    });
}

/// Migration is idempotent and never clobbers a populated per-profile db.
#[test]
fn migration_is_idempotent_and_never_clobbers() {
    with_home(|home| {
        // No global db → no-op.
        assert!(!migration::maybe_migrate_global_sessions_db().unwrap());

        // A populated default db plus a leftover global db: the global must
        // NOT overwrite the profile's data.
        let dest = paths::sessions_db("default");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        {
            let store = SessionStore::open(&dest).unwrap();
            store.new_session("keep:model", "profile").unwrap();
        }
        let global = global_data_dir(home);
        std::fs::create_dir_all(&global).unwrap();
        {
            let store = SessionStore::open(&global.join("sessions.db")).unwrap();
            store.new_session("stale:model", "global").unwrap();
        }

        let did = migration::maybe_migrate_global_sessions_db().unwrap();
        assert!(!did, "must not move when destination already has data");
        assert_eq!(
            SessionStore::open(&dest)
                .unwrap()
                .list_sessions(10)
                .unwrap()[0]
                .model,
            "keep:model",
            "profile db must be preserved untouched",
        );
    });
}

// ── kb.db ──────────────────────────────────────────────────────────────────
//
// The db-migration machinery (WAL checkpoint → move → no-clobber → idempotent)
// is shared with sessions and fully exercised by the tests above. kb's own
// migration operates at the file level and is schema-agnostic, so we seed the
// fixtures with a `SessionStore` purely because it is the handiest way to
// produce a valid WAL-mode SQLite file in-test — the row count, not the
// schema, is what these assertions track.

/// kb.db resolves per-profile, so two profiles never share a corpus. Mirrors
/// what `resolve_kb_db_path` computes for its profile branch
/// (`resolve_active_name` → `paths::kb_db`).
#[test]
fn kb_dbs_are_isolated_per_profile() {
    with_home(|_home| {
        assert_ne!(
            paths::kb_db("work"),
            paths::kb_db("personal"),
            "each profile must resolve its own kb.db"
        );

        std::env::set_var("RANTAICLAW_PROFILE", "work");
        let active = paths::kb_db(&ProfileManager::resolve_active_name());
        assert!(
            active.ends_with("profiles/work/kb.db"),
            "active profile kb.db should live under profiles/work/: {}",
            active.display()
        );
        assert_eq!(
            active,
            ProfileManager::ensure("work").unwrap().kb_db_path(),
            "Profile::kb_db_path must match the resolver's path"
        );
    });
}

/// The one-shot migration moves the global kb.db into `default`, preserving
/// its data through the WAL checkpoint, and leaves the source gone.
#[test]
fn global_kb_db_moves_into_default_profile() {
    with_home(|home| {
        let global = global_data_dir(home);
        std::fs::create_dir_all(&global).unwrap();
        let legacy = global.join("kb.db");
        SessionStore::open(&legacy)
            .unwrap()
            .new_session("kb:fixture", "legacy")
            .unwrap();

        let did = migration::maybe_migrate_global_kb_db().unwrap();
        assert!(did, "migration should fire when a global kb.db exists");

        let dest = paths::kb_db("default");
        assert!(dest.exists(), "kb.db should now live under default");
        assert_eq!(
            SessionStore::open(&dest)
                .unwrap()
                .list_sessions(10)
                .unwrap()
                .len(),
            1,
            "migrated kb.db retains its data through the WAL checkpoint",
        );
        assert!(
            !legacy.exists(),
            "legacy global kb.db must be moved, not copied"
        );
    });
}

/// kb migration is idempotent and never clobbers a populated per-profile db.
#[test]
fn kb_migration_is_idempotent_and_never_clobbers() {
    with_home(|home| {
        // No global db → no-op.
        assert!(!migration::maybe_migrate_global_kb_db().unwrap());

        let dest = paths::kb_db("default");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        SessionStore::open(&dest)
            .unwrap()
            .new_session("keep:fixture", "profile")
            .unwrap();

        let global = global_data_dir(home);
        std::fs::create_dir_all(&global).unwrap();
        SessionStore::open(&global.join("kb.db"))
            .unwrap()
            .new_session("stale:fixture", "global")
            .unwrap();

        let did = migration::maybe_migrate_global_kb_db().unwrap();
        assert!(!did, "must not move when destination already has data");
        assert_eq!(
            SessionStore::open(&dest)
                .unwrap()
                .list_sessions(10)
                .unwrap()[0]
                .model,
            "keep:fixture",
            "profile kb.db must be preserved untouched",
        );
    });
}
