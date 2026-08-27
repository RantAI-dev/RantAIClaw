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
pub const CURRENT_VERSION: u32 = 25;

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
    // Parse `schema_version` strictly. `.as_integer().unwrap_or(0)` silently
    // treated a malformed value (a string `"23"`, a float `23.0`) as version 0
    // and re-ran the ENTIRE chain, rewriting the file on every load; a negative
    // integer wrapped through `as u32` into a bogus "newer than this binary"
    // error. A present-but-malformed value is a corrupt config — say so; a huge
    // integer falls through to the too-new guard below.
    let from = match raw.get(SCHEMA_VERSION_KEY) {
        None => 0,
        Some(Value::Integer(i)) if *i < 0 => anyhow::bail!(
            "config.toml {SCHEMA_VERSION_KEY}={i} is negative; expected an integer 0..={CURRENT_VERSION}. \
             Fix or remove the line and re-run."
        ),
        Some(Value::Integer(i)) => u32::try_from(*i).unwrap_or(CURRENT_VERSION + 1),
        Some(other) => anyhow::bail!(
            "config.toml {SCHEMA_VERSION_KEY} is malformed ({}); expected an integer 0..={CURRENT_VERSION}. \
             Fix or remove the line and re-run.",
            other.type_str()
        ),
    };

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

    // v15 → v16: remote-skill trust boundary (plan 045). `[skills].open_skills_ref`
    // (Option<String>, default unset) was added — pins the community
    // open-skills repo to a specific commit/tag instead of auto-advancing on
    // every periodic `git pull --ff-only`. Two fingerprinted DEFAULT changes
    // ride along: remote-origin skills (open-skills, ClawHub) now default to
    // compact prompt injection instead of verbatim `Full` injection
    // (SECURITY-01 — a remote body is no longer treated as an authoritative
    // instruction), and `source = "literal"` skill API keys are now routed
    // through the secret store on save/load like every other credential
    // (DX-01) instead of round-tripping as plaintext. None of this is a
    // structural rename/re-type: the new key is additive with a serde
    // default, and the literal-key encryption happens transparently on the
    // next `save()`. Nothing to transform here; this arm burns a version
    // slot so the schema_drift fingerprint (which embeds both the new field
    // and the changed injection default) is accepted with intent.
    if from < 16 {
        // (no transformation; additive field + default/injection-behavior change)
    }

    // v16 → v17: the response cache was removed. Its three `[memory]` keys
    // configured a module nothing ever constructed, so they promised behaviour
    // that never existed. Strip them rather than leave dead knobs behind.
    if from < 17 {
        migrate_v17(raw);
    }

    // v17 → v18: `[knowledge] enabled` gates the Knowledge Base explicitly.
    // Existing installs that already carry an embedding key were configured
    // deliberately, so they upgrade ON. A config with no key upgrades OFF,
    // which matches what it already did.
    if from < 18 {
        migrate_v18(raw);
    }

    // v18 → v19: `[channels.email] require_authenticated_sender` (bool,
    // default `false`) was added. It refuses mail whose `From:` is not backed
    // by a passing SPF/DKIM/DMARC result. Additive with a serde default, and
    // the default is the *old* behaviour, so an existing mailbox keeps working
    // exactly as before; nothing to transform. Independently of the flag, mail
    // claiming an `approval_owners` address is now refused when it did not
    // authenticate — that tightening is unconditional and carries no key, so it
    // cannot be expressed as a transformation either. This arm burns a version
    // slot so the schema_drift fingerprint is accepted with intent.
    if from < 19 {
        // (no transformation; additive field whose default preserves behaviour)
    }

    // v19 → v20: `[channels.irc] allow_insecure_tls_with_password` (bool,
    // default `false`) was added. It is the second, explicit opt-in for
    // `verify_tls = false` alongside a configured password — the channel now
    // refuses to start on that combination, because SASL PLAIN is reversible
    // base64 and NickServ IDENTIFY is plaintext, so an unauthenticated link
    // hands the credential to whoever answered it. The default leaves the
    // refusal in force, which is the point; an operator who wants the old
    // behaviour sets the key. Additive with a serde default, nothing to
    // transform — this arm burns a version slot so the schema_drift
    // fingerprint is accepted with intent.
    if from < 20 {
        // (no transformation; additive field, refusal is the new default)
    }

    // v20 → v21: `[channels_config.webhook] port` was REMOVED. Nothing ever
    // read it — the gateway binds its own listener — so an operator who set it
    // was told to open a firewall port nothing listens on, and the callback
    // never arrived. A leftover `port` in an existing config is ignored by
    // serde rather than rejected, so there is nothing to transform; the key can
    // be deleted by hand at leisure. This arm burns a version slot so the
    // schema_drift fingerprint is accepted with intent.
    if from < 21 {
        // (no transformation; a removed key that no code read)
    }

    // v21 → v22: `[channels_config] thread_replies` (bool, default `true`) was
    // added — the shared switch for replying in-thread, with the existing
    // `[channels_config.mattermost] thread_replies` still overriding it. The
    // default preserves Mattermost's behaviour and turns threading on for the
    // channels that just gained it, which is the intended change. Additive with
    // a serde default, nothing to transform — this arm burns a version slot so
    // the schema_drift fingerprint is accepted with intent.
    if from < 22 {
        // (no transformation; additive field whose default is the new behaviour)
    }

    // v22 → v23: `[cron] max_catchup_age_secs` (u64, default 86400) was added — a
    // staleness gate that skips-and-re-anchors a missed scheduled run older than
    // the window instead of firing it late on restart. Additive with a serde
    // default, nothing to transform — this arm burns a version slot so the
    // schema_drift fingerprint is accepted with intent.
    if from < 23 {
        // (no transformation; additive default-only field)
    }

    // v23 → v24: aligned the serde `#[serde(default = ...)]` defaults with the
    // `impl Default` values the v8→v9 migration already declared as the intended
    // contract — the serde side had never been updated, so a config that OMITTED
    // a key loaded the STALE value (`http_request.allowed_domains = []`, which
    // rejects every request; `web_search`/`browser` disabled; `block_high_risk_
    // commands = true`; `cost.prices = {}`). Also added serde defaults to several
    // previously-required fields (`[autonomy]`, `default_temperature`,
    // `[channels_config].cli`, `[memory]`, `[heartbeat]`, `[observability]`) so a
    // PARTIAL section no longer fails the whole load with "missing field".
    // Configs that set these explicitly keep their values; omitting configs pick
    // up the corrected defaults. No data transformation — this arm burns a
    // version slot so the schema_drift fingerprint (which embeds default values)
    // is accepted with intent (mirrors v8→v9).
    if from < 24 {
        // (no transformation; serde-default alignment only)
    }

    // v24 → v25: drop dead config keys that serialized into every config.toml
    // and `config schema` output but that nothing read — `cost.allow_override`,
    // `cost.prices` (+ its bundled pricing table), and `agent.parallel_tools`.
    // Leaving them advertised a functionality the binary does not implement.
    // serde already ignores unknown keys at load, so this only tidies the file
    // in place; the fields keep any explicit value nowhere else, so nothing is
    // lost.
    if from < 25 {
        migrate_v25(raw);
    }

    // Future migrations (v26, …) inserted here in order.

    set_schema_version(raw, CURRENT_VERSION).context("stamp schema_version after migration")?;
    Ok(true)
}

