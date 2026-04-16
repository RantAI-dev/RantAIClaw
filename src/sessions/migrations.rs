use anyhow::Result;
use rusqlite::Connection;

const CURRENT_VERSION: i32 = 1;

pub fn run_migrations(conn: &Connection) -> Result<()> {
    let version = get_schema_version(conn)?;

    if version < 1 {
        migrate_v1(conn)?;
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
}
