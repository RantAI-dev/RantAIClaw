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
pub const CURRENT_VERSION: u32 = 15;

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

    // v8 → v9: flipped several tool/security DEFAULTS to "easy mode" (no
    // key/surface change, no transformation): `[web_search].enabled` false→true,
    // `[browser].enabled` false→true, `[http_request]` enabled with
    // `allowed_domains = ["*"]` (allow-all wildcard) + `max_response_size`
    // 1MB→5MiB + `timeout_secs` 30→20, and `[autonomy].block_high_risk_commands`
    // true→false. Configs that set these explicitly keep their values; configs
    // that omit them pick up the new defaults. This arm burns a version slot so
    // the schema_drift fingerprint (which embeds default values) is accepted with
    // intent (mirrors v6 → v7).
    if from < 9 {
        // (no transformation; default-value change only)
    }

    // v9 → v10: additive only — new optional `[knowledge]` config section
    // (embedding_api_key, vision_api_key), defaulted by serde. No data
    // transformation; this arm burns a version slot so the schema_drift
    // fingerprint (which embeds defaults + structure) is accepted.
    if from < 10 {
        // (no transformation; additive field only)
    }

    // v10 → v11: additive only — new optional `[gateway.login]` credential
    // (username, password_hash), defaulted by serde. No data transformation;
    // this arm burns a version slot so the schema_drift fingerprint is accepted.
    if from < 11 {
        // (no transformation; additive field only)
    }

    // v11 → v12: default-value change only — `[gateway].port` default 3000 →
    // 9393 (avoids the crowded 3000 that collides with other local dev servers).
    // Configs written by rantaiclaw serialize `port` explicitly and keep their
    // value; only fresh/omitting configs pick up the new default. This arm burns
    // a version slot so the schema_drift fingerprint (which embeds default values)
    // is accepted with intent (mirrors v8 → v9).
    if from < 12 {
        // (no transformation; default-value change only)
    }

    // v12 → v13: `[ui].host` (String, default "127.0.0.1") was added — the bind
    // address for the web console served by `rantaiclaw ui start`. Additive
    // field with a serde default: configs that lack it deserialise fine and gain
    // the secure default (loopback) on next write, so there is nothing to
    // transform. Burns a version slot so the schema_drift fingerprint is
    // accepted with intent (mirrors prior additive-field arms).
    if from < 13 {
        // (no transformation; additive default-only field)
    }

    // v13 → v14: `[gateway.login].idle_timeout_secs` (u64, default 0) was added
    // — auto-lock after N seconds without operator input, `0` meaning never.
    // The default is inert, so existing configs keep behaving exactly as before
    // and there is nothing to transform. Burns a version slot so the
    // schema_drift fingerprint is accepted with intent.
    if from < 14 {
        // (no transformation; additive default-only field)
    }

    // v14 → v15: `[gateway].api_rate_limit_per_minute` (u32, default 600) was
    // added — a per-client cap on `/api/v1/*`, which previously had none while
    // `/pair` and `/webhook` did. Additive field with a serde default, so
    // existing configs deserialise fine and gain the default on next write;
    // nothing to transform. Burns a version slot so the schema_drift
    // fingerprint is accepted with intent.
    if from < 15 {
        // (no transformation; additive default-only field)
    }

    // Future migrations (v16, v17, …) inserted here in order.
    // if from < 16 { migrate_v16(raw)?; }

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
    fn v10_config_migrates_to_v11_without_data_change() {
        let mut raw = parse("schema_version = 10\n[gateway]\nport = 3000\n");
        let changed = migrate(&mut raw).unwrap();
        assert!(changed);
        assert_eq!(version_of(&raw), Some(CURRENT_VERSION.into()));
    }

    #[test]
    fn v11_to_v12_is_default_only_noop_preserving_port() {
        let mut raw = parse("schema_version = 11\n[gateway]\nport = 3000\n");
        let changed = migrate(&mut raw).unwrap();
        assert!(changed, "stamps the new version");
        assert_eq!(version_of(&raw), Some(CURRENT_VERSION.into()));
        // an explicit port is preserved (not rewritten to the new default)
        assert_eq!(
            raw.get("gateway")
                .and_then(|g| g.get("port"))
                .and_then(|p| p.as_integer()),
            Some(3000)
        );
    }

    #[test]
    fn v14_to_v15_is_additive_noop() {
        let mut raw = parse("schema_version = 14\n[gateway]\nport = 9393\n");
        let changed = migrate(&mut raw).unwrap();
        assert!(changed, "stamps the new version");
        assert_eq!(version_of(&raw), Some(CURRENT_VERSION.into()));
        let gw = raw.get("gateway").expect("gateway table survives");
        assert_eq!(gw.get("port").and_then(toml::Value::as_integer), Some(9393));
        assert!(
            gw.get("api_rate_limit_per_minute").is_none(),
            "migration must not write the new key; the serde default supplies it"
        );
    }

    #[test]
    fn v13_to_v14_is_additive_noop_preserving_login_credential() {
        let mut raw = parse(
            "schema_version = 13\n[gateway.login]\nusername = \"rantaiclaw_user\"\npassword_hash = \"$argon2id$v=19$m=1,t=1,p=1$a$b\"\n",
        );
        let changed = migrate(&mut raw).unwrap();
        assert!(changed, "stamps the new version");
        assert_eq!(version_of(&raw), Some(CURRENT_VERSION.into()));
        // The stored credential survives untouched, and the new key is left
        // absent so the inert serde default (0 = never lock) applies.
        let login = raw
            .get("gateway")
            .and_then(|g| g.get("login"))
            .expect("login table survives");
        assert_eq!(
            login.get("username").and_then(|u| u.as_str()),
            Some("rantaiclaw_user")
        );
        assert!(
            login.get("idle_timeout_secs").is_none(),
            "migration must not write the new key; the serde default supplies it"
        );
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
    fn v8_to_v9_is_default_only_noop_preserving_content() {
        // v8 → v9 only flipped tool/security DEFAULTS to "easy mode" (no
        // key/surface change). A v8 config that set values explicitly migrates
        // to CURRENT_VERSION with all content intact and without the migration
        // injecting or transforming anything.
        let mut v = parse("schema_version = 8\n[autonomy]\nblock_high_risk_commands = true\n");
        let migrated = migrate(&mut v).unwrap();
        assert!(migrated, "v8 bump should be reported as transformed");
        assert_eq!(version_of(&v), Some(i64::from(CURRENT_VERSION)));
        let autonomy = v.get("autonomy").unwrap().as_table().unwrap();
        assert_eq!(
            autonomy.get("block_high_risk_commands").unwrap().as_bool(),
            Some(true),
            "default-only migration must not touch an explicit user value"
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
    fn v9_to_v10_is_additive_noop() {
        // v9 → v10 only added the optional `[knowledge]` config section
        // (additive, serde-defaulted). A v9 config migrates to v10 (current)
        // without the migration injecting a `knowledge` table.
        let mut v = parse("schema_version = 9\n");
        let changed = migrate(&mut v).unwrap();
        assert!(changed, "v9 config should be migrated to current");
        assert_eq!(version_of(&v), Some(i64::from(CURRENT_VERSION)));
        assert!(
            v.get("knowledge").is_none(),
            "migration must not inject knowledge; serde default handles it"
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