/// v24 → v25: remove the dead `cost.allow_override` / `cost.prices` /
/// `agent.parallel_tools` keys from an existing config.
fn migrate_v25(raw: &mut Value) {
    let Some(root) = raw.as_table_mut() else {
        return;
    };
    if let Some(cost) = root.get_mut("cost").and_then(Value::as_table_mut) {
        cost.remove("allow_override");
        cost.remove("prices");
    }
    if let Some(agent) = root.get_mut("agent").and_then(Value::as_table_mut) {
        agent.remove("parallel_tools");
    }
}

/// v16 → v17: drop the response-cache keys.
///
/// `[memory] response_cache_enabled`, `response_cache_ttl_minutes` and
/// `response_cache_max_entries` configured a cache that was never wired to
/// anything — setting them changed nothing. Leaving them in a config would keep
/// advertising a feature that does not exist.
fn migrate_v17(raw: &mut Value) {
    const REMOVED: [&str; 3] = [
        "response_cache_enabled",
        "response_cache_ttl_minutes",
        "response_cache_max_entries",
    ];

    let Some(memory) = raw
        .as_table_mut()
        .and_then(|t| t.get_mut("memory"))
        .and_then(Value::as_table_mut)
    else {
        return;
    };

    for key in REMOVED {
        memory.remove(key);
    }
}

