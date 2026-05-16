//! Schema-drift enforcement tests.
//!
//! These integration tests are the "you can't quietly break the update
//! cycle" backstop for the migration framework in `src/config/migrations.rs`
//! and `src/sessions/migrations.rs`. They snapshot the *shape* of the
//! current schema for both stores (config TOML structure via JSON Schema,
//! sessions.db DDL via `sqlite_master`) and compare against a checked-in
//! baseline.
//!
//! When a maintainer changes either schema, these tests fail with a clear
//! "you changed the schema — bump CURRENT_VERSION and add a migration"
//! message. To unblock:
//!
//!   1. Bump `CURRENT_VERSION` in the relevant module.
//!   2. Add a `migrate_vN` function + chain arm.
//!   3. Add a unit test for the new migration.
//!   4. Re-run with `INSTA_UPDATE=auto cargo test --test schema_drift`
//!      (or `cargo insta accept`) to refresh the snapshot.
//!
//! Step 4 is the conscious-acknowledgement point — the maintainer has to
//! actively accept the new snapshot, which the PR reviewer can spot.

use rantaiclaw::config::{migrations as config_migrations, Config};
use rusqlite::Connection;
use schemars::schema_for;

/// Snapshot of the `Config` JSON Schema. Any field rename, type change,
/// or required-field change here trips this test. Cosmetic changes
/// (doc comments) also trip it — that's intentional friction so the
/// maintainer always knows the on-disk surface moved.
#[test]
fn config_schema_does_not_drift_unannounced() {
    let schema = schema_for!(Config);
    let pretty = serde_json::to_string_pretty(&schema).expect("schema serialises");

    insta::with_settings!({
        description => "Config JSON Schema fingerprint. If this test fails, you changed the on-disk \
                        config.toml surface. Bump config::migrations::CURRENT_VERSION, add a \
                        migrate_vN function, write a unit test, then `cargo insta accept` here.",
        snapshot_suffix => format!("v{}", config_migrations::CURRENT_VERSION),
    }, {
        insta::assert_snapshot!("config_schema", pretty);
    });
}

/// Snapshot of the sessions.db DDL after running migrations. Any table,
/// column, index, or trigger added/removed/renamed without bumping the
/// sessions `CURRENT_VERSION` will trip this test.
#[test]
fn sessions_ddl_does_not_drift_unannounced() {
    let conn = Connection::open_in_memory().expect("in-memory sqlite");
    // `run_migrations` is re-exported via `sessions::run_migrations`;
    // the version constant is private to the module, but the asserted
    // shape is what we actually care about here.
    rantaiclaw::sessions::run_migrations(&conn).expect("run sessions migrations");

    let ddl = dump_sqlite_master(&conn);

    insta::with_settings!({
        description => "sessions.db DDL fingerprint. If this test fails, you changed the SQLite \
                        schema. Bump sessions::migrations::CURRENT_VERSION, add a migrate_vN \
                        function, write a unit test, then `cargo insta accept` here.",
    }, {
        insta::assert_snapshot!("sessions_ddl", ddl);
    });
}

/// Read every CREATE statement in `sqlite_master`, normalise whitespace,
/// and sort so the snapshot is deterministic across SQLite versions.
fn dump_sqlite_master(conn: &Connection) -> String {
    let mut stmt = conn
        .prepare(
            "SELECT type, name, sql FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL \
             ORDER BY type, name",
        )
        .expect("prepare sqlite_master query");
    let rows = stmt
        .query_map([], |row| {
            let kind: String = row.get(0)?;
            let name: String = row.get(1)?;
            let sql: String = row.get(2)?;
            Ok((kind, name, sql))
        })
        .expect("read sqlite_master")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect sqlite_master rows");

    let mut out = String::new();
    for (kind, name, sql) in rows {
        let normalised = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        out.push_str(&format!("-- {kind} {name}\n{normalised}\n\n"));
    }
    out
}
