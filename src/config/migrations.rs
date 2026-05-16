//! Versioned config-schema migrations.
//!
//! Parallel to `src/sessions/migrations.rs` which handles the SQLite
//! side. This module migrates the on-disk `config.toml` shape when a
//! field is renamed / removed / re-typed across versions, so a user
//! who runs `rantaiclaw update` from v0.6.N to v0.6.N+M never ends up
//! with a config the new binary can't deserialise.
//!
//! Each migration is an explicit function that takes a `toml::Value`
//! and transforms it. The runner walks them in order from the on-disk
//! `schema_version` up to [`CURRENT_VERSION`]. The runner is
//! idempotent — running it against an already-current config is a
//! no-op.
//!
//! ## Adding a migration
//!
//! When a future cut needs to rename `[autonomy].level` to
//! `[autonomy].mode` (for example):
//!
//! 1. Bump [`CURRENT_VERSION`] from `1` to `2`.
//! 2. Add a `migrate_v2(raw: &mut toml::Value) -> Result<()>` that
//!    performs the rename in-place.
//! 3. Append a `if from < 2 { migrate_v2(raw)?; }` arm inside
//!    [`migrate`].
//! 4. Add a unit test that feeds a pre-v2 fixture through `migrate`
//!    and asserts the post-shape.
//!
//! Keep each migration narrow — one schema change per `migrate_vN`.
//! That makes the chain easy to read top-to-bottom.

use anyhow::{Context, Result};
use toml::Value;

/// Bump when a `migrate_vN` is added. The `Config` struct's compiled
/// schema must match this version after [`migrate`] runs.
pub const CURRENT_VERSION: u32 = 1;

/// Field name stored at the top level of `config.toml` carrying the
/// schema version of the on-disk content. Absent on configs written
/// by pre-v0.6.45 binaries; treated as version `0` for the purposes
/// of running migrations.
pub const SCHEMA_VERSION_KEY: &str = "schema_version";

/// Migrate `raw` in place from whatever `schema_version` it carries
/// (or `0` if missing) up to [`CURRENT_VERSION`]. Returns `Ok(true)`
/// if any transformation ran (caller should persist the result),
/// `Ok(false)` if the config was already current (no write needed).
pub fn migrate(raw: &mut Value) -> Result<bool> {
    let from = raw
        .get(SCHEMA_VERSION_KEY)
        .and_then(|v| v.as_integer())
        .map(|i| i as u32)
        .unwrap_or(0);

    if from == CURRENT_VERSION {
        return Ok(false);
    }
    if from > CURRENT_VERSION {
        // Config was written by a newer binary than this one. Don't
        // attempt to downgrade — that's lossy in general. Surface a
        // clear error so the user knows to upgrade their binary.
        anyhow::bail!(
            "config.toml schema_version={from} is newer than this binary supports \
             (max={CURRENT_VERSION}). Update rantaiclaw before continuing."
        );
    }

    // Per-version steps. Each block is responsible for raising
    // `schema_version` to its own number at the end (or relying on
    // the final write below, since the runner stamps the value
    // unconditionally before returning).

    // v0 → v1: framework introduction. Nothing to transform — pre-
    // framework configs are structurally identical to v1 because no
    // breaking schema change has shipped yet. The version field
    // simply starts being written.
    if from < 1 {
        // (no transformation; placeholder for symmetry)
    }

    // Future migrations (v2, v3, …) inserted here in order.
    // if from < 2 { migrate_v2(raw)?; }

    set_schema_version(raw, CURRENT_VERSION).context("stamp schema_version after migration")?;
    Ok(true)
}

/// Write `version` into the root TOML table. Creates the field if
/// absent; overwrites if already present.
fn set_schema_version(raw: &mut Value, version: u32) -> Result<()> {
    let table = raw
        .as_table_mut()
        .context("config.toml root is not a table; cannot stamp schema_version")?;
    table.insert(
        SCHEMA_VERSION_KEY.to_string(),
        Value::Integer(version as i64),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Value {
        toml::from_str(toml).expect("test fixture parses")
    }

    fn version_of(v: &Value) -> Option<i64> {
        v.get(SCHEMA_VERSION_KEY)?.as_integer()
    }

    #[test]
    fn fresh_v1_config_is_noop() {
        let mut v = parse("schema_version = 1\n[other]\nfoo = \"bar\"\n");
        let migrated = migrate(&mut v).unwrap();
        assert!(
            !migrated,
            "current-version config should not be transformed"
        );
        assert_eq!(version_of(&v), Some(1));
    }

    #[test]
    fn pre_framework_config_gets_stamped_to_current() {
        // No schema_version field — represents every config.toml written
        // by a pre-v0.6.45 binary.
        let mut v = parse("[other]\nfoo = \"bar\"\n");
        assert!(version_of(&v).is_none());
        let migrated = migrate(&mut v).unwrap();
        assert!(migrated, "pre-framework config should be transformed");
        assert_eq!(version_of(&v), Some(CURRENT_VERSION as i64));
        // Existing content must survive verbatim.
        let other = v.get("other").unwrap().as_table().unwrap();
        assert_eq!(other.get("foo").unwrap().as_str(), Some("bar"));
    }

    #[test]
    fn migration_is_idempotent() {
        let mut v = parse("[other]\nfoo = \"bar\"\n");
        let first = migrate(&mut v).unwrap();
        assert!(first);
        let second = migrate(&mut v).unwrap();
        assert!(!second, "second pass must be a no-op");
        assert_eq!(version_of(&v), Some(CURRENT_VERSION as i64));
    }

    #[test]
    fn future_version_is_refused() {
        let mut v = parse("schema_version = 999\n");
        let err = migrate(&mut v).expect_err("must refuse future schema");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("newer than this binary supports"),
            "got: {msg}"
        );
    }

    #[test]
    fn non_table_root_is_an_error() {
        // toml::Value can hold scalars at the root via array wrappers in
        // some weird cases; ensure we don't silently corrupt them.
        let mut v = Value::Integer(7);
        assert!(migrate(&mut v).is_err());
    }
}
