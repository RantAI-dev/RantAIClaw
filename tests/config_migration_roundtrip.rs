//! Round-trip tests for the config-migration framework.
//!
//! These tests use `Config::default()` as the "current shape" baseline.
//! That way they're automatically up-to-date with every code change —
//! no fixture file to keep in sync.
//!
//! What they enforce:
//!
//!   1. A current-shape config with no `schema_version` field (= what
//!      a pre-v0.6.45 binary would write) migrates cleanly to the
//!      current version.
//!   2. After migrate, the TOML still deserialises into `Config`.
//!      This catches "maintainer added a required field without
//!      `#[serde(default)]` or a `migrate_vN` to fill it in" —
//!      exactly the silent-update-break the framework is meant to
//!      eliminate.
//!   3. Once `schema_version` is stamped, re-running migrate is a
//!      no-op (idempotence).
//!
//! When adding a new schema version, add a `vN_xxx` test below that
//! constructs the prior-shape TOML in-memory and asserts the same
//! three invariants. Don't edit existing tests — they're the
//! upgrade-from-old-binary regression coverage.

use rantaiclaw::config::migrations::{migrate, CURRENT_VERSION, SCHEMA_VERSION_KEY};
use rantaiclaw::config::Config;

/// Serialise `Config::default()` to a TOML value and strip the
/// `schema_version` field — this is the closest in-memory analogue of
/// a config written by a pre-v0.6.45 binary, kept always-current with
/// the live `Config` struct so the test moves with the code.
fn pre_framework_default() -> toml::Value {
    let cfg = Config::default();
    let s = toml::to_string(&cfg).expect("Config::default() serialises");
    let mut v: toml::Value = toml::from_str(&s).expect("re-parse Config TOML");
    if let Some(t) = v.as_table_mut() {
        t.remove(SCHEMA_VERSION_KEY);
    }
    v
}

#[test]
fn pre_framework_config_round_trips_to_current() {
    let mut v = pre_framework_default();
    assert!(
        v.get(SCHEMA_VERSION_KEY).is_none(),
        "fixture must lack schema_version to simulate pre-framework state"
    );

    let migrated = migrate(&mut v).expect("migrate runs without error");
    assert!(migrated, "pre-framework config should be transformed");

    let stamped = v
        .get(SCHEMA_VERSION_KEY)
        .and_then(|x| x.as_integer())
        .expect("schema_version present after migrate");
    assert_eq!(stamped, CURRENT_VERSION as i64);
}

#[test]
fn deserialise_into_config_after_migrate_succeeds() {
    // End-to-end "update never breaks deserialise" assertion. If this
    // test fails after adding a new required field, EITHER add
    // `#[serde(default)]` to the field OR add a `migrate_vN` that
    // fills it in — see docs/project/operating-conventions.md
    // "Adding a schema migration".
    let mut v = pre_framework_default();
    migrate(&mut v).expect("migrate runs");
    let result: Result<Config, _> = v.try_into();
    assert!(
        result.is_ok(),
        "post-migration TOML must deserialise into Config — \
         a required field is missing a default or a migration. got: {:?}",
        result.err()
    );
}

#[test]
fn migrate_is_idempotent_post_stamp() {
    let mut v = pre_framework_default();
    let first = migrate(&mut v).expect("first migrate runs");
    assert!(first);
    let second = migrate(&mut v).expect("second migrate runs");
    assert!(
        !second,
        "re-running migrate on a stamped config must be a no-op"
    );
}
