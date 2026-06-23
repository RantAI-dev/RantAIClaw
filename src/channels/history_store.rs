//! Durable persistence for per-sender channel conversation history.
//!
//! Channel conversation history (the running user+assistant turns per sender)
//! normally lives only in the in-memory map owned by [`super::ChannelRuntimeContext`].
//! That map is rebuilt empty on every daemon boot, so a restart silently wipes
//! every live thread. [`ChannelHistoryStore`] persists that map into the same
//! sqlite `brain.db` the memory backend uses, and reloads it at startup so
//! conversations survive restarts.
//!
//! The store owns its own [`rusqlite::Connection`] to the shared `brain.db`.
//! Because `brain.db` runs in WAL mode, a second connection is safe as long as
//! `busy_timeout` is set so concurrent writes with the memory backend retry
//! instead of erroring with "database is locked".

use crate::providers::ChatMessage;
use anyhow::Context;
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Persists per-sender channel conversation history to the shared `brain.db`.
///
/// Keyed by the same `"{channel}_{sender}"` history key used by the in-memory
/// map, so loading at startup seeds the live map transparently.
pub struct ChannelHistoryStore {
    conn: Mutex<Connection>,
}

impl ChannelHistoryStore {
    /// Open (or create) the channel-history table in the workspace `brain.db`.
    ///
    /// Uses the exact same db file as the sqlite memory backend
    /// (`<workspace_dir>/memory/brain.db`) and sets `busy_timeout` so writes
    /// coordinate with the memory backend's connection.
    pub fn open(workspace_dir: &Path) -> anyhow::Result<Self> {
        let db_path = workspace_dir.join("memory").join("brain.db");

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create memory dir for {}", parent.display()))?;
        }

        let conn = Connection::open(&db_path).with_context(|| {
            format!("failed to open channel history db at {}", db_path.display())
        })?;

        // busy_timeout is REQUIRED: brain.db is shared with the memory backend's
        // own connection, so concurrent writers must retry instead of failing
        // with "database is locked".
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous  = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS channel_history (
                history_key TEXT PRIMARY KEY,
                turns_json  TEXT NOT NULL,
                updated_at  INTEGER NOT NULL DEFAULT 0
            );",
        )?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Load every persisted conversation into a map keyed by history key.
    ///
    /// Rows whose `turns_json` fails to deserialize are skipped (with a warning)
    /// rather than aborting the whole load, so one corrupt row can't wipe the
    /// rest of the live state.
    pub fn load_all(&self) -> anyhow::Result<HashMap<String, Vec<ChatMessage>>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT history_key, turns_json FROM channel_history")?;
        let rows = stmt.query_map([], |row| {
            let key: String = row.get(0)?;
            let json: String = row.get(1)?;
            Ok((key, json))
        })?;

        let mut out: HashMap<String, Vec<ChatMessage>> = HashMap::new();
        for row in rows {
            let (key, json) = row?;
            match serde_json::from_str::<Vec<ChatMessage>>(&json) {
                Ok(turns) => {
                    out.insert(key, turns);
                }
                Err(e) => {
                    tracing::warn!(
                        history_key = %key,
                        error = %e,
                        "skipping channel history row that failed to deserialize"
                    );
                }
            }
        }

        Ok(out)
    }

    /// Persist the turns for one history key (upsert).
    ///
    /// An empty `turns` slice deletes the row instead of storing an empty entry,
    /// keeping the table free of dead keys.
    pub fn save(&self, history_key: &str, turns: &[ChatMessage]) -> anyhow::Result<()> {
        if turns.is_empty() {
            return self.delete(history_key);
        }

        let json = serde_json::to_string(turns)?;
        let updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO channel_history(history_key, turns_json, updated_at)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(history_key) DO UPDATE SET
                 turns_json = excluded.turns_json,
                 updated_at = excluded.updated_at",
            params![history_key, json, updated_at],
        )?;

        Ok(())
    }

    /// Remove the persisted turns for one history key.
    pub fn delete(&self, history_key: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM channel_history WHERE history_key = ?1",
            params![history_key],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn roundtrip_persists_across_reopen() {
        let tmp = TempDir::new().unwrap();
        {
            let store = ChannelHistoryStore::open(tmp.path()).unwrap();
            store
                .save(
                    "telegram_123",
                    &[ChatMessage::user("hi"), ChatMessage::assistant("hello")],
                )
                .unwrap();
        }

        // Fresh store (simulates daemon restart) sees the persisted turns.
        let store2 = ChannelHistoryStore::open(tmp.path()).unwrap();
        let loaded = store2.load_all().unwrap();
        let turns = loaded
            .get("telegram_123")
            .expect("key present after reopen");
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].content, "hi");
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].content, "hello");
    }

    #[test]
    fn delete_removes_key() {
        let tmp = TempDir::new().unwrap();
        let store = ChannelHistoryStore::open(tmp.path()).unwrap();
        store
            .save("telegram_123", &[ChatMessage::user("hi")])
            .unwrap();
        store.delete("telegram_123").unwrap();

        let loaded = store.load_all().unwrap();
        assert!(!loaded.contains_key("telegram_123"));
    }

    #[test]
    fn save_empty_slice_stores_nothing() {
        let tmp = TempDir::new().unwrap();
        let store = ChannelHistoryStore::open(tmp.path()).unwrap();
        store.save("telegram_123", &[]).unwrap();

        let loaded = store.load_all().unwrap();
        assert!(loaded.is_empty());

        // Saving empty over an existing row deletes it.
        store
            .save("telegram_123", &[ChatMessage::user("hi")])
            .unwrap();
        store.save("telegram_123", &[]).unwrap();
        let loaded = store.load_all().unwrap();
        assert!(!loaded.contains_key("telegram_123"));
    }

    #[test]
    fn load_all_returns_multiple_keys() {
        let tmp = TempDir::new().unwrap();
        let store = ChannelHistoryStore::open(tmp.path()).unwrap();
        store
            .save("telegram_123", &[ChatMessage::user("a")])
            .unwrap();
        store
            .save(
                "discord_456",
                &[ChatMessage::user("b"), ChatMessage::assistant("c")],
            )
            .unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("telegram_123").unwrap().len(), 1);
        assert_eq!(loaded.get("discord_456").unwrap().len(), 2);
    }
}
