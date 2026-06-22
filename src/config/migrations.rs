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
pub const CURRENT_VERSION: u32 = 8;

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

    // v1 → v2: the default for `[agent].max_tool_iterations` changed
    // from 10 to 25. Configs that have an EXPLICIT 10 keep their
    // explicit value (treated as a user choice, not the old default).
    // Configs that lack the field gain it on first write through the
    // serde default — which is now 25 — without us touching anything
    // here. No structural migration needed; this arm exists only to
    // burn a version slot so the schema_drift fingerprint can be
    // accepted with intent.
    if from < 2 {
        // (no transformation; default-only change)
    }

    // v2 → v3: `[channels_config].autonomous_tools` (bool, default false)
    // was added — opt-in to running tools unattended over gateway channel
    // webhooks. Additive field with a serde default: configs that lack it
    // deserialise fine and gain the default on next write, so there is
    // nothing to transform. This arm exists only to burn a version slot so
    // the schema_drift fingerprint can be accepted with intent (mirrors
    // v1 → v2).
    if from < 3 {
        // (no transformation; additive default-only field)
    }

    // v3 → v4: `[channels_config].approval_owners` (Vec<String>, default empty)
    // was added — the owner-authority allowlist for in-chat / in-browser tool
    // approval (unified-agent-runtime). Additive field with a serde default:
    // configs that lack it deserialise fine and gain the default (`[]`,
    // secure — nobody can approve) on next write, so there is nothing to
    // transform. This arm exists only to burn a version slot so the
    // schema_drift fingerprint can be accepted with intent (mirrors v2 → v3).
    if from < 4 {
        // (no transformation; additive default-only field)
    }

    // v4 → v5: `[channels_config].guest_allowed_tools` + `guest_allowed_commands`
    // (Vec<String>, default empty) were added — the per-role capability ceiling
    // for non-owner ("normal") users. Additive fields with serde defaults:
    // configs that lack them deserialise fine and gain the defaults (`[]`,
    // secure — guests get only read-only tools + skills) on next write, so there
    // is nothing to transform. Burns a version slot so schema_drift is accepted
    // with intent (mirrors v3 → v4).
    if from < 5 {
        // (no transformation; additive default-only fields)
    }

    // v5 → v6: `provider_api_keys` (HashMap<String, String>, default empty) was
    // added — a per-provider API key store so switching the active provider in
    // the console no longer reuses another provider's credential. Additive with
    // a serde default: configs that lack it deserialise fine and gain the
    // default (`{}`) on next write, so there is nothing to transform. This arm
    // burns a version slot so the schema_drift fingerprint is accepted with
    // intent (mirrors v3 → v4 / v4 → v5).
    if from < 6 {
        // (no transformation; additive default-only field)
    }

    // v6 → v7: raised several limit DEFAULTS (no key/surface change, no
    // transformation): `[autonomy].max_actions_per_hour` 20→200,
    // `[agent].max_tool_iterations` 25→50,
    // `[channels_config].message_timeout_secs` 300→600,
    // `[reliability].provider_retries` 2→3. Configs that set these explicitly
    // keep their values; configs that omit them pick up the new defaults. This
    // arm burns a version slot so the schema_drift fingerprint (which embeds
    // default values) is accepted with intent.
    if from < 7 {
        // (no transformation; default-value change only)
    }

    // v7 → v8: documentation-only schema changes. Corrected doc comments that
    // the JSON-schema fingerprint embeds: `[autonomy].level` `read_only`→
    // `readonly` (the value that errors if mistyped), the stale `Default:`
    // annotations on `[agent].max_tool_iterations` (`10`→`25`) and
    // `[autonomy].max_actions_per_hour` (`100`→`200`), and a note that
    // `max_cost_per_day_cents` is tracked but not enforced. No key, surface, or
    // default-value change → nothing to transform. This arm burns a version slot
    // so the schema_drift fingerprint (which embeds doc strings) is accepted with
    // intent (mirrors v6 → v7).
    if from < 8 {
        // (no transformation; documentation-only fingerprint change)
    }

    // Future migrations (v9, v10, …) inserted here in order.
    // if from < 9 { migrate_v9(raw)?; }

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
    fn fresh_current_version_config_is_noop() {
        let mut v = parse(&format!(
            "schema_version = {CURRENT_VERSION}\n[other]\nfoo = \"bar\"\n"
        ));
        let migrated = migrate(&mut v).unwrap();
        assert!(
            !migrated,
            "current-version config should not be transformed"
        );
        assert_eq!(version_of(&v), Some(CURRENT_VERSION as i64));
    }

    #[test]
    fn v1_preserves_explicit_max_tool_iterations_through_current() {
        // A user who had set max_tool_iterations = 10 explicitly in
        // their v1 config keeps that exact value through the migration
        // chain. The v1 → v2 default change (10 → 25) doesn't override
        // their choice, and the later bumps are additive no-ops.
        let mut v = parse("schema_version = 1\n[agent]\nmax_tool_iterations = 10\n");
        let migrated = migrate(&mut v).unwrap();
        assert!(migrated, "v1 config should be reported as transformed");
        assert_eq!(version_of(&v), Some(i64::from(CURRENT_VERSION)));
        let agent = v.get("agent").unwrap().as_table().unwrap();
        assert_eq!(
            agent.get("max_tool_iterations").unwrap().as_integer(),
            Some(10),
            "explicit 10 must survive migration (user choice, not default)"
        );
    }

    #[test]
    fn v2_to_v3_is_additive_noop_preserving_content() {
        // v2 → v3 only added `[channels_config].autonomous_tools` (additive,
        // default false). A v2 config migrates to v3 with all content intact
        // and without the migration injecting autonomous_tools.
        let mut v = parse("schema_version = 2\n[channels_config]\ncli = true\n");
        let migrated = migrate(&mut v).unwrap();
        assert!(migrated, "v2 bump should be reported as transformed");
        // migrate() always stamps to CURRENT_VERSION (the chain can't stop at an
        // intermediate version); the v2→v3 step is the additive autonomous_tools
        // field, which must not be injected by the migration.
        assert_eq!(version_of(&v), Some(CURRENT_VERSION as i64));
        let cc = v.get("channels_config").unwrap().as_table().unwrap();
        assert_eq!(cc.get("cli").unwrap().as_bool(), Some(true));
        assert!(
            cc.get("autonomous_tools").is_none(),
            "migration must not inject autonomous_tools; serde default handles it"
        );
    }

    #[test]
    fn v7_to_v8_is_doc_only_noop_preserving_content() {
        // v7 → v8 only corrected doc comments embedded in the JSON-schema
        // fingerprint (no key/surface/default-value change). A v7 config
        // migrates to v8 (CURRENT_VERSION) with all content intact and without
        // the migration injecting or transforming anything.
        let mut v = parse("schema_version = 7\n[autonomy]\nlevel = \"full\"\n");
        let migrated = migrate(&mut v).unwrap();
        assert!(migrated, "v7 bump should be reported as transformed");
        assert_eq!(version_of(&v), Some(CURRENT_VERSION as i64));
        let autonomy = v.get("autonomy").unwrap().as_table().unwrap();
        assert_eq!(
            autonomy.get("level").unwrap().as_str(),
            Some("full"),
            "doc-only migration must not touch config content"
        );
    }

    #[test]
    fn v3_to_v4_is_additive_noop_preserving_content() {
        // v3 → v4 only added `[channels_config].approval_owners` (additive,
        // default empty). A v3 config migrates to v4 with all content intact
        // and without the migration injecting approval_owners.
        let mut v = parse("schema_version = 3\n[channels_config]\ncli = true\n");
        let migrated = migrate(&mut v).unwrap();
        assert!(migrated, "v3 bump should be reported as transformed");
        assert_eq!(version_of(&v), Some(CURRENT_VERSION as i64));
        let cc = v.get("channels_config").unwrap().as_table().unwrap();
        assert_eq!(cc.get("cli").unwrap().as_bool(), Some(true));
        assert!(
            cc.get("approval_owners").is_none(),
            "migration must not inject approval_owners; serde default handles it"
        );
    }

    #[test]
    fn v4_to_v5_is_additive_noop_preserving_content() {
        // v4 → v5 only added `[channels_config].guest_allowed_tools` +
        // `guest_allowed_commands` (additive, default empty). A v4 config
        // migrates to current with content intact and without the migration
        // injecting the guest fields.
        let mut v = parse("schema_version = 4\n[channels_config]\ncli = true\n");
        let migrated = migrate(&mut v).unwrap();
        assert!(migrated, "v4 bump should be reported as transformed");
        assert_eq!(version_of(&v), Some(i64::from(CURRENT_VERSION)));
        let cc = v.get("channels_config").unwrap().as_table().unwrap();
        assert_eq!(cc.get("cli").unwrap().as_bool(), Some(true));
        assert!(
            cc.get("guest_allowed_tools").is_none() && cc.get("guest_allowed_commands").is_none(),
            "migration must not inject guest fields; serde defaults handle them"
        );
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
