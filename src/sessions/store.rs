use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use uuid::Uuid;

use super::migrations::run_migrations;
use super::types::{Message, SearchResult, Session, SessionMeta};

/// Maximum displayable length of an auto-derived session title.
const MAX_AUTO_TITLE_CHARS: usize = 50;

/// Maximum stored length of a caller-supplied session title. Roomier than the
/// auto-derived cap — a hand-written title is a deliberate choice — but still
/// bounded so one row cannot swamp a listing.
const MAX_SET_TITLE_CHARS: usize = 200;

/// Strip control characters from a title.
///
/// Titles are printed straight to the operator's terminal by
/// `rantaiclaw sessions list` (`sessions/cli.rs`), so an `ESC` that survives to
/// storage is an escape sequence executing on their terminal later: cursor
/// moves that overwrite neighbouring rows, or an OSC 52 clipboard write on
/// terminals that permit it. Whitespace collapsing alone does not catch this —
/// `ESC` (0x1B) is not whitespace.
///
/// `char::is_control` covers both C0 (0x00–0x1F, 0x7F) and C1 (0x80–0x9F), so
/// the 8-bit CSI introducer is handled along with the familiar `ESC [` form.
///
/// Knowingly *not* handled: bidirectional overrides (U+202A–U+202E, U+2066–
/// U+2069), which can reorder how a title displays without any control
/// character. That is a rendering-spoof class rather than terminal control, and
/// stripping it needs care not to break legitimate right-to-left titles.
fn strip_control(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Derive a session title from a user message: pick the first non-empty
/// line, drop control characters, collapse whitespace, truncate to
/// `MAX_AUTO_TITLE_CHARS` chars, and append `…` when truncated. Returns an
/// empty string for content that has no usable text.
///
/// This path matters more than the explicit setter: it runs on a session's
/// first message, so anything that can put a message into a session — an
/// inbound channel message included — can decide a title without anyone
/// calling the title API.
pub fn derive_session_title(content: &str) -> String {
    let first_line = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let cleaned = strip_control(first_line);
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = collapsed.chars().count();
    if count <= MAX_AUTO_TITLE_CHARS {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(MAX_AUTO_TITLE_CHARS).collect();
        format!("{truncated}…")
    }
}

/// Normalise a caller-supplied title: drop control characters, collapse
/// whitespace, cap the length. Returns an empty string when nothing usable is
/// left, which [`SessionStore::set_title`] treats as an error.
pub fn normalize_set_title(raw: &str) -> String {
    let cleaned = strip_control(raw);
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(MAX_SET_TITLE_CHARS).collect()
}

/// Outcome of resolving a session id or id prefix — see
/// [`SessionStore::resolve_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRef {
    /// Exactly one session matched; carries its full id.
    One(String),
    /// Nothing matched.
    None,
    /// The prefix matched several sessions; carries how many.
    Ambiguous(usize),
}

