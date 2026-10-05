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

    /// How long an untouched conversation is kept on disk.
    ///
    /// Rows accumulated for every chat that ever messaged the bot and were never
    /// removed. Not a config key: there is no concrete reason yet for operators
    /// to vary it, and §3.2 says not to add the knob until there is.
    const RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

    /// One-time maintenance at startup: drop rows the runtime can no longer
    /// address, and rows nothing has touched inside the retention window.
    ///
    /// **Legacy keys.** History used to be keyed `channel_sender`, which merged
    /// one person's DM with every group they shared with the bot. It is now
    /// keyed by the conversation (`surface:chat[:thread]`), so every live key
    /// contains a `:` and no legacy key does. Those rows are unreachable under
    /// the new scheme — leaving them would keep leaked cross-chat transcripts on
    /// disk while nothing could ever read or prune them. A one-time reset of
    /// channel history is the honest outcome; a silent orphan is not.
    ///
    /// Returns `(legacy_removed, expired_removed)`.
    pub fn prune_at_startup(&self) -> anyhow::Result<(usize, usize)> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let cutoff = now.saturating_sub(Self::RETENTION_SECS);

        let conn = self.conn.lock();
        let legacy = conn.execute(
            "DELETE FROM channel_history WHERE instr(history_key, ':') = 0",
            [],
        )?;
        // `updated_at` has existed since the table was created and was read by
        // nothing; this is what it was for.
        let expired = conn.execute(
            "DELETE FROM channel_history WHERE updated_at > 0 AND updated_at < ?1",
            params![cutoff],
        )?;
        drop(conn);

        if legacy > 0 {
            tracing::info!(
                rows = legacy,
                "Removed channel history rows keyed by the pre-conversation-scope \
                 scheme; those conversations start fresh"
            );
        }
        if expired > 0 {
            tracing::info!(
                rows = expired,
                retention_days = Self::RETENTION_SECS / (24 * 60 * 60),
                "Pruned channel history rows untouched inside the retention window"
            );
        }

        Ok((legacy, expired))
    }

    /// Load every persisted conversation into a map keyed by history key.
    ///
    /// Rows whose `turns_json` fails to deserialize are skipped (with a warning)
    /// rather than aborting the whole load, so one corrupt row can't wipe the
    /// rest of the live state.
    ///
    /// Each loaded turn is walked for shapes the runtime used to leave in
    /// persisted history but no longer does:
    ///
    /// - the runtime's own `[Used tools: …]` label on an assistant turn — a
    ///   pattern the next turn's provider would read straight back as a way
    ///   to fake tool work;
    /// - native `role = "tool"` rows, assistant tool-call carrier rows
    ///   (XML `<tool_call>` or native JSON with `"tool_calls":`), and XML
    ///   `[Tool results]` user rows — rows that, in a shared chat, belong to
    ///   another sender's turn and would leak one sender's tool output into
    ///   the next sender's prompt.
    ///
    /// Stripping happens at load so a daemon upgrade doesn't ship stale
    /// forgery patterns or cross-sender leaks into the live cache. The strip is
    /// keyed on the row shape, not the chat kind, so it fires for an owner's
    /// direct chat too: a persisted key is a chat, and nothing in it records
    /// whether that chat was shared or direct. That is accepted because only
    /// unreleased builds ever wrote these rows — an owner's DM loses its own
    /// pre-fix structured tool rows on restart, the documented cost of not
    /// being able to tell a shared chat from a DM in a persisted row.
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
                    let cleaned: Vec<ChatMessage> = turns
                        .into_iter()
                        .filter_map(|mut turn| {
                            if turn.role == "tool" {
                                // Native tool result from an older build that
                                // stored structured rows. The provider expects
                                // its preceding call row to be present too;
                                // without that pairing, the row is unsafe to
                                // ship forward.
                                return None;
                            }
                            if turn.role == "assistant"
                                && crate::channels::dispatch::is_tool_call_carrier(&turn.content)
                            {
                                // XML `<tool_call>…</tool_call>` carrier or
                                // native `{"content":...,"tool_calls":...}`
                                // carrier. The runtime only stores one of
                                // these in an `All`-chat owner-DM; on every
                                // other path a carrier that survived a restart
                                // is a row from a previous shape.
                                return None;
                            }
                            if turn.role == "user"
                                && (turn.content.starts_with("[Tool results]")
                                    || turn.content.starts_with("[Tool Results]"))
                            {
                                // XML `[Tool results]` carrier — the dispatcher
                                // flattens `ToolResults` into a single `user`
                                // row in the XML path. Surviving a restart, it
                                // needs its call row to make sense, and that
                                // row was already filtered above.
                                return None;
                            }
                            if turn.role == "assistant" {
                                turn.content = strip_legacy_tool_label(&turn.content);
                            }
                            Some(turn)
                        })
                        .collect();
                    out.insert(key, cleaned);
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
    ///
    /// The whole turn list is rewritten on each call, which is bounded rather
    /// than unbounded: `append_sender_turn` trims to `MAX_CHANNEL_HISTORY`
    /// before persisting, so a write is at most that many turns however long the
    /// conversation runs. A second cap here would be duplicated policy with no
    /// current caller needing it (§3.2), so the growth this store actually had —
    /// rows accumulating forever — is handled by `prune_at_startup` instead.
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

