use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use uuid::Uuid;

use super::migrations::run_migrations;
use super::types::{Message, SearchResult, Session, SessionMeta};

/// Maximum displayable length of an auto-derived session title.
const MAX_AUTO_TITLE_CHARS: usize = 50;

/// Derive a session title from a user message: pick the first non-empty
/// line, collapse whitespace, truncate to `MAX_AUTO_TITLE_CHARS` chars,
/// and append `…` when truncated. Returns an empty string for content
/// that has no usable text.
pub fn derive_session_title(content: &str) -> String {
    let first_line = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let collapsed = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = collapsed.chars().count();
    if count <= MAX_AUTO_TITLE_CHARS {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(MAX_AUTO_TITLE_CHARS).collect();
        format!("{truncated}…")
    }
}

/// Persistent store for TUI sessions and messages backed by SQLite.
pub struct SessionStore {
    conn: Connection,
}

impl SessionStore {
    /// Open (or create) a file-based SQLite database at `path`.
    ///
    /// Enables WAL journal mode and foreign-key enforcement, then runs
    /// pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open session db at {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        run_migrations(&conn)?;

        Ok(Self { conn })
    }

    /// Open an in-memory SQLite database (useful for testing).
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("failed to open in-memory session db")?;

        run_migrations(&conn)?;

        Ok(Self { conn })
    }

    /// Create a new session with a generated UUID.
    pub fn new_session(&self, model: &str, source: &str) -> Result<Session> {
        let id = Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now().timestamp();

        self.conn.execute(
            "INSERT INTO sessions (id, model, started_at, source) VALUES (?1, ?2, ?3, ?4)",
            params![id, model, started_at, source],
        )?;

        Ok(Session {
            id,
            title: None,
            parent_session_id: None,
            model: model.to_string(),
            started_at,
            ended_at: None,
            message_count: 0,
            token_count: 0,
            source: source.to_string(),
        })
    }

    /// Retrieve a session by its ID, returning `None` if not found.
    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, parent_session_id, model, started_at, ended_at, \
             message_count, token_count, source FROM sessions WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id], |row| {
            Ok(Session {
                id: row.get(0)?,
                title: row.get(1)?,
                parent_session_id: row.get(2)?,
                model: row.get(3)?,
                started_at: row.get(4)?,
                ended_at: row.get(5)?,
                message_count: row.get(6)?,
                token_count: row.get(7)?,
                source: row.get(8)?,
            })
        });

        match result {
            Ok(session) => Ok(Some(session)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Set the `ended_at` timestamp on a session to mark it as finished.
    pub fn end_session(&self, id: &str) -> Result<()> {
        let ended_at = chrono::Utc::now().timestamp();
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
            params![ended_at, id],
        )?;
        Ok(())
    }

    /// Update the human-readable title of a session.
    pub fn set_title(&self, id: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = ?1 WHERE id = ?2",
            params![title, id],
        )?;
        Ok(())
    }

    /// One-shot backfill: for every session whose title is NULL or empty,
    /// derive a title from the earliest user message (first 50 chars of
    /// the first non-empty line, whitespace collapsed). Sessions with no
    /// user messages are left untitled. Idempotent — re-running it on a
    /// store with no untitled sessions is a no-op.
    ///
    /// Returns the number of rows updated.
    pub fn backfill_titles(&self) -> Result<usize> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, m.content
             FROM sessions s
             JOIN messages m ON m.session_id = s.id AND m.role = 'user'
             WHERE (s.title IS NULL OR s.title = '')
             AND m.id = (
                 SELECT MIN(id) FROM messages
                 WHERE session_id = s.id AND role = 'user'
             )",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .filter_map(Result::ok)
            .collect();

        let mut updated = 0;
        for (id, content) in rows {
            let title = derive_session_title(&content);
            if title.is_empty() {
                continue;
            }
            self.conn.execute(
                "UPDATE sessions SET title = ?1 WHERE id = ?2",
                params![title, id],
            )?;
            updated += 1;
        }
        Ok(updated)
    }

    /// Insert a message into the store and increment the session's `message_count`.
    ///
    /// Returns the assigned row ID of the new message.
    pub fn append_message(&self, msg: &Message) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls, timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                msg.session_id,
                msg.role,
                msg.content,
                msg.tool_calls,
                msg.timestamp
            ],
        )?;

        let row_id = self.conn.last_insert_rowid();

        self.conn.execute(
            "UPDATE sessions SET message_count = message_count + 1 WHERE id = ?1",
            params![msg.session_id],
        )?;

        Ok(row_id)
    }

    /// Replace every message in a session with a fresh list. Used by
    /// context compaction (`/compress`) so the on-disk history matches
    /// the in-memory `[summary, ...recent]` shape after older turns
    /// have been folded into a summary.
    ///
    /// Atomically deletes the existing rows + inserts the new set + sets
    /// `message_count` to match. `session_id` on each input `Message` is
    /// rewritten so callers don't have to thread it through.
    pub fn replace_messages(&mut self, session_id: &str, messages: &[Message]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )?;
        for msg in messages {
            tx.execute(
                "INSERT INTO messages (session_id, role, content, tool_calls, timestamp) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id,
                    msg.role,
                    msg.content,
                    msg.tool_calls,
                    msg.timestamp
                ],
            )?;
        }
        tx.execute(
            "UPDATE sessions SET message_count = ?1 WHERE id = ?2",
            params![messages.len() as i64, session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Retrieve all messages for a session, ordered by timestamp ascending.
    pub fn get_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, content, tool_calls, timestamp \
             FROM messages WHERE session_id = ?1 ORDER BY timestamp ASC",
        )?;

        let messages = stmt
            .query_map(params![session_id], |row| {
                Ok(Message {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    tool_calls: row.get(4)?,
                    timestamp: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(messages)
    }

    /// List recent sessions ordered by `started_at` descending.
    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model, started_at, message_count \
             FROM sessions ORDER BY started_at DESC LIMIT ?1",
        )?;

        let sessions = stmt
            .query_map(params![limit as i64], |row| {
                Ok(SessionMeta {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    model: row.get(2)?,
                    started_at: row.get(3)?,
                    message_count: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(sessions)
    }

    /// Full-text search across message content using FTS5, ranked by BM25.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.session_id, s.title, m.id, m.role, m.content, m.timestamp, \
             bm25(messages_fts) as rank \
             FROM messages_fts \
             JOIN messages m ON messages_fts.rowid = m.id \
             JOIN sessions s ON m.session_id = s.id \
             WHERE messages_fts MATCH ?1 \
             ORDER BY rank \
             LIMIT ?2",
        )?;

        let results = stmt
            .query_map(params![query, limit as i64], |row| {
                Ok(SearchResult {
                    session_id: row.get(0)?,
                    session_title: row.get(1)?,
                    message_id: row.get(2)?,
                    role: row.get(3)?,
                    content: row.get(4)?,
                    timestamp: row.get(5)?,
                    rank: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(results)
    }

    /// End the current session and create a new linked session with a summary
    /// system message.
    ///
    /// The new session's `parent_session_id` is set to `session_id`, and
    /// `summary` is inserted as the first message with role `"system"`.
    pub fn split_session(&self, session_id: &str, summary: &str, model: &str) -> Result<Session> {
        self.end_session(session_id)?;

        let source = self
            .get_session(session_id)?
            .map(|s| s.source)
            .unwrap_or_else(|| "tui".to_string());

        let new_id = Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now().timestamp();

        self.conn.execute(
            "INSERT INTO sessions (id, parent_session_id, model, started_at, source) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![new_id, session_id, model, started_at, source],
        )?;

        let summary_msg = Message {
            id: 0,
            session_id: new_id.clone(),
            role: "system".to_string(),
            content: summary.to_string(),
            tool_calls: None,
            timestamp: started_at,
        };
        self.append_message(&summary_msg)?;

        Ok(Session {
            id: new_id,
            title: None,
            parent_session_id: Some(session_id.to_string()),
            model: model.to_string(),
            started_at,
            ended_at: None,
            message_count: 1,
            token_count: 0,
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SessionStore {
        SessionStore::in_memory().expect("in-memory store")
    }

    #[test]
    fn new_session_creates_session_with_uuid() {
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();

        assert!(!sess.id.is_empty());
        // A valid UUID v4 has 36 chars with hyphens
        assert_eq!(sess.id.len(), 36);
        assert_eq!(sess.model, "gpt-4o");
        assert_eq!(sess.source, "tui");
        assert_eq!(sess.message_count, 0);
    }

    #[test]
    fn get_session_returns_none_for_nonexistent() {
        let s = store();
        let result = s.get_session("no-such-id").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn append_and_get_messages_roundtrip() {
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();

        let msg = Message::user(&sess.id, "hello world");
        let row_id = s.append_message(&msg).unwrap();
        assert!(row_id > 0);

        let msgs = s.get_messages(&sess.id).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "hello world");
        assert_eq!(msgs[0].session_id, sess.id);
    }

    #[test]
    fn list_sessions_returns_most_recent_first() {
        let s = store();

        // Insert sessions with distinct timestamps by manipulating directly
        let id_a = Uuid::new_v4().to_string();
        let id_b = Uuid::new_v4().to_string();

        s.conn
            .execute(
                "INSERT INTO sessions (id, model, started_at, source) VALUES (?1, 'gpt-4o', 100, 'tui')",
                params![id_a],
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO sessions (id, model, started_at, source) VALUES (?1, 'gpt-4o', 200, 'tui')",
                params![id_b],
            )
            .unwrap();

        let list = s.list_sessions(10).unwrap();
        assert_eq!(list.len(), 2);
        // Most recent first: started_at 200 before 100
        assert_eq!(list[0].id, id_b);
        assert_eq!(list[1].id, id_a);
    }

    #[test]
    fn search_finds_messages_by_content() {
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();

        s.append_message(&Message::user(&sess.id, "the quick brown fox"))
            .unwrap();
        s.append_message(&Message::user(&sess.id, "an unrelated message"))
            .unwrap();

        let results = s.search("quick", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("quick"));
    }

    #[test]
    fn set_title_updates_session() {
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();

        s.set_title(&sess.id, "My conversation").unwrap();

        let updated = s.get_session(&sess.id).unwrap().unwrap();
        assert_eq!(updated.title.as_deref(), Some("My conversation"));
    }

    #[test]
    fn split_session_creates_linked_session() {
        let s = store();
        let parent = s.new_session("gpt-4o", "tui").unwrap();

        let child = s
            .split_session(&parent.id, "context summary", "gpt-4o")
            .unwrap();

        // Parent session should now be ended
        let parent_updated = s.get_session(&parent.id).unwrap().unwrap();
        assert!(parent_updated.ended_at.is_some());

        // Child links back to parent
        assert_eq!(child.parent_session_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.model, "gpt-4o");

        // Child has the summary as its first message
        let msgs = s.get_messages(&child.id).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "context summary");
    }

    #[test]
    fn end_session_sets_ended_at() {
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();
        assert!(sess.ended_at.is_none());

        s.end_session(&sess.id).unwrap();

        let updated = s.get_session(&sess.id).unwrap().unwrap();
        assert!(updated.ended_at.is_some());
    }

    #[test]
    fn derive_session_title_collapses_and_truncates() {
        assert_eq!(derive_session_title(""), "");
        assert_eq!(derive_session_title("\n\n  \n"), "");
        assert_eq!(derive_session_title("hello world"), "hello world");
        assert_eq!(
            derive_session_title("  hello   world  \nsecond line"),
            "hello world"
        );
        let long = "a".repeat(80);
        let result = derive_session_title(&long);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), MAX_AUTO_TITLE_CHARS + 1);
    }

    #[test]
    fn backfill_titles_sets_titles_for_untitled_sessions() {
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();
        assert!(sess.title.is_none());

        s.append_message(&Message::user(&sess.id, "fix the bug in payments"))
            .unwrap();
        s.append_message(&Message::assistant(&sess.id, "ok let me look"))
            .unwrap();
        // A second user message — backfill should pick the FIRST one.
        s.append_message(&Message::user(&sess.id, "actually nevermind"))
            .unwrap();

        let updated = s.backfill_titles().unwrap();
        assert_eq!(updated, 1);

        let after = s.get_session(&sess.id).unwrap().unwrap();
        assert_eq!(after.title.as_deref(), Some("fix the bug in payments"));

        // Idempotent — second call updates nothing.
        let again = s.backfill_titles().unwrap();
        assert_eq!(again, 0);
    }

    #[test]
    fn backfill_skips_sessions_with_no_user_message() {
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();
        s.append_message(&Message::assistant(&sess.id, "hi there"))
            .unwrap();

        let updated = s.backfill_titles().unwrap();
        assert_eq!(updated, 0);

        let after = s.get_session(&sess.id).unwrap().unwrap();
        assert!(after.title.is_none());
    }
}