/// Whether a caller-supplied session id has the canonical UUID shape
/// (`8-4-4-4-12` lowercase-or-uppercase hex).
///
/// Deliberately a shape check, not a parse: it only needs to keep arbitrary
/// strings out of the primary key so ids stay uniform with the ones
/// [`Uuid::new_v4`] mints. Anything else falls back to a server-generated id.
fn is_uuid_shaped(s: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let mut parts = s.split('-');
    for want in groups {
        match parts.next() {
            Some(p) if p.len() == want && p.chars().all(|c| c.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

/// Escape the LIKE wildcards in a user-supplied prefix.
///
/// Session ids are UUIDs, but the prefix is whatever the caller typed. Without
/// this, `_` (LIKE's single-character wildcard) would silently over-match and a
/// prefix could resolve to a session the operator never named.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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

        // busy_timeout is REQUIRED: every /api/v1 handler opens its own connection
        // to this file, so concurrent writers must retry instead of failing
        // immediately with "database is locked". Matches channels/history_store.rs.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
        )?;
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

    /// Resolve a full session id, or a unique prefix of one, against the whole
    /// table.
    ///
    /// Callers used to do this themselves by scanning `list_sessions(500)` and
    /// filtering on `starts_with`, which was wrong in two ways. A session
    /// outside the 500 most recent was unreachable *even by its full id*, and —
    /// worse — the uniqueness check only saw that window, so a prefix matching
    /// one session inside it and another outside looked unambiguous. For
    /// `delete` that meant silently removing a different session than the one
    /// the operator named.
    ///
    /// An exact id match wins outright and short-circuits, so a full id is
    /// never reported ambiguous just because it also prefixes another id.
    pub fn resolve_id(&self, id_or_prefix: &str) -> Result<SessionRef> {
        if id_or_prefix.is_empty() {
            // An empty prefix would `LIKE '%'` its way to every row, and
            // resolve to "the only session" on a single-session store.
            return Ok(SessionRef::None);
        }
        if self.get_session(id_or_prefix)?.is_some() {
            return Ok(SessionRef::One(id_or_prefix.to_string()));
        }

        // Two rows is all it takes to decide none/one/ambiguous.
        let pattern = format!("{}%", escape_like(id_or_prefix));
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM sessions WHERE id LIKE ?1 ESCAPE '\\' LIMIT 2")?;
        let ids: Vec<String> = stmt
            .query_map(params![pattern], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?;

        match ids.len() {
            0 => Ok(SessionRef::None),
            1 => Ok(SessionRef::One(ids.into_iter().next().expect("len == 1"))),
            _ => {
                // Only now is the exact count worth a second query — it makes
                // "use a longer prefix" concrete.
                let total: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM sessions WHERE id LIKE ?1 ESCAPE '\\'",
                    params![pattern],
                    |row| row.get(0),
                )?;
                Ok(SessionRef::Ambiguous(
                    usize::try_from(total).unwrap_or(2).max(2),
                ))
            }
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
    ///
    /// The title is normalised first (see [`normalize_set_title`]) so every
    /// caller — the HTTP API, the CLI, and the TUI's `/title` — gets the same
    /// treatment. Normalising here rather than at each surface is deliberate:
    /// this is the single point every write goes through, and a per-surface
    /// guard is one new entry point away from being bypassed.
    ///
    /// Errors when nothing usable is left, rather than storing a blank. A
    /// caller asking to set an all-whitespace or all-control-character title
    /// has made a mistake, and reporting success while storing nothing hides
    /// it (CLAUDE.md §3.5). `backfill_titles` does treat `''` as untitled and
    /// would recover such a row, so this is about honest feedback rather than
    /// data recovery.
    pub fn set_title(&self, id: &str, title: &str) -> Result<()> {
        let title = normalize_set_title(title);
        if title.is_empty() {
            anyhow::bail!("session title is empty after normalisation");
        }
        self.conn.execute(
            "UPDATE sessions SET title = ?1 WHERE id = ?2",
            params![title, id],
        )?;
        Ok(())
    }

    /// Delete a session and all of its messages. Returns `true` if a session
    /// row was removed, `false` if no session matched `id`.
    ///
    /// Messages are deleted first, in a single transaction: the
    /// `messages.session_id` foreign key has no `ON DELETE CASCADE`, so with
    /// `PRAGMA foreign_keys=ON` (file-backed stores) removing the session row
    /// first would violate the constraint.
    pub fn delete_session(&mut self, id: &str) -> Result<bool> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM messages WHERE session_id = ?1", params![id])?;
        let removed = tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(removed > 0)
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
    /// Append a message and bump the session's counter.
    ///
    /// Both statements run in one transaction. Separately, a failure on the
    /// `UPDATE` left the message stored with `message_count` never incremented
    /// — and since the counter is only ever adjusted by `+1` here, the drift is
    /// permanent: nothing recomputes it from the messages table.
    pub fn append_message(&self, msg: &Message) -> Result<i64> {
        // `unchecked_transaction` takes `&self`, so atomicity here does not
        // force `&mut` on every caller holding the store behind a shared ref.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
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

        let row_id = tx.last_insert_rowid();

        tx.execute(
            "UPDATE sessions SET message_count = message_count + 1 WHERE id = ?1",
            params![msg.session_id],
        )?;
        tx.commit()?;

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

    /// Atomically record one API chat turn: continue-or-create the session,
    /// append the user + assistant messages, bump `message_count`, title a new
    /// session, and stamp `ended_at` — all in a single transaction. If any step
    /// fails the whole turn rolls back, so a contended write can never leave an
    /// orphan user row or a drifted `message_count`.
    ///
    /// Uses `IMMEDIATE` (not the default `DEFERRED`): this reads the session row
    /// then writes, and two concurrent DEFERRED read→write transactions deadlock
    /// with a `SQLITE_BUSY` that `busy_timeout` cannot resolve. `IMMEDIATE` takes
    /// the write lock up front so contenders serialize and retry cleanly.
    ///
    /// Returns the session id the turn landed in.
    pub fn record_api_turn(
        &mut self,
        model: &str,
        session_id: Option<&str>,
        user_message: &str,
        assistant_message: &str,
    ) -> Result<String> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Continue the supplied session when it exists; else start a fresh one.
        let existing = match session_id {
            Some(sid) if !sid.is_empty() => {
                match tx.query_row("SELECT 1 FROM sessions WHERE id = ?1", params![sid], |_| {
                    Ok(())
                }) {
                    Ok(()) => Some(sid.to_string()),
                    Err(rusqlite::Error::QueryReturnedNoRows) => None,
                    Err(e) => return Err(e.into()),
                }
            }
            _ => None,
        };
        let (id, is_new) = match existing {
            Some(id) => (id, false),
            None => {
                // Honour a caller-supplied id when it is UUID-shaped, instead of
                // discarding it and minting a different one.
                //
                // The console needs an id it can use *before* the first turn:
                // chat attachments are ingested into the KB under a per-
                // conversation category at upload time, which is before any
                // session exists. Previously the client invented its own key,
                // the gateway assigned a different one, and reopening the
                // session looked under the gateway's key — where the documents
                // were not. Letting the client name the session up front gives
                // one id end to end.
                //
                // Shape is enforced so a caller cannot litter the table with
                // arbitrary primary keys. A supplied id that already exists is
                // handled above (the turn continues that session), which is the
                // pre-existing behaviour.
                let id = session_id
                    .filter(|sid| is_uuid_shaped(sid))
                    .map_or_else(|| Uuid::new_v4().to_string(), str::to_string);
                let started_at = chrono::Utc::now().timestamp();
                tx.execute(
                    "INSERT INTO sessions (id, model, started_at, source) VALUES (?1, ?2, ?3, ?4)",
                    params![id, model, started_at, "api"],
                )?;
                (id, true)
            }
        };

        // Same timestamp for the pair — get_messages' `id ASC` tiebreaker keeps
        // the user turn before the assistant turn on replay.
        let now = chrono::Utc::now().timestamp();
        for (role, content) in [("user", user_message), ("assistant", assistant_message)] {
            tx.execute(
                "INSERT INTO messages (session_id, role, content, tool_calls, timestamp) \
                 VALUES (?1, ?2, ?3, NULL, ?4)",
                params![id, role, content, now],
            )?;
        }
        tx.execute(
            "UPDATE sessions SET message_count = message_count + 2, ended_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;

        // Title only the first turn — from the user's own text (decorations are
        // appended after it, so the first line stays the real question).
        if is_new {
            let title = derive_session_title(user_message);
            if !title.is_empty() {
                tx.execute(
                    "UPDATE sessions SET title = ?1 WHERE id = ?2",
                    params![title, id],
                )?;
            }
        }

        tx.commit()?;
        Ok(id)
    }

    /// Retrieve all messages for a session, ordered by timestamp ascending.
    pub fn get_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, content, tool_calls, timestamp \
             FROM messages WHERE session_id = ?1 ORDER BY timestamp ASC, id ASC",
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
        self.list_sessions_paged(limit, 0)
    }

    /// Total number of stored sessions — what a client needs to know how many
    /// pages [`Self::list_sessions_paged`] has.
    pub fn count_sessions(&self) -> Result<usize> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        Ok(usize::try_from(total).unwrap_or(0))
    }

    /// One page of sessions, newest first, skipping `offset` rows.
    ///
    /// Without an offset the API could only ever show the newest 500 sessions;
    /// anything older was invisible in the console with no way to reach it.
    pub fn list_sessions_paged(&self, limit: usize, offset: usize) -> Result<Vec<SessionMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model, started_at, message_count \
             FROM sessions ORDER BY started_at DESC LIMIT ?1 OFFSET ?2",
        )?;

        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        let sessions = stmt
            .query_map(params![limit, offset], |row| {
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
    /// End `session_id` and open a child session carrying `summary` as its
    /// first message.
    ///
    /// All four writes — end the parent, insert the child, insert the summary,
    /// set the child's counter — run in one transaction. Run separately, a
    /// failure part-way left the parent ended with no child to continue into,
    /// or a child with no summary, and the operator had no way to tell which.
    pub fn split_session(&self, session_id: &str, summary: &str, model: &str) -> Result<Session> {
        let source = self
            .get_session(session_id)?
            .map(|s| s.source)
            .unwrap_or_else(|| "tui".to_string());

        let new_id = Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now().timestamp();
        let ended_at = chrono::Utc::now().timestamp();

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
            params![ended_at, session_id],
        )?;
        tx.execute(
            "INSERT INTO sessions (id, parent_session_id, model, started_at, source) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![new_id, session_id, model, started_at, source],
        )?;
        tx.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls, timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                new_id,
                "system",
                summary,
                Option::<String>::None,
                started_at
            ],
        )?;
        tx.execute(
            "UPDATE sessions SET message_count = message_count + 1 WHERE id = ?1",
            params![new_id],
        )?;
        tx.commit()?;

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
    fn open_sets_busy_timeout() {
        // File-backed stores must retry on lock contention instead of erroring,
        // so concurrent /api/v1 handlers don't hit "database is locked".
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::open(&dir.path().join("sessions.db")).expect("open store");
        let ms: i64 = store
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .expect("query busy_timeout");
        assert_eq!(ms, 5000);
    }

    #[test]
    fn record_api_turn_orders_user_before_assistant() {
        let mut s = store();
        let id = s
            .record_api_turn("m", None, "the question", "the answer")
            .unwrap();

        let msgs = s.get_messages(&id).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
        // Both rows share a (second-granular) timestamp — replay order relies on
        // the `id ASC` tiebreaker in get_messages, not on the timestamp.
        assert_eq!(msgs[0].timestamp, msgs[1].timestamp);
        assert!(msgs[0].id < msgs[1].id);

        let sess = s.get_session(&id).unwrap().unwrap();
        assert_eq!(sess.message_count, 2);
        assert_eq!(sess.title.as_deref(), Some("the question"));
        assert!(sess.ended_at.is_some());
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

    /// Force a known id onto a session so prefix collisions can be constructed.
    fn insert_with_id(store: &SessionStore, id: &str, started_at: i64) {
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, model, started_at, message_count, token_count, source) \
                 VALUES (?1, 'test-model', ?2, 0, 0, 'tui')",
                params![id, started_at],
            )
            .unwrap();
    }

    #[test]
    fn record_api_turn_adopts_a_uuid_shaped_client_id() {
        // The console picks the id before the first turn so it can use the same
        // value as the KB category for attachments uploaded before send.
        let mut s = SessionStore::in_memory().unwrap();
        let chosen = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
        let got = s
            .record_api_turn("m", Some(chosen), "question", "answer")
            .unwrap();
        assert_eq!(got, chosen);
        assert!(s.get_session(chosen).unwrap().is_some());
    }

    #[test]
    fn record_api_turn_reuses_the_adopted_id_on_the_next_turn() {
        // Second turn must continue the same session, not start another.
        let mut s = SessionStore::in_memory().unwrap();
        let chosen = "3f2504e0-4f89-41d3-9a0c-0305e82c3302";
        s.record_api_turn("m", Some(chosen), "one", "a").unwrap();
        let second = s.record_api_turn("m", Some(chosen), "two", "b").unwrap();
        assert_eq!(second, chosen);
        assert_eq!(s.list_sessions(10).unwrap().len(), 1);
        assert_eq!(s.get_messages(chosen).unwrap().len(), 4);
    }

    #[test]
    fn record_api_turn_rejects_a_non_uuid_client_id() {
        // Anything not UUID-shaped falls back to a server-minted id, so callers
        // cannot put arbitrary strings into the primary key.
        let mut s = SessionStore::in_memory().unwrap();
        for junk in ["c-3-8471", "../../etc/passwd", "", "not-a-uuid", "3f2504e0"] {
            let got = s.record_api_turn("m", Some(junk), "q", "a").unwrap();
            assert_ne!(got, junk, "junk id {junk:?} was adopted");
            assert!(is_uuid_shaped(&got));
        }
    }

    #[test]
    fn uuid_shape_check_accepts_generated_ids_and_rejects_near_misses() {
        assert!(is_uuid_shaped(&Uuid::new_v4().to_string()));
        assert!(is_uuid_shaped("3F2504E0-4F89-41D3-9A0C-0305E82C3301"));
        // Wrong group lengths, non-hex, missing and extra groups.
        assert!(!is_uuid_shaped("3f2504e0-4f89-41d3-9a0c-0305e82c330"));
        assert!(!is_uuid_shaped(
            "3f2504e0-4f89-41d3-9a0c-0305e82c3301-extra"
        ));
        assert!(!is_uuid_shaped("zf2504e0-4f89-41d3-9a0c-0305e82c3301"));
        assert!(!is_uuid_shaped("3f2504e04f8941d39a0c0305e82c3301"));
    }

    #[test]
    fn list_sessions_paged_walks_past_the_first_page() {
        // Without an offset the API could only ever show the newest N; older
        // sessions were invisible with no way to reach them.
        let store = SessionStore::in_memory().unwrap();
        for i in 0..25 {
            insert_with_id(&store, &format!("s-{i:03}"), i64::from(i));
        }
        assert_eq!(store.count_sessions().unwrap(), 25);

        let page1 = store.list_sessions_paged(10, 0).unwrap();
        let page2 = store.list_sessions_paged(10, 10).unwrap();
        let page3 = store.list_sessions_paged(10, 20).unwrap();
        assert_eq!(page1.len(), 10);
        assert_eq!(page2.len(), 10);
        assert_eq!(page3.len(), 5, "last page is partial");

        // Newest first, and no row appears twice across the pages.
        assert_eq!(page1[0].id, "s-024");
        assert_eq!(page3[4].id, "s-000");
        let seen: std::collections::HashSet<_> = page1
            .iter()
            .chain(page2.iter())
            .chain(page3.iter())
            .map(|s| s.id.clone())
            .collect();
        assert_eq!(seen.len(), 25, "pages must not overlap or skip");
    }

    #[test]
    fn list_sessions_paged_past_the_end_is_empty_not_an_error() {
        let store = SessionStore::in_memory().unwrap();
        insert_with_id(&store, "only-one", 1);
        assert!(store.list_sessions_paged(10, 99).unwrap().is_empty());
    }

    #[test]
    fn list_sessions_still_returns_the_first_page() {
        // The old signature is now a zero-offset call; existing callers unchanged.
        let store = SessionStore::in_memory().unwrap();
        for i in 0..5 {
            insert_with_id(&store, &format!("t-{i}"), i64::from(i));
        }
        assert_eq!(store.list_sessions(3).unwrap().len(), 3);
        assert_eq!(store.list_sessions(3).unwrap()[0].id, "t-4");
    }

    #[test]
    fn resolve_id_matches_a_full_id() {
        let store = SessionStore::in_memory().unwrap();
        let s = store.new_session("m", "tui").unwrap();
        assert_eq!(store.resolve_id(&s.id).unwrap(), SessionRef::One(s.id));
    }

    #[test]
    fn resolve_id_matches_a_unique_prefix() {
        let store = SessionStore::in_memory().unwrap();
        insert_with_id(&store, "abc12345", 1);
        assert_eq!(
            store.resolve_id("abc").unwrap(),
            SessionRef::One("abc12345".into())
        );
    }

    #[test]
    fn resolve_id_reports_no_match() {
        let store = SessionStore::in_memory().unwrap();
        insert_with_id(&store, "abc12345", 1);
        assert_eq!(store.resolve_id("zzz").unwrap(), SessionRef::None);
    }

    #[test]
    fn resolve_id_reports_ambiguity_with_a_count() {
        let store = SessionStore::in_memory().unwrap();
        insert_with_id(&store, "abc11111", 1);
        insert_with_id(&store, "abc22222", 2);
        insert_with_id(&store, "abc33333", 3);
        assert_eq!(store.resolve_id("abc").unwrap(), SessionRef::Ambiguous(3));
    }

    #[test]
    fn resolve_id_prefers_an_exact_id_over_the_longer_ones_it_prefixes() {
        // "abc" is both a complete id and a prefix of two others. Naming it
        // exactly must address it, not report an ambiguity the operator has no
        // way to resolve.
        let store = SessionStore::in_memory().unwrap();
        insert_with_id(&store, "abc", 1);
        insert_with_id(&store, "abcdef", 2);
        insert_with_id(&store, "abcxyz", 3);
        assert_eq!(
            store.resolve_id("abc").unwrap(),
            SessionRef::One("abc".into())
        );
    }

    #[test]
    fn resolve_id_reaches_past_the_five_hundred_most_recent() {
        // The regression this fix exists for: resolution used to scan
        // `list_sessions(500)`, so an older session was unreachable even by its
        // full id.
        let store = SessionStore::in_memory().unwrap();
        insert_with_id(&store, "oldest-session-id", 0);
        for i in 1..=600 {
            insert_with_id(&store, &format!("filler-{i:04}"), i64::from(i) + 1000);
        }
        assert_eq!(
            store.resolve_id("oldest-session-id").unwrap(),
            SessionRef::One("oldest-session-id".into())
        );
        assert_eq!(
            store.resolve_id("oldest").unwrap(),
            SessionRef::One("oldest-session-id".into())
        );
    }

    #[test]
    fn resolve_id_sees_ambiguity_that_straddles_the_old_window() {
        // The dangerous case. Two sessions share a prefix; one is recent, the
        // other is far outside the old 500-row window. The old scan saw a
        // single match and reported success — for `delete`, that removed a
        // session the operator had not named.
        let store = SessionStore::in_memory().unwrap();
        insert_with_id(&store, "dupe-old", 0);
        for i in 1..=600 {
            insert_with_id(&store, &format!("filler-{i:04}"), i64::from(i) + 1000);
        }
        insert_with_id(&store, "dupe-new", 9999);
        assert_eq!(store.resolve_id("dupe").unwrap(), SessionRef::Ambiguous(2));
    }

    #[test]
    fn resolve_id_rejects_an_empty_prefix() {
        // `LIKE '%'` would match everything, and on a single-session store an
        // empty prefix would resolve to that session.
        let store = SessionStore::in_memory().unwrap();
        store.new_session("m", "tui").unwrap();
        assert_eq!(store.resolve_id("").unwrap(), SessionRef::None);
    }

    #[test]
    fn resolve_id_does_not_treat_like_wildcards_as_wildcards() {
        // `_` is LIKE's single-character wildcard; unescaped, "a_c" would match
        // "abc" and address a session the operator never named.
        let store = SessionStore::in_memory().unwrap();
        insert_with_id(&store, "abc12345", 1);
        assert_eq!(store.resolve_id("a_c").unwrap(), SessionRef::None);
        assert_eq!(store.resolve_id("%").unwrap(), SessionRef::None);
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
    fn set_title_strips_terminal_escape_sequences() {
        // `sessions list` prints titles straight to the operator's terminal, so
        // a stored ESC is an escape sequence executing on their machine later.
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();

        s.set_title(
            &sess.id,
            "\u{1b}[2K\u{1b}[Ashadowed\u{1b}]52;c;cGVybmc\u{7}",
        )
        .unwrap();

        let stored = s.get_session(&sess.id).unwrap().unwrap().title.unwrap();
        assert!(
            !stored.chars().any(char::is_control),
            "control characters survived: {stored:?}"
        );
        assert_eq!(stored, "[2K[Ashadowed]52;c;cGVybmc");
    }

    #[test]
    fn set_title_strips_the_eight_bit_csi_introducer() {
        // C1 controls reach the same terminal behaviour without a literal ESC.
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();
        s.set_title(&sess.id, "before\u{9b}31mafter").unwrap();
        let stored = s.get_session(&sess.id).unwrap().unwrap().title.unwrap();
        assert_eq!(stored, "before31mafter");
    }

    #[test]
    fn set_title_collapses_whitespace_and_newlines() {
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();
        s.set_title(&sess.id, "  spread \n\n over \t lines  ")
            .unwrap();
        let stored = s.get_session(&sess.id).unwrap().unwrap().title.unwrap();
        assert_eq!(stored, "spread over lines");
    }

    #[test]
    fn set_title_caps_length() {
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();
        s.set_title(&sess.id, &"x".repeat(500)).unwrap();
        let stored = s.get_session(&sess.id).unwrap().unwrap().title.unwrap();
        assert_eq!(stored.chars().count(), 200);
    }

    #[test]
    fn set_title_rejects_a_title_with_nothing_usable_left() {
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();
        assert!(s.set_title(&sess.id, "   ").is_err());
        assert!(s.set_title(&sess.id, "\u{1b}\u{7}\u{9b}").is_err());
        // The session keeps whatever it had rather than gaining a blank.
        assert!(s.get_session(&sess.id).unwrap().unwrap().title.is_none());
    }

    #[test]
    fn derived_titles_are_stripped_too() {
        // The auto-title path is the more reachable one: it runs on a session's
        // first message, so an inbound channel message can decide a title
        // without anyone calling the title API.
        let derived = derive_session_title("\u{1b}[2Khidden real question");
        assert!(!derived.chars().any(char::is_control));
        assert_eq!(derived, "[2Khidden real question");
    }

    #[test]
    fn message_count_tracks_the_messages_actually_stored() {
        // `message_count` is only ever adjusted by `+1` in `append_message` —
        // nothing recomputes it from the messages table — so a write that
        // stored the row but skipped the increment would drift permanently.
        let s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();
        for i in 0..5 {
            s.append_message(&Message::user(&sess.id, &format!("m{i}")))
                .unwrap();
        }
        let stored = s.get_session(&sess.id).unwrap().unwrap();
        assert_eq!(stored.message_count, 5);
        assert_eq!(s.get_messages(&sess.id).unwrap().len(), 5);
    }

    #[test]
    fn split_session_leaves_a_consistent_pair() {
        // The parent must be ended *and* the child must exist with its summary
        // and a matching counter — the four writes are one unit.
        let s = store();
        let parent = s.new_session("gpt-4o", "tui").unwrap();
        s.append_message(&Message::user(&parent.id, "before split"))
            .unwrap();

        let child = s
            .split_session(&parent.id, "context summary", "gpt-4o")
            .unwrap();

        let parent_row = s.get_session(&parent.id).unwrap().unwrap();
        let child_row = s.get_session(&child.id).unwrap().unwrap();
        assert!(parent_row.ended_at.is_some(), "parent ended");
        assert_eq!(parent_row.message_count, 1, "parent counter untouched");
        assert_eq!(
            child_row.parent_session_id.as_deref(),
            Some(parent.id.as_str())
        );
        assert_eq!(child_row.message_count, 1, "child counter includes summary");
        assert_eq!(s.get_messages(&child.id).unwrap().len(), 1);
    }

    #[test]
    fn split_session_reports_the_counter_it_actually_stored() {
        // The returned Session used to hard-code `message_count: 1` while the
        // row was written by a separate statement; assert the two agree.
        let s = store();
        let parent = s.new_session("gpt-4o", "tui").unwrap();
        let child = s.split_session(&parent.id, "summary", "gpt-4o").unwrap();
        let stored = s.get_session(&child.id).unwrap().unwrap();
        assert_eq!(child.message_count, stored.message_count);
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

    #[test]
    fn delete_session_removes_session_and_its_messages() {
        let mut s = store();
        let sess = s.new_session("gpt-4o", "tui").unwrap();
        s.append_message(&Message::user(&sess.id, "hello")).unwrap();
        s.append_message(&Message::assistant(&sess.id, "hi"))
            .unwrap();

        let removed = s.delete_session(&sess.id).unwrap();
        assert!(removed);

        assert!(s.get_session(&sess.id).unwrap().is_none());
        assert!(s.get_messages(&sess.id).unwrap().is_empty());
    }

    #[test]
    fn delete_session_returns_false_for_nonexistent() {
        let mut s = store();
        let removed = s.delete_session("no-such-id").unwrap();
        assert!(!removed);
    }
}