/// v17 → v18: add `[knowledge] enabled`.
///
/// The default is `false` (fresh installs get no KB until activated), but the
/// migration must not strip a working KB from an existing operator: a config
/// that already carries a non-empty `embedding_api_key` — plaintext or
/// `enc2:`-encrypted, presence is what matters — was configured on purpose
/// and upgrades to `enabled = true`.
///
/// PURE: this transform depends only on `raw`. The env case — an operator who
/// supplies the key ONLY via `KB_EMBEDDING_API_KEY`, with nothing in the file —
/// is handled at LOAD by `apply_env_overrides`, which sets `knowledge.enabled`
/// when it folds in a non-empty env key. Reading `std::env` here made the
/// migration outcome depend on which process ran first, and once stamped to the
/// current version it never re-ran — so the KB stayed off permanently with the
/// key present. Keeping this pure honors the module contract (`:9-13`).
fn migrate_v18(raw: &mut Value) {
    let file_key_present = raw
        .get("knowledge")
        .and_then(|k| k.get("embedding_api_key"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());

    // Do NOT inject a `[knowledge]` table into configs that lack one — the serde
    // default already yields `enabled = false` at load, and the v9→v10 invariant
    // (additive fields ride on serde defaults) is pinned by test. Only the
    // existing-table case needs writing.
    let Some(table) = raw.as_table_mut() else {
        // Non-table root errors in the runner's own validation; nothing to do.
        return;
    };
    if let Some(kt) = table.get_mut("knowledge").and_then(|k| k.as_table_mut()) {
        // Idempotent: an existing `enabled` value (v18 config re-run) wins.
        kt.entry("enabled")
            .or_insert(Value::Boolean(file_key_present));
    }
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

    /// The three response-cache keys configured a module nothing constructed,
    /// so they advertised behaviour that never existed. A config carrying them
    /// must come out without them.
    #[test]
    fn v17_strips_the_response_cache_keys() {
        let mut raw = parse(
            r#"
schema_version = 16

[memory]
backend = "sqlite"
response_cache_enabled = true
response_cache_ttl_minutes = 120
response_cache_max_entries = 9000
"#,
        );

        assert!(migrate(&mut raw).expect("migration runs"));

        let memory = raw
            .get("memory")
            .and_then(Value::as_table)
            .expect("memory table survives");
        assert!(memory.get("response_cache_enabled").is_none());
        assert!(memory.get("response_cache_ttl_minutes").is_none());
        assert!(memory.get("response_cache_max_entries").is_none());
        assert_eq!(
            memory.get("backend").and_then(Value::as_str),
            Some("sqlite"),
            "unrelated keys must be left alone"
        );
        assert_eq!(version_of(&raw), Some(i64::from(CURRENT_VERSION)));
    }

    /// v22 → v23 is additive-only (`[cron] max_catchup_age_secs`); a config at
    /// schema_version = 22 migrates to 23 with its content untouched.
    #[test]
    fn v23_is_additive_and_preserves_content() {
        let mut raw = parse(
            r#"
schema_version = 22

[cron]
enabled = true
max_run_history = 42
"#,
        );

        assert!(migrate(&mut raw).expect("migration runs"));

        let cron = raw
            .get("cron")
            .and_then(Value::as_table)
            .expect("cron table survives");
        assert_eq!(cron.get("enabled").and_then(Value::as_bool), Some(true));
        assert_eq!(
            cron.get("max_run_history").and_then(Value::as_integer),
            Some(42),
            "existing cron keys must be left alone"
        );
        assert_eq!(version_of(&raw), Some(i64::from(CURRENT_VERSION)));
    }

    #[test]
    fn v17_is_a_noop_when_the_keys_are_absent() {
        let mut raw = parse(
            "schema_version = 16

[memory]
backend = \"markdown\"
",
        );
        assert!(migrate(&mut raw).expect("migration runs"));
        let memory = raw.get("memory").and_then(Value::as_table).unwrap();
        assert_eq!(memory.len(), 1, "nothing else may be touched");
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
    fn v15_to_v16_is_additive_noop_preserving_skill_entries() {
        // v15 → v16 (plan 045) only added `[skills].open_skills_ref`
        // (additive, unset default) plus two DEFAULT-only behavior changes
        // (remote-skill compact injection, literal skill-key encryption) that
        // have no on-disk shape at all. A v15 config migrates to v16 with all
        // existing `[skills.entries.*]` content intact and without the
        // migration injecting `open_skills_ref`.
        let mut raw = parse(
            "schema_version = 15\n[skills.entries.weather]\nenabled = true\n\
             [skills.entries.weather.api_key]\nsource = \"literal\"\nvalue = \"placeholder\"\n",
        );
        let changed = migrate(&mut raw).unwrap();
        assert!(changed, "v15 bump should be reported as transformed");
        assert_eq!(version_of(&raw), Some(i64::from(CURRENT_VERSION)));
        let skills = raw.get("skills").expect("skills table survives");
        assert!(
            skills.get("open_skills_ref").is_none(),
            "migration must not inject open_skills_ref; serde default handles it"
        );
        let entry = skills
            .get("entries")
            .and_then(|e| e.get("weather"))
            .expect("skill entry survives");
        assert_eq!(entry.get("enabled").and_then(Value::as_bool), Some(true));
        assert_eq!(
            entry
                .get("api_key")
                .and_then(|k| k.get("value"))
                .and_then(|v| v.as_str()),
            Some("placeholder"),
            "migration itself must not touch the literal value; encryption \
             happens on the next Config::save(), not during schema migration"
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

    #[test]
    fn string_version_is_rejected() {
        // A string `schema_version = "17"` must error clearly, not fall to 0 and
        // silently re-run the whole chain rewriting the file every load.
        let mut v = parse("schema_version = \"17\"\n");
        let err = migrate(&mut v).expect_err("string version must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("malformed"), "got: {msg}");
    }

    #[test]
    fn float_version_is_rejected() {
        let mut v = parse("schema_version = 17.0\n");
        let err = migrate(&mut v).expect_err("float version must be rejected");
        assert!(format!("{err:#}").contains("malformed"));
    }

    #[test]
    fn negative_version_is_rejected() {
        let mut v = parse("schema_version = -1\n");
        let err = migrate(&mut v).expect_err("negative version must be rejected");
        assert!(format!("{err:#}").contains("negative"));
    }

    #[test]
    fn dropped_keys_are_removed_on_migration() {
        let mut v = parse(
            "schema_version = 24\n\
             [cost]\nenabled = true\nallow_override = true\n\
             prices = { \"anthropic/x\" = { input = 1.0, output = 2.0 } }\n\
             [agent]\nparallel_tools = true\nmax_tool_iterations = 30\n",
        );
        assert!(migrate(&mut v).unwrap());
        assert_eq!(version_of(&v), Some(i64::from(CURRENT_VERSION)));

        let cost = v
            .get("cost")
            .and_then(Value::as_table)
            .expect("cost survives");
        assert!(
            cost.get("allow_override").is_none(),
            "allow_override dropped"
        );
        assert!(cost.get("prices").is_none(), "prices dropped");
        assert_eq!(
            cost.get("enabled").and_then(Value::as_bool),
            Some(true),
            "kept cost keys survive"
        );

        let agent = v
            .get("agent")
            .and_then(Value::as_table)
            .expect("agent survives");
        assert!(
            agent.get("parallel_tools").is_none(),
            "parallel_tools dropped"
        );
        assert_eq!(
            agent.get("max_tool_iterations").and_then(Value::as_integer),
            Some(30),
            "kept agent keys survive"
        );
    }

    /// Serialize the v18 tests against every env-mutating test in the crate and
    /// scrub `KB_EMBEDDING_API_KEY` for their duration. `migrate_v18` is now pure
    /// (it ignores the env), but the purity test SETS the var and the others
    /// assert the no-key shape, so an ambient value on a dev/CI machine would
    /// still perturb them.
    struct V18EnvGuard {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
    }
    impl V18EnvGuard {
        fn scrubbed() -> Self {
            let lock = crate::test_env::ENV_LOCK.blocking_lock();
            let prev = std::env::var_os("KB_EMBEDDING_API_KEY");
            std::env::remove_var("KB_EMBEDDING_API_KEY");
            Self { _lock: lock, prev }
        }
    }
    impl Drop for V18EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("KB_EMBEDDING_API_KEY", v),
                None => std::env::remove_var("KB_EMBEDDING_API_KEY"),
            }
        }
    }

    #[test]
    fn v18_is_pure_ignores_env() {
        // migrate_v18 is a pure `toml::Value` transform now — the env-only case
        // is handled at LOAD by `apply_env_overrides`, not here. Setting the env
        // var must NOT change the migration output (with no file key, no table
        // is injected).
        let _guard = V18EnvGuard::scrubbed();
        std::env::set_var("KB_EMBEDDING_API_KEY", "rantaiclaw_test_key");
        let mut v = parse("schema_version = 17\n");
        migrate(&mut v).unwrap();
        std::env::remove_var("KB_EMBEDDING_API_KEY");
        assert!(
            v.get("knowledge").is_none(),
            "v18 must ignore the env key — it is handled at load, not in the migration"
        );
    }

    #[test]
    fn v18_configured_install_upgrades_on() {
        let _guard = V18EnvGuard::scrubbed();
        // THE protective test: an operator with a working KB (key present)
        // must not lose it on upgrade. Fails if the rule is inverted.
        let mut v = parse("schema_version = 17\n[knowledge]\nembedding_api_key = \"enc2:abc\"\n");
        migrate(&mut v).unwrap();
        assert_eq!(version_of(&v), Some(i64::from(CURRENT_VERSION)));
        let k = v.get("knowledge").unwrap().as_table().unwrap();
        assert_eq!(
            k.get("enabled").unwrap().as_bool(),
            Some(true),
            "a config that carries a key was configured on purpose — upgrade ON"
        );
    }

    #[test]
    fn v18_unconfigured_install_upgrades_off() {
        let _guard = V18EnvGuard::scrubbed();
        // No key anywhere: the migration must not inject a [knowledge]
        // table (v9->v10 invariant — serde default yields enabled = false
        // at load), so the KB stays off exactly as it already was.
        let mut v = parse("schema_version = 17\n");
        migrate(&mut v).unwrap();
        assert!(
            v.get("knowledge").is_none(),
            "no key -> no injected table; serde default supplies enabled=false"
        );
    }

    #[test]
    fn v19_email_config_is_untouched_and_stamped() {
        // v18 -> v19 is additive: `require_authenticated_sender` defaults to
        // `false`, which is the old behaviour, so an existing email config must
        // come through byte-identical and only the stamp moves. The flag is
        // supplied by serde at load, exactly like the v9->v10 invariant.
        let _guard = V18EnvGuard::scrubbed();
        let mut v = parse(
            "schema_version = 18\n[channels.email]\nimap_host = \"imap.example.com\"\nsmtp_host = \"smtp.example.com\"\n",
        );
        migrate(&mut v).unwrap();
        assert_eq!(version_of(&v), Some(i64::from(CURRENT_VERSION)));
        let email = v
            .get("channels")
            .and_then(|c| c.get("email"))
            .and_then(Value::as_table)
            .expect("email table survives");
        assert_eq!(
            email.get("imap_host").and_then(Value::as_str),
            Some("imap.example.com")
        );
        assert!(
            email.get("require_authenticated_sender").is_none(),
            "the migration must not inject the key — serde's default supplies it"
        );
    }

    #[test]
    fn v20_irc_config_is_untouched_and_stamped() {
        // Additive: the refusal it gates is the new default, so an existing
        // IRC config comes through byte-identical and only the stamp moves.
        // An operator who needs the old behaviour writes the key themselves —
        // a migration must not opt them back into a credential disclosure.
        let _guard = V18EnvGuard::scrubbed();
        let mut v = parse(
            "schema_version = 19\n[channels.irc]\nserver = \"irc.example.com\"\nnickname = \"rantaiclaw_bot\"\nverify_tls = false\n",
        );
        migrate(&mut v).unwrap();
        assert_eq!(version_of(&v), Some(i64::from(CURRENT_VERSION)));
        let irc = v
            .get("channels")
            .and_then(|c| c.get("irc"))
            .and_then(Value::as_table)
            .expect("irc table survives");
        assert_eq!(irc.get("verify_tls").and_then(Value::as_bool), Some(false));
        assert!(
            irc.get("allow_insecure_tls_with_password").is_none(),
            "the migration must not grant the opt-in on the operator's behalf"
        );
    }

    #[test]
    fn v22_mattermost_thread_override_survives_the_shared_default() {
        // The per-channel key predates the shared one and operators have it
        // set; a migration that dropped it would silently re-enable threading
        // on a channel where it was turned off.
        let _guard = V18EnvGuard::scrubbed();
        let mut v = parse(
            "schema_version = 21\n[channels_config.mattermost]\nurl = \"https://mm.example.com\"\nbot_token = \"t\"\nthread_replies = false\n",
        );
        migrate(&mut v).unwrap();
        assert_eq!(version_of(&v), Some(i64::from(CURRENT_VERSION)));
        let mm = v
            .get("channels_config")
            .and_then(|c| c.get("mattermost"))
            .and_then(Value::as_table)
            .expect("mattermost table survives");
        assert_eq!(
            mm.get("thread_replies").and_then(Value::as_bool),
            Some(false)
        );
        // The shared key is left absent so the serde default (`true`) applies —
        // the migration must not write an operator's preference for them.
        assert!(v
            .get("channels_config")
            .and_then(|c| c.get("thread_replies"))
            .is_none());
    }

    #[test]
    fn v21_webhook_config_keeps_loading_with_the_removed_port_key() {
        // The key is gone from the schema, so the only thing that matters is
        // that a config still carrying it loads instead of being rejected.
        let _guard = V18EnvGuard::scrubbed();
        let mut v = parse(
            "schema_version = 20\n[channels_config.webhook]\nport = 8080\nsecret = \"shared\"\n",
        );
        migrate(&mut v).unwrap();
        assert_eq!(version_of(&v), Some(i64::from(CURRENT_VERSION)));
        let webhook = v
            .get("channels_config")
            .and_then(|c| c.get("webhook"))
            .and_then(Value::as_table)
            .expect("webhook table survives");
        assert_eq!(
            webhook.get("secret").and_then(Value::as_str),
            Some("shared"),
            "the key that IS read must survive"
        );
        // The dead key is left in place rather than rewritten out: a migration
        // that edits a file to remove something inert is churn, and serde
        // ignores it.
        assert!(webhook.get("port").is_some());
        let parsed: crate::config::schema::WebhookConfig =
            toml::from_str("port = 8080\nsecret = \"shared\"\n")
                .expect("a section still carrying `port` must load, not error");
        assert_eq!(parsed.secret.as_deref(), Some("shared"));
    }

    #[test]
    fn v18_existing_enabled_false_with_key_is_preserved() {
        let _guard = V18EnvGuard::scrubbed();
        // Idempotence for the deliberate deactivated-but-configured state:
        // re-running migration on a v18-shaped config must NOT flip it on.
        let mut v = parse(
            "schema_version = 17\n[knowledge]\nenabled = false\nembedding_api_key = \"enc2:abc\"\n",
        );
        migrate(&mut v).unwrap();
        let k = v.get("knowledge").unwrap().as_table().unwrap();
        assert_eq!(
            k.get("enabled").unwrap().as_bool(),
            Some(false),
            "an explicit enabled value must win over the derivation"
        );
    }
}
