//! Schema definition + migration for the sqlite-vec backend.
//!
//! Schema lives here as a single `execute_batch` string so the entire on-disk
//! layout is auditable in one place. Bumping [`SCHEMA_VERSION`] requires a
//! corresponding migration branch below.

use std::os::raw::{c_char, c_int};
use std::sync::Once;

use rusqlite::{ffi, Connection};

use crate::kb::KbResult;

/// Raw signature SQLite expects for an extension entry point. Matches the
/// `xEntryPoint` parameter type in `sqlite3_auto_extension`.
type SqliteEntryPoint = unsafe extern "C" fn(
    db: *mut ffi::sqlite3,
    pz_err_msg: *mut *mut c_char,
    _: *const ffi::sqlite3_api_routines,
) -> c_int;

/// Schema version — bump and add a migration branch when fields change.
pub const SCHEMA_VERSION: i64 = 1;

/// Register the sqlite-vec extension as an auto-extension once per process.
///
/// The `sqlite-vec` 0.1 crate exposes `sqlite3_vec_init()` as a bare extern
/// (the SQLite extension init signature), not as a `(conn) -> Result` helper.
/// The supported integration pattern is to register it via
/// `sqlite3_auto_extension` before opening connections — see the crate's own
/// test in `src/lib.rs`. We guard with `std::sync::Once` so re-registration
/// across `SqliteStore::open` calls is a no-op.
pub(crate) fn ensure_vec_extension_registered() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `sqlite3_vec_init` is declared by the sqlite-vec crate as
        // `unsafe extern "C" fn()` to keep the binding minimal, but the
        // underlying C symbol has the SQLite extension entry-point signature
        // (`db, pzErrMsg, pApi -> int`). Transmute to that real signature
        // before passing to `sqlite3_auto_extension`. This mirrors the
        // canonical usage in the crate's own integration test
        // (`sqlite-vec/src/lib.rs:test_rusqlite_auto_extension`).
        unsafe {
            let init = std::mem::transmute::<unsafe extern "C" fn(), SqliteEntryPoint>(
                sqlite_vec::sqlite3_vec_init,
            );
            ffi::sqlite3_auto_extension(Some(init));
        }
    });
}

/// Initialize sqlite-vec for this connection. Safe to call even after the
/// auto-extension is already registered.
pub(crate) fn load_vec_extension(_conn: &Connection) -> KbResult<()> {
    ensure_vec_extension_registered();
    Ok(())
}

pub fn migrate(conn: &Connection, embedding_dim: usize) -> KbResult<()> {
    load_vec_extension(conn)?;

    conn.execute_batch(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS kb_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

        CREATE TABLE IF NOT EXISTS document (
            id              TEXT PRIMARY KEY,
            title           TEXT NOT NULL,
            content         TEXT NOT NULL,
            categories_json TEXT NOT NULL DEFAULT '[]',
            subcategory     TEXT,
            metadata_json   TEXT NOT NULL DEFAULT '{{}}',
            s3_key          TEXT,
            file_type       TEXT,
            mime_type       TEXT,
            file_size       INTEGER,
            organization_id TEXT,
            created_by      TEXT,
            session_id      TEXT,
            artifact_type   TEXT,
            created_at      TEXT NOT NULL,
            updated_at      TEXT NOT NULL,
            deleted_at      TEXT,
            retention_days  INTEGER,
            retrieval_count INTEGER NOT NULL DEFAULT 0,
            last_retrieved_at TEXT
        );
        CREATE INDEX IF NOT EXISTS document_org_idx ON document(organization_id);
        CREATE INDEX IF NOT EXISTS document_deleted_idx ON document(deleted_at);

        CREATE TABLE IF NOT EXISTS knowledge_base_group (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            description     TEXT,
            color           TEXT,
            organization_id TEXT,
            created_by      TEXT,
            created_at      TEXT NOT NULL,
            updated_at      TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS document_group (
            document_id TEXT NOT NULL,
            group_id    TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            PRIMARY KEY (document_id, group_id),
            FOREIGN KEY (document_id) REFERENCES document(id) ON DELETE CASCADE,
            FOREIGN KEY (group_id)    REFERENCES knowledge_base_group(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS document_group_group_idx ON document_group(group_id);

        CREATE TABLE IF NOT EXISTS category (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            label           TEXT NOT NULL,
            color           TEXT NOT NULL,
            is_system       INTEGER NOT NULL DEFAULT 0,
            organization_id TEXT,
            created_at      TEXT NOT NULL,
            updated_at      TEXT NOT NULL,
            UNIQUE (organization_id, name)
        );

        CREATE TABLE IF NOT EXISTS chunk (
            id                TEXT PRIMARY KEY,
            document_id       TEXT NOT NULL,
            content           TEXT NOT NULL,
            chunk_index       INTEGER NOT NULL,
            metadata_json     TEXT NOT NULL DEFAULT '{{}}',
            contextual_prefix TEXT,
            embedding_model   TEXT,
            created_at        TEXT NOT NULL,
            FOREIGN KEY (document_id) REFERENCES document(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS chunk_document_idx ON chunk(document_id);
        CREATE INDEX IF NOT EXISTS chunk_embed_model_idx ON chunk(embedding_model);

        -- sqlite-vec virtual table for vectors. The dimension is fixed at table
        -- creation; changing KB_EMBEDDING_DIM requires migration via the
        -- bulk_re_embed path.
        CREATE VIRTUAL TABLE IF NOT EXISTS chunk_vec USING vec0(
            embedding float[{dim}]
        );

        -- FTS5 BM25 for lexical search.
        CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts USING fts5(
            content,
            content='chunk',
            content_rowid='rowid',
            tokenize='porter unicode61'
        );

        -- Keep FTS in sync via triggers (sqlite-recommended pattern).
        CREATE TRIGGER IF NOT EXISTS chunk_ai AFTER INSERT ON chunk BEGIN
            INSERT INTO chunk_fts(rowid, content) VALUES (new.rowid, new.content);
        END;
        CREATE TRIGGER IF NOT EXISTS chunk_ad AFTER DELETE ON chunk BEGIN
            INSERT INTO chunk_fts(chunk_fts, rowid, content) VALUES('delete', old.rowid, old.content);
        END;
        CREATE TRIGGER IF NOT EXISTS chunk_au AFTER UPDATE ON chunk BEGIN
            INSERT INTO chunk_fts(chunk_fts, rowid, content) VALUES('delete', old.rowid, old.content);
            INSERT INTO chunk_fts(rowid, content) VALUES (new.rowid, new.content);
        END;
    "#,
        dim = embedding_dim
    ))?;

    conn.execute(
        "INSERT OR REPLACE INTO kb_meta(key, value) VALUES('schema_version', ?1)",
        rusqlite::params![SCHEMA_VERSION.to_string()],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO kb_meta(key, value) VALUES('embedding_dim', ?1)",
        rusqlite::params![embedding_dim.to_string()],
    )?;

    Ok(())
}

pub fn current_embedding_dim(conn: &Connection) -> KbResult<Option<usize>> {
    let val: Option<String> = conn
        .query_row(
            "SELECT value FROM kb_meta WHERE key='embedding_dim'",
            [],
            |row| row.get(0),
        )
        .ok();
    Ok(val.and_then(|s| s.parse().ok()))
}
