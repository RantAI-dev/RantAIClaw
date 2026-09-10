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

// ── v32: WhatsApp Web gets its own table ────────────────────────────────────
//
// These three carry the risk of the v32 bump. The owner's own config is the
// first case — `session_path` set, no `phone_number_id` — so this migration
// runs on a real machine on first launch after the upgrade.

/// Build a v31 config whose `[channels_config.whatsapp]` holds `keys`.
fn v31_with_whatsapp(keys: &str) -> toml::Value {
    let toml_src = format!(
        "schema_version = 31\n\n[channels_config.whatsapp]\n{keys}\n",
        keys = keys
    );
    toml::from_str(&toml_src).expect("fixture parses")
}

fn table<'a>(v: &'a toml::Value, path: &[&str]) -> Option<&'a toml::Value> {
    let mut cur = v;
    for seg in path {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// Case 1, the owner's own config: Web keys only.
///
/// The Web keys move to the new table and the Cloud table goes away entirely,
/// rather than being left behind empty — an empty `[channels_config.whatsapp]`
/// would make `channel_is_configured("whatsapp")` answer yes for a transport
/// that cannot run.
#[test]
fn v32_moves_a_web_only_config_into_its_own_table() {
    let mut v = v31_with_whatsapp(
        "session_path = \"/home/rantaiclaw_user/.rantaiclaw/wa.db\"\n\
         pair_phone = \"15551234567\"\n\
         allowed_numbers = [\"+15550000001\"]",
    );
    migrate(&mut v).expect("migrate runs");

    let web = table(&v, &["channels_config", "whatsapp_web"]).expect("web table created");
    assert_eq!(
        web.get("session_path").and_then(toml::Value::as_str),
        Some("/home/rantaiclaw_user/.rantaiclaw/wa.db"),
        "the session path must survive the move"
    );
    assert_eq!(
        web.get("pair_phone").and_then(toml::Value::as_str),
        Some("15551234567")
    );
    assert_eq!(
        web.get("allowed_numbers")
            .and_then(toml::Value::as_array)
            .map(Vec::len),
        Some(1),
        "the allowlist must follow the transport that reads it"
    );
    assert!(
        table(&v, &["channels_config", "whatsapp"]).is_none(),
        "an empty Cloud table must not be left behind: {v:?}"
    );

    let cfg: Result<Config, _> = v.try_into();
    assert!(cfg.is_ok(), "migrated config must load: {:?}", cfg.err());
}

/// Case 2: both transports in the old table. Nothing the operator wrote is
/// dropped — both tables exist afterwards, each with its own keys.
#[test]
fn v32_splits_a_dual_transport_config_into_both_tables() {
    let mut v = v31_with_whatsapp(
        "access_token = \"cloud-token\"\n\
         phone_number_id = \"1234567890\"\n\
         verify_token = \"verify\"\n\
         session_path = \"/var/lib/rantaiclaw/wa.db\"\n\
         pair_code = \"ABCD1234\"\n\
         allowed_numbers = [\"*\"]",
    );
    migrate(&mut v).expect("migrate runs");

    let cloud = table(&v, &["channels_config", "whatsapp"]).expect("cloud table kept");
    assert_eq!(
        cloud.get("access_token").and_then(toml::Value::as_str),
        Some("cloud-token")
    );
    assert_eq!(
        cloud.get("phone_number_id").and_then(toml::Value::as_str),
        Some("1234567890")
    );
    assert!(
        cloud.get("session_path").is_none() && cloud.get("pair_code").is_none(),
        "Web keys must not be left in the Cloud table: {cloud:?}"
    );

    let web = table(&v, &["channels_config", "whatsapp_web"]).expect("web table created");
    assert_eq!(
        web.get("session_path").and_then(toml::Value::as_str),
        Some("/var/lib/rantaiclaw/wa.db")
    );
    assert_eq!(
        web.get("pair_code").and_then(toml::Value::as_str),
        Some("ABCD1234")
    );
    assert_eq!(
        web.get("allowed_numbers")
            .and_then(toml::Value::as_array)
            .map(Vec::len),
        Some(1),
        "allowed_numbers is copied, not moved: both transports read it"
    );
    assert_eq!(
        cloud
            .get("allowed_numbers")
            .and_then(toml::Value::as_array)
            .map(Vec::len),
        Some(1),
        "the Cloud table must keep the allowlist it already had"
    );

    let cfg: Result<Config, _> = v.try_into();
    assert!(cfg.is_ok(), "migrated config must load: {:?}", cfg.err());
}

/// Case 3: Cloud only. Nothing moves and no Web table appears — a v32 config
/// that invented an empty `[channels_config.whatsapp_web]` would report a
/// channel the operator never asked for.
#[test]
fn v32_leaves_a_cloud_only_config_alone() {
    let mut v = v31_with_whatsapp(
        "access_token = \"cloud-token\"\n\
         phone_number_id = \"1234567890\"\n\
         verify_token = \"verify\"\n\
         allowed_numbers = [\"+15550000001\"]",
    );
    migrate(&mut v).expect("migrate runs");

    let cloud = table(&v, &["channels_config", "whatsapp"]).expect("cloud table kept");
    assert_eq!(
        cloud.get("access_token").and_then(toml::Value::as_str),
        Some("cloud-token")
    );
    assert!(
        table(&v, &["channels_config", "whatsapp_web"]).is_none(),
        "no Web table may be invented for a Cloud-only operator: {v:?}"
    );

    let cfg: Result<Config, _> = v.try_into();
    assert!(cfg.is_ok(), "migrated config must load: {:?}", cfg.err());
}

/// The version the migration chain claims to reach. If this drifts from
/// `CURRENT_VERSION` the three cases above are testing a migration nobody runs.
#[test]
fn v32_is_the_current_version() {
    assert_eq!(CURRENT_VERSION, 32);
    let mut v = v31_with_whatsapp("session_path = \"/tmp/wa.db\"");
    migrate(&mut v).expect("migrate runs");
    assert_eq!(
        v.get(SCHEMA_VERSION_KEY).and_then(toml::Value::as_integer),
        Some(32),
        "the migrated config must be stamped with the version it reached"
    );
}

/// A v31 Web config that never listed an allowlist must land fail-closed.
///
/// The migration copies `allowed_numbers` when it exists; when it does not, the
/// new table must default to deny-all rather than inheriting anything wider.
#[test]
fn v32_leaves_a_web_config_without_an_allowlist_denying_everyone() {
    let mut v = v31_with_whatsapp("session_path = \"/tmp/wa.db\"");
    migrate(&mut v).expect("migrate runs");

    let cfg: Config = v.try_into().expect("migrated config loads");
    let web = cfg.channels_config.whatsapp_web.expect("web table created");
    assert!(
        web.allowed_numbers.is_empty(),
        "an absent allowlist must stay empty, not become a wildcard: {:?}",
        web.allowed_numbers
    );
}
