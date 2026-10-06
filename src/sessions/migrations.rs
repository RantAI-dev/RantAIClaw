use anyhow::Result;
use rusqlite::Connection;

pub const CURRENT_VERSION: i32 = 2;

pub fn run_migrations(conn: &Connection) -> Result<()> {
    let version = get_schema_version(conn)?;

    if version < 1 {
        migrate_v1(conn)?;
    }

    if version < 2 {
        migrate_v2(conn)?;
    }

    Ok(())
}

fn get_schema_version(conn: &Connection) -> Result<i32> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY)",
        [],
    )?;

    let version: i32 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;

    Ok(version)
}

fn set_schema_version(conn: &Connection, version: i32) -> Result<()> {
    conn.execute("DELETE FROM schema_version", [])?;
    conn.execute("INSERT INTO schema_version (version) VALUES (?)", [version])?;
    Ok(())
}

fn migrate_v1(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            title TEXT,
            parent_session_id TEXT,
            model TEXT NOT NULL,
            started_at INTEGER NOT NULL,
            ended_at INTEGER,
            message_count INTEGER DEFAULT 0,
            token_count INTEGER DEFAULT 0,
            source TEXT DEFAULT 'tui',
            FOREIGN KEY (parent_session_id) REFERENCES sessions(id)
        );

        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            tool_calls TEXT,
            timestamp INTEGER NOT NULL,
            FOREIGN KEY (session_id) REFERENCES sessions(id)
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            content,
            content=messages,
            content_rowid=id
        );

        CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);
        CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, timestamp);

        CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
            INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
        END;
        "#,
    )?;

    set_schema_version(conn, 1)?;
    Ok(())
}