/// Strip a leading `[Used tools: …]` label the runtime used to write into
/// persisted assistant turns, plus its trailing newline if any. The label is
/// on its own line by the runtime's old contract, so a leading-only scan is
/// enough and a global scan would also edit any prose the model happened to
/// write with that exact shape (the runtime's vocabulary, not the model's).
/// Returns the original string unchanged when no leading label is found.
fn strip_legacy_tool_label(content: &str) -> String {
    let trimmed_start = content.trim_start();
    if !trimmed_start.starts_with("[Used tools:") {
        return content.to_string();
    }
    match trimmed_start.find('\n') {
        Some(nl) => trimmed_start[nl + 1..].to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Rows keyed by the pre-conversation-scope `channel_sender` shape are
    /// unreachable once history is keyed by the conversation, and they hold the
    /// cross-chat transcripts that scheme produced. They must not be left to rot.
    #[test]
    fn legacy_keyed_rows_are_removed_at_startup() {
        let tmp = TempDir::new().unwrap();
        let store = ChannelHistoryStore::open(tmp.path()).unwrap();
        store
            .save("telegram_alice", &[ChatMessage::user("leaked DM turn")])
            .unwrap();
        store
            .save("telegram:chat-1", &[ChatMessage::user("kept")])
            .unwrap();

        let (legacy, _) = store.prune_at_startup().unwrap();
        assert_eq!(legacy, 1, "exactly the legacy-shaped row");

        let loaded = store.load_all().unwrap();
        assert!(
            !loaded.contains_key("telegram_alice"),
            "the unreachable row is gone"
        );
        assert!(
            loaded.contains_key("telegram:chat-1"),
            "conversation-scoped rows are untouched"
        );
    }

    /// Rows accumulated for every chat that ever messaged the bot and were never
    /// removed. `updated_at` existed for this and was read by nothing.
    #[test]
    fn rows_past_the_retention_window_are_pruned() {
        let tmp = TempDir::new().unwrap();
        let store = ChannelHistoryStore::open(tmp.path()).unwrap();
        store
            .save("telegram:chat-old", &[ChatMessage::user("ancient")])
            .unwrap();
        store
            .save("telegram:chat-new", &[ChatMessage::user("recent")])
            .unwrap();

        // Age one row past the window without touching the other.
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE channel_history SET updated_at = 1 WHERE history_key = ?1",
                params!["telegram:chat-old"],
            )
            .unwrap();
        }

        let (_, expired) = store.prune_at_startup().unwrap();
        assert_eq!(expired, 1, "only the aged row");

        let loaded = store.load_all().unwrap();
        assert!(!loaded.contains_key("telegram:chat-old"));
        assert!(
            loaded.contains_key("telegram:chat-new"),
            "a live conversation must survive maintenance"
        );
    }

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

    /// Rows persisted by an older build carry the runtime's own `[Used tools: …]`
    /// label as the assistant turn. The runtime no longer writes that, but the
    /// next turn's provider would read it back as a pattern to copy. Strip it
    /// on load so an upgrade doesn't ship stale forgery patterns into the model.
    #[test]
    fn load_all_strips_legacy_assistant_authored_labels() {
        let tmp = TempDir::new().unwrap();
        let store = ChannelHistoryStore::open(tmp.path()).unwrap();
        store
            .save(
                "telegram:chat-1",
                &[
                    ChatMessage::user("save it"),
                    ChatMessage::assistant("[Used tools: memory_store]\nSaved your note."),
                    ChatMessage::user("what was last saved?"),
                ],
            )
            .unwrap();

        let loaded = store.load_all().unwrap();
        let turns = loaded
            .get("telegram:chat-1")
            .expect("persisted conversation");
        assert_eq!(turns.len(), 3);
        for turn in turns {
            assert!(
                !turn.content.contains("[Used tools:"),
                "no loaded assistant turn may carry the legacy label: {turn:?}"
            );
        }
        assert!(
            turns[1].content.contains("Saved your note."),
            "the reply text survives the strip: {turns:?}"
        );
    }

    /// Rows persisted by older builds hold the structured tool-call and tool-
    /// result shapes the dispatcher used to write: a native `role = "tool"`
    /// row, an XML `[Tool results]` user row, and assistant tool-call carrier
    /// rows in either XML or native JSON form. Loading them through the
    /// runtime's store and then driving a fresh turn would hand the next
    /// turn's provider a transcript whose tool output a different sender
    /// could read in a shared chat. Drop them on load so an upgrade starts
    /// clean.
    #[test]
    fn load_all_strips_legacy_tool_rows_and_carriers() {
        let tmp = TempDir::new().unwrap();
        let store = ChannelHistoryStore::open(tmp.path()).unwrap();
        let native_tool_row = serde_json::json!({
            "tool_call_id": "call-x",
            "content": "owner-only body",
        })
        .to_string();
        store
            .save(
                "telegram:chat-shared",
                &[
                    ChatMessage::user("q1"),
                    ChatMessage::assistant("<tool_call>{\"name\":\"x\"}</tool_call>"),
                    ChatMessage::tool(native_tool_row),
                    ChatMessage::assistant("reply"),
                    ChatMessage::user(
                        "[Tool results]\n<tool_result name=\"x\">\nfoo\n</tool_result>",
                    ),
                ],
            )
            .unwrap();
        store
            .save(
                "telegram:chat-dm",
                &[
                    ChatMessage::user("q2"),
                    ChatMessage::assistant(
                        "{\"content\":\"\",\"tool_calls\":[{\"name\":\"y\"}]}".to_string(),
                    ),
                    ChatMessage::tool(
                        "{\"tool_call_id\":\"call-y\",\"content\":\"ok\"}".to_string(),
                    ),
                    ChatMessage::assistant("reply2"),
                ],
            )
            .unwrap();

        let loaded = store.load_all().unwrap();
        let shared = loaded
            .get("telegram:chat-shared")
            .expect("shared chat history");
        assert_eq!(
            shared.len(),
            2,
            "user q1 and plain assistant reply survive; carrier + tool + XML results row are stripped: {shared:?}"
        );
        assert_eq!(shared[0].role, "user");
        assert_eq!(shared[0].content, "q1");
        assert_eq!(shared[1].role, "assistant");
        assert_eq!(shared[1].content, "reply");
        assert!(
            !shared.iter().any(|t| t.content.contains("owner-only body")),
            "the tool body must not survive a load: {shared:?}"
        );
        assert!(
            !shared
                .iter()
                .any(|t| t.content.contains("<tool_call>") || t.content.contains("[Tool results]")),
            "no carrier or results row survives a load: {shared:?}"
        );
        let dm = loaded.get("telegram:chat-dm").expect("dm chat history");
        assert!(
            dm.iter()
                .all(|t| !t.content.contains("tool_calls") && !t.content.contains("call-y")),
            "no native carrier or tool row survives: {dm:?}"
        );
        assert!(
            dm.iter()
                .any(|t| t.role == "assistant" && t.content == "reply2"),
            "the plain prose assistant turn survives a load"
        );
    }
}
