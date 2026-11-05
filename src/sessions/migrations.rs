use anyhow::Result;
use rusqlite::Connection;

pub const CURRENT_VERSION: i32 = 3;

pub fn run_migrations(conn: &Connection) -> Result<()> {
    let version = get_schema_version(conn)?;

    if version < 1 {
        migrate_v1(conn)?;
    }

    if version < 2 {
        migrate_v2(conn)?;
    }

    if version < 3 {
        migrate_v3(conn)?;
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

/// v2 → v3: a fork outlives the session it was branched from.
///
/// v1 declared `parent_session_id` with a bare `FOREIGN KEY ...
/// REFERENCES sessions(id)` and no `ON DELETE` action. Deleting a session
/// that had forks therefore failed with "FOREIGN KEY constraint failed" —
/// the fork's reference blocked the delete. SQLite cannot alter a foreign
/// key, so the `sessions` table is rebuilt with `ON DELETE SET NULL`:
/// deleting the parent clears the fork's `parent_session_id`, and the fork
/// keeps every message it had.
///
/// The rebuild follows SQLite's documented table-rebuild procedure: foreign
/// keys are turned off on the outer connection (the pragma is a no-op inside
/// a transaction), the new table is created and filled, the old one is
/// dropped, the new one is renamed into place, the indexes are recreated and
/// `PRAGMA foreign_key_check` runs before the commit. `messages` keeps its
/// own foreign key to `sessions` — the rename does not rewrite it while
/// foreign keys are off — and the FTS objects sit on `messages` and stay as
/// they are.
fn migrate_v3(conn: &Connection) -> Result<()> {
    // `PRAGMA foreign_keys` cannot be changed inside a transaction, so the
    // enforcement state is read before the rebuild and put back afterwards.
    // The file-backed store enables it (`SessionStore::open`), the in-memory
    // store never did — putting back what was there keeps the in-memory
    // store's behaviour unchanged.
    let foreign_keys_were_on: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    conn.execute_batch("PRAGMA foreign_keys=OFF;")?;

    let rebuilt = rebuild_sessions_for_v3(conn);

    // Put the enforcement state back even when the rebuild failed, so a
    // failed migration never leaves the connection without foreign keys.
    conn.execute_batch(if foreign_keys_were_on == 1 {
        "PRAGMA foreign_keys=ON;"
    } else {
        "PRAGMA foreign_keys=OFF;"
    })?;

    rebuilt
}

/// The v2 → v3 rebuild of the `sessions` table. One transaction: the new
/// table, the copy, the swap, the indexes, the foreign key check and the
/// version stamp either all land or none of them do.
fn rebuild_sessions_for_v3(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;

    tx.execute_batch(
        r#"
        CREATE TABLE sessions_new (
            id TEXT PRIMARY KEY,
            title TEXT,
            parent_session_id TEXT,
            model TEXT NOT NULL,
            started_at INTEGER NOT NULL,
            ended_at INTEGER,
            message_count INTEGER DEFAULT 0,
            token_count INTEGER DEFAULT 0,
            source TEXT DEFAULT 'tui',
            conversation_key TEXT,
            FOREIGN KEY (parent_session_id) REFERENCES sessions(id) ON DELETE SET NULL
        );

        INSERT INTO sessions_new (
            id, title, parent_session_id, model, started_at, ended_at,
            message_count, token_count, source, conversation_key
        )
        SELECT id, title, parent_session_id, model, started_at, ended_at,
               message_count, token_count, source, conversation_key
        FROM sessions;

        DROP TABLE sessions;

        ALTER TABLE sessions_new RENAME TO sessions;

        CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);

        CREATE INDEX IF NOT EXISTS idx_sessions_conversation_key
            ON sessions(conversation_key, started_at DESC);
        "#,
    )?;

    // The rebuild must not leave a dangling reference behind: a fork whose
    // parent is gone would keep pointing at a row that no longer exists.
    // The check runs before the commit, so a database that cannot be
    // migrated cleanly is reported instead of migrated.
    let violations: i64 =
        tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations > 0 {
        anyhow::bail!("the v3 sessions rebuild leaves {violations} dangling reference(s)");
    }

    tx.execute("DELETE FROM schema_version", [])?;
    // Stamp the literal 3 so a later v4 cannot accidentally push the row to
    // its own value before the v4 changes have run. `CURRENT_VERSION` is the
    // current head; the per-migration stamp must always be the version that
    // migration introduces.
    tx.execute("INSERT INTO schema_version (version) VALUES (?1)", [3])?;

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
    /// so the v1 → v3 schema bump (now run to the current head) must not
    /// race against it.
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
        // migration to the current head (the v2 step's ALTER TABLE +
        // CREATE INDEX among them) must succeed while the reader's
        // transaction is still live.
        let writer = Connection::open(&db_path).expect("open writer");
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
            )
            .expect("writer pragmas");
        run_migrations(&writer)
            .expect("migration to the current head must run while a reader holds the database");

        // The reader's transaction is still alive: the row survives the
        // migration. Committing the transaction lets the file finalise so
        // the post-migration schema checks can read it cleanly.
        let mid: i64 = reader_tx
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .expect("reader mid-migration count");
        assert_eq!(
            mid, 1,
            "row survives the migration to the current head inside the held reader transaction"
        );
        reader_tx.commit().expect("commit reader tx");

        // Schema is at the current head (v3) with v2's new column and index
        // present on the writer (the connection that performed the migration).
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
        // The literal 2, not `CURRENT_VERSION`: this migration stamps the
        // version it introduces, and `CURRENT_VERSION` is the head of the
        // chain (v3 by now).
        assert_eq!(version, 2, "the v2 migration stamps version 2");

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

    /// A v2 `sessions.db` that already holds forks must survive the v2 → v3
    /// migration with every row and every message intact, and deleting the
    /// parent afterwards must succeed.
    ///
    /// This is the test the `ON DELETE SET NULL` clause exists for. With the
    /// old bare `FOREIGN KEY (parent_session_id) REFERENCES sessions(id)` the
    /// delete of a parent that has forks fails with "FOREIGN KEY constraint
    /// failed", so the operator cannot remove a branched session at all.
    ///
    /// The database is file-backed on purpose: only a file-backed store turns
    /// `PRAGMA foreign_keys` on, and without the foreign key the delete
    /// succeeds even against the old schema — the defect would not show.
    #[test]
    fn migration_v2_to_v3_keeps_forks_and_allows_deleting_their_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.db");

        // Seed a v2 database by hand: the v1 schema, then the v2 migration,
        // with a parent, two forks of it and one unrelated session. Every
        // session carries a message, so a lost message is visible.
        {
            let seed = Connection::open(&db_path).expect("open seed");
            seed.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
            )
            .expect("seed pragmas");
            seed.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);",
            )
            .expect("schema_version table");
            migrate_v1(&seed).expect("seed v1 schema");
            migrate_v2(&seed).expect("seed v2 schema");

            for (id, parent, started_at) in [
                ("parent", None, 1_i64),
                ("fork-a", Some("parent"), 2),
                ("fork-b", Some("parent"), 3),
                ("other", None, 4),
            ] {
                match parent {
                    Some(parent_id) => seed
                        .execute(
                            "INSERT INTO sessions \
                             (id, title, parent_session_id, model, started_at, source) \
                             VALUES (?1, ?2, ?3, 'm', ?4, 'tui')",
                            rusqlite::params![id, format!("{id} title"), parent_id, started_at],
                        )
                        .expect("insert session"),
                    None => seed
                        .execute(
                            "INSERT INTO sessions (id, title, model, started_at, source) \
                             VALUES (?1, ?2, 'm', ?3, 'tui')",
                            rusqlite::params![id, format!("{id} title"), started_at],
                        )
                        .expect("insert session"),
                };
                seed.execute(
                    "INSERT INTO messages (session_id, role, content, timestamp) \
                     VALUES (?1, 'user', ?2, ?3)",
                    rusqlite::params![id, format!("{id} message"), started_at],
                )
                .expect("insert message");
            }
        }

        // Open the file the way the store does — that is the path that runs
        // the pending migration.
        let mut store = crate::sessions::SessionStore::open(&db_path).expect("open v2 store");

        // The schema is at v3 now.
        let check = Connection::open(&db_path).expect("open check");
        assert_eq!(
            get_schema_version(&check).expect("schema version"),
            3,
            "the migration brings a v2 database to v3"
        );

        // The rebuild must not have moved the `messages` table's own foreign
        // key: it still points at the table that is now called `sessions`.
        let messages_parent: String = check
            .query_row(
                "SELECT \"table\" FROM pragma_foreign_key_list('messages')",
                [],
                |row| row.get(0),
            )
            .expect("the messages foreign key");
        assert_eq!(
            messages_parent, "sessions",
            "messages still references the rebuilt sessions table"
        );

        // Every row and every message survived the rebuild, and the fork
        // links still point at the parent.
        for id in ["parent", "fork-a", "fork-b", "other"] {
            let session = store
                .get_session(id)
                .expect("get session")
                .unwrap_or_else(|| panic!("the {id} row survives the migration"));
            assert_eq!(
                session.title.as_deref(),
                Some(format!("{id} title").as_str()),
                "the {id} row keeps its title"
            );
            let messages = store.get_messages(id).expect("messages");
            assert_eq!(messages.len(), 1, "the {id} message survives");
            assert_eq!(messages[0].content, format!("{id} message"));
        }
        for id in ["fork-a", "fork-b"] {
            let session = store
                .get_session(id)
                .expect("get session")
                .expect("fork row");
            assert_eq!(
                session.parent_session_id.as_deref(),
                Some("parent"),
                "the {id} link to its parent survives the migration"
            );
        }

        // The delete that used to fail with "FOREIGN KEY constraint failed".
        assert!(
            store.delete_session("parent").expect("delete the parent"),
            "the parent row is removed"
        );

        // The forks outlive the parent: they keep their messages and become
        // top-level sessions.
        for id in ["fork-a", "fork-b"] {
            let session = store
                .get_session(id)
                .expect("get session")
                .unwrap_or_else(|| panic!("the {id} fork survives the delete"));
            assert!(
                session.parent_session_id.is_none(),
                "the {id} fork becomes a top-level session: {session:?}"
            );
            let messages = store.get_messages(id).expect("messages");
            assert_eq!(messages.len(), 1, "the {id} fork keeps its message");
            assert_eq!(messages[0].content, format!("{id} message"));
        }

        // No other session's rows changed, and the parent's own messages went
        // with it.
        assert!(
            store.get_messages("parent").expect("messages").is_empty(),
            "the parent's messages go with the parent"
        );
        let other = store
            .get_session("other")
            .expect("get session")
            .expect("other row");
        assert!(other.parent_session_id.is_none());
        assert_eq!(other.title.as_deref(), Some("other title"));
        assert_eq!(store.get_messages("other").expect("messages").len(), 1);
    }

    /// The v3 rebuild has to turn foreign keys off to drop the old table, so
    /// it has to turn them back on afterwards. A store that lost its foreign
    /// key enforcement would let a session row point at a session that no
    /// longer exists.
    #[test]
    fn migration_v3_restores_foreign_key_enforcement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(dir.path().join("sessions.db")).expect("open");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
        )
        .expect("pragmas");

        run_migrations(&conn).expect("migrate");

        let version = get_schema_version(&conn).expect("schema version");
        assert_eq!(version, 3, "the migration ran");

        let foreign_keys: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("foreign key state");
        assert_eq!(
            foreign_keys, 1,
            "the migration must leave foreign key enforcement on"
        );
    }

    /// The v3 migration rebuilds the `sessions` table while another
    /// connection may still hold `sessions.db` open — the TUI and the
    /// channel recording layer keep a long-lived connection to that file.
    /// The rebuild must not race against it.
    ///
    /// The reader holds a transaction open across the migration, so the
    /// schema change happens under a live snapshot.
    #[test]
    fn migration_v3_runs_while_a_reader_holds_the_database_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("sessions.db");

        // A v2 database with a parent and a fork of it, plus a reader that
        // keeps a transaction open across the migration.
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
        migrate_v2(&reader).expect("seed v2 schema");
        reader
            .execute(
                "INSERT INTO sessions (id, model, started_at, source) \
                 VALUES ('reader-row', 'm', 1, 'tui')",
                [],
            )
            .expect("seed row");
        reader
            .execute(
                "INSERT INTO sessions (id, parent_session_id, model, started_at, source) \
                 VALUES ('reader-fork', 'reader-row', 'm', 2, 'tui')",
                [],
            )
            .expect("seed fork row");
        reader
            .execute(
                "INSERT INTO messages (session_id, role, content, timestamp) \
                 VALUES ('reader-row', 'user', 'hello', 1)",
                [],
            )
            .expect("seed message");

        let reader_tx = reader.unchecked_transaction().expect("begin reader tx");
        let pre: i64 = reader_tx
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .expect("reader pre-migration count");
        assert_eq!(
            pre, 2,
            "both rows are visible to the held reader transaction pre-migration"
        );

        // Second connection: open the file the way SessionStore::open does
        // (WAL + busy_timeout + foreign keys), then run_migrations. The v3
        // rebuild must succeed while the reader's transaction is still live.
        let writer = Connection::open(&db_path).expect("open writer");
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
            )
            .expect("writer pragmas");
        run_migrations(&writer)
            .expect("v3 migration must run while the reader connection holds sessions.db open");

        // The reader's transaction is still alive: its rows survive the
        // rebuild inside the held snapshot.
        let mid: i64 = reader_tx
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .expect("reader mid-migration count");
        assert_eq!(
            mid, 2,
            "the rows survive the v2→v3 rebuild inside the held reader transaction"
        );
        reader_tx.commit().expect("commit reader tx");

        // The schema is at v3 and the rebuild left no dangling reference.
        let version = get_schema_version(&writer).expect("schema version");
        assert_eq!(version, 3);

        let violations: i64 = writer
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .expect("foreign key check");
        assert_eq!(violations, 0, "the rebuild leaves no dangling reference");

        // The fork still points at its parent after the rebuild.
        let parent: Option<String> = writer
            .query_row(
                "SELECT parent_session_id FROM sessions WHERE id = 'reader-fork'",
                [],
                |r| r.get(0),
            )
            .expect("fork row");
        assert_eq!(parent.as_deref(), Some("reader-row"));
    }
}