/// v1 → v2: per-conversation channel recording.
///
/// Adds `sessions.conversation_key` and the index the channel recording path uses
/// to find the open session for a given chat. The column is nullable on
/// purpose: every session written before v2 has no key to recover — the
/// conversation-keyed path is opt-in and only rows tagged `source = 'channel'`
/// ever set it.
///
/// The index covers `(conversation_key, started_at DESC)` because the
/// channel-recording lookup is "the open session for this key, most recent
/// first". A key with no open session opens a new one; the lookup needs to
/// answer that in one round trip.
fn migrate_v2(conn: &Connection) -> Result<()> {
    // A previous build that crashed between the `ALTER TABLE` and the
    // version write would leave the table already carrying the column, so a
    // second migration would error with "duplicate column name". Skip the
    // ALTER when `pragma_table_info` already lists it. The version write is
    // inside the same transaction so a crash anywhere in this function
    // leaves the schema at v1 and the next open retries the migration.
    let tx = conn.unchecked_transaction()?;

    let has_column: i64 = tx.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('sessions') \
         WHERE name = 'conversation_key'",
        [],
        |row| row.get(0),
    )?;
    if has_column == 0 {
        tx.execute_batch("ALTER TABLE sessions ADD COLUMN conversation_key TEXT;")?;
    }

    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_sessions_conversation_key \
         ON sessions(conversation_key, started_at DESC);",
    )?;

    tx.execute("DELETE FROM schema_version", [])?;
    // Stamp the literal 2 so a later v3 cannot accidentally push the row to
    // its own value before the v3 changes have run. `CURRENT_VERSION` is the
    // current head; the per-migration stamp must always be the version that
    // migration introduces.
    tx.execute("INSERT INTO schema_version (version) VALUES (?1)", [2])?;

    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_run_without_error() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        let version = get_schema_version(&conn).unwrap();
        assert_eq!(version, CURRENT_VERSION);
    }

    #[test]
    fn migrations_are_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        let version = get_schema_version(&conn).unwrap();
        assert_eq!(version, CURRENT_VERSION);
    }

    /// A populated v1 store must survive the v1 → v2 ALTER TABLE: every row keeps
    /// its messages, its title, its counters. `conversation_key` is NULL for
    /// rows that pre-date the column — the channel path only ever sets it on
    /// new rows.
    #[test]
    fn migration_v1_to_v2_preserves_existing_rows() {
        let conn = Connection::open_in_memory().unwrap();
        // Bring the connection up to v1 by hand: writes the table without
        // `conversation_key`, then a row + message that must survive the
        // upgrade.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);",
        )
        .unwrap();
        migrate_v1(&conn).unwrap();

        conn.execute(
            "INSERT INTO sessions (id, title, model, started_at, source) \
             VALUES ('row-1', 'a title', 'm', 100, 'tui')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) \
             VALUES ('row-1', 'user', 'hello', 100)",
            [],
        )
        .unwrap();

        // v2 must not touch the existing row.
        migrate_v2(&conn).unwrap();

        let (title, key): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT title, conversation_key FROM sessions WHERE id = 'row-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(title.as_deref(), Some("a title"), "title preserved");
        assert!(
            key.is_none(),
            "pre-v2 rows keep conversation_key NULL: got {key:?}"
        );

        let msg_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = 'row-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(msg_count, 1, "messages table is untouched");
    }

    /// The v2 migration runs an `ALTER TABLE sessions ADD COLUMN
    /// conversation_key` plus `CREATE INDEX` on a live file-backed store
    /// while another connection may still hold `sessions.db` open. The
    /// channel recording layer keeps a long-lived connection to that file,
    /// so the v1 → v2 schema bump must not race against it.
    ///
    /// Pin test for the file-backed store: with WAL and `busy_timeout` set
    /// the same way `SessionStore::open` sets them, the migration must
    /// succeed even while the first connection is mid-read. If SQLite
    /// genuinely refuses (`SQLITE_BUSY` beyond the timeout), that is the
    /// STOP condition the plan names — the migration cannot run while the
    /// TUI holds `sessions.db` open — and the test must surface the error
    /// rather than work around it.
    #[test]
    fn migration_v2_runs_while_a_reader_holds_the_database_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.db");

        // First connection: open the file, bring the schema up to v1 by
        // hand (CREATE TABLE sessions — no `conversation_key` column yet),
        // insert a row, and start a read transaction that stays alive
        // across the second connection's migration.
        let reader = Connection::open(&db_path).expect("open reader");
        reader
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
            )
            .expect("reader pragmas");
        reader
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);",
            )
            .expect("schema_version table");
        migrate_v1(&reader).expect("seed v1 schema");
        reader
            .execute(
                "INSERT INTO sessions (id, model, started_at, source) \
                 VALUES ('reader-row', 'm', 1, 'tui')",
                [],
            )
            .expect("seed row");

        let reader_tx = reader.unchecked_transaction().expect("begin reader tx");
        let pre: i64 = reader_tx
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .expect("reader pre-migration count");
        assert_eq!(
            pre, 1,
            "row is visible to the held reader transaction pre-migration"
        );

        // Second connection: open the file the way SessionStore::open does
        // (WAL + busy_timeout + foreign keys), then run_migrations. The
        // v1 → v2 ALTER TABLE + CREATE INDEX must succeed while the
        // reader's transaction is still live.
        let writer = Connection::open(&db_path).expect("open writer");
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
            )
            .expect("writer pragmas");
        run_migrations(&writer)
            .expect("v2 migration must run while the reader connection holds sessions.db open");

        // The reader's transaction is still alive: the row survives the
        // migration. Committing the transaction lets the file finalise so
        // the post-migration schema checks can read it cleanly.
        let mid: i64 = reader_tx
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .expect("reader mid-migration count");
        assert_eq!(
            mid, 1,
            "row survives the v1→v2 migration inside the held reader transaction"
        );
        reader_tx.commit().expect("commit reader tx");

        // Schema is at v2 with the new column and index present on the
        // writer (the connection that performed the migration).
        let version = get_schema_version(&writer).expect("schema version");
        assert_eq!(version, CURRENT_VERSION);

        let col: i64 = writer
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('sessions') \
                 WHERE name = 'conversation_key'",
                [],
                |r| r.get(0),
            )
            .expect("column check");
        assert_eq!(col, 1, "conversation_key column present post-migration");

        let idx: i64 = writer
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_sessions_conversation_key'",
                [],
                |r| r.get(0),
            )
            .expect("index check");
        assert_eq!(
            idx, 1,
            "idx_sessions_conversation_key present post-migration"
        );

        // The reader's row survived, and its `conversation_key` is NULL —
        // the ALTER TABLE adds the column with NULL for existing rows.
        let still_there: (String, Option<String>) = reader
            .query_row(
                "SELECT id, conversation_key FROM sessions WHERE id = 'reader-row'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("reader row still readable post-migration");
        assert_eq!(still_there.0, "reader-row", "row id preserved");
        assert!(
            still_there.1.is_none(),
            "pre-v2 row keeps conversation_key NULL: got {:?}",
            still_there.1
        );
    }

    /// A second call to `run_migrations` after v2 must leave the schema at v2
    /// and not fail on the `ADD COLUMN` or the index `CREATE` — the channel
    /// runtime opens `sessions.db` on every message, so a second migration
    /// pass is the steady state, not an exception.
    #[test]
    fn migration_v2_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        // Running again must be a no-op, not an "duplicate column" error.
        run_migrations(&conn).unwrap();

        let version = get_schema_version(&conn).unwrap();
        assert_eq!(version, CURRENT_VERSION);

        // The new column and index both still exist.
        let col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('sessions') \
                 WHERE name = 'conversation_key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(col, 1, "conversation_key column is present");

        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_sessions_conversation_key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1, "idx_sessions_conversation_key exists");
    }

    /// A `sessions.db` that already has the `conversation_key` column added
    /// by hand (e.g. a v2 migration that crashed between the `ALTER TABLE`
    /// and the version write) must open cleanly through `run_migrations`
    /// and land at v2 — the migration has to be tolerant of its own
    /// partially-applied state, not just of running twice.
    #[test]
    fn migration_v2_is_runnable_when_conversation_key_column_already_exists() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);",
        )
        .unwrap();
        migrate_v1(&conn).unwrap();
        // A row exists, and the v2 column has been added by hand — the
        // situation a crash between `ALTER TABLE` and the version write
        // leaves the file in. `pragma_table_info` already lists the column.
        conn.execute("ALTER TABLE sessions ADD COLUMN conversation_key TEXT", [])
            .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, model, started_at, source) \
             VALUES ('row-1', 'm', 100, 'tui')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) \
             VALUES ('row-1', 'user', 'hello', 100)",
            [],
        )
        .unwrap();

        // Re-running the v2 migration must not raise "duplicate column",
        // and the existing row must keep its data.
        migrate_v2(&conn).expect("v2 must be re-runnable on a partially-migrated db");

        let version = get_schema_version(&conn).unwrap();
        assert_eq!(version, CURRENT_VERSION);

        let (title, key): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT title, conversation_key FROM sessions WHERE id = 'row-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(
            key.is_none(),
            "the hand-inserted row keeps key NULL: {key:?}"
        );
        assert!(title.is_none(), "no title set on the hand-inserted row");

        let msg_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = 'row-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(msg_count, 1, "messages table is untouched");
    }
}
