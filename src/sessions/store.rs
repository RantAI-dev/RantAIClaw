use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use super::migrations::run_migrations;
use super::scrub::scrub_channel_message;
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

/// Turn free text into an FTS5 query that matches it literally: each whitespace
/// token becomes a quoted phrase (inner `"` doubled), joined by implicit AND.
/// `"`, `*`, `(`, `NEAR` and other FTS operators in user input therefore never
/// reach the parser as syntax. Returns an empty string for whitespace-only
/// input (the caller treats that as "no results").
fn fts_literal_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Like [`fts_literal_query`], but joins the quoted tokens with `OR` so the
/// FTS5 parser matches any single word. Used by the any-word fallback the
/// `session_search` tool runs when its first pass returns nothing; see
/// [`SessionStore::search_any_word_with_conversation`].
fn fts_any_word_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Cumulative session/message statistics, computed in SQL by [`SessionStore::stats`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStats {
    pub total_sessions: usize,
    pub total_messages: i64,
    pub latest_session_id: Option<String>,
    pub latest_session_started_at: Option<i64>,
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
            conversation_key: None,
        })
    }

    /// Retrieve a session by its ID, returning `None` if not found.
    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, parent_session_id, model, started_at, ended_at, \
             message_count, token_count, source, conversation_key \
             FROM sessions WHERE id = ?1",
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
                conversation_key: row.get(9)?,
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

    /// Every session id for one conversation_key, ordered newest first.
    /// Caller still has to fetch each session's metadata if it needs it; this
    /// helper only returns ids so the DELETE handler can fan out grant
    /// clearing across the rows the delete is about to remove.
    pub fn session_ids_for_conversation(&self, conversation_key: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM sessions WHERE conversation_key = ?1 \
             ORDER BY started_at DESC, id DESC",
        )?;
        let rows = stmt
            .query_map(params![conversation_key], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One transaction: every session for `conversation_key` and every
    /// message under those sessions. Returns the number of session rows
    /// removed. Caller `clear_session_grants` runs outside this transaction
    /// for each id (grants are in-memory) so a busy database lock never
    /// blocks prompt granting.
    pub fn delete_conversation(&mut self, conversation_key: &str) -> Result<usize> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE session_id IN \
             (SELECT id FROM sessions WHERE conversation_key = ?1)",
            params![conversation_key],
        )?;
        let removed = tx.execute(
            "DELETE FROM sessions WHERE conversation_key = ?1",
            params![conversation_key],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// Page of channel sessions with conversation metadata for the console's
    /// `source=channel` listing. Ordered newest first.
    pub fn list_conversation_sessions(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::sessions::ConversationSessionRow>> {
        let limit_v = i64::try_from(limit).unwrap_or(i64::MAX);
        let offset_v = i64::try_from(offset).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.title, s.model, s.started_at, s.ended_at, \
                    s.message_count, s.source, s.conversation_key, \
                    COALESCE( \
                        (SELECT MAX(m.timestamp) FROM messages m \
                         WHERE m.session_id = s.id), s.started_at) \
             FROM sessions s \
             WHERE s.source = 'channel' AND s.conversation_key IS NOT NULL \
             ORDER BY s.started_at DESC, s.id DESC \
             LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt
            .query_map(params![limit_v, offset_v], |row| {
                Ok(crate::sessions::ConversationSessionRow {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    model: row.get(2)?,
                    started_at: row.get(3)?,
                    ended_at: row.get(4)?,
                    message_count: row.get(5)?,
                    source: row.get(6)?,
                    conversation_key: row.get(7)?,
                    last_activity_at: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Count of channel rows for the console's `source=channel` count.
    pub fn count_conversation_sessions(&self) -> Result<usize> {
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE source = 'channel' \
             AND conversation_key IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(total).unwrap_or(0))
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
    /// — and since the counter is only ever adjusted by `+1` here, the drift
    /// would be permanent: the per-row `+1` could not fix a session whose
    /// count was already wrong. `prune_channel_sessions` re-counts the
    /// surviving rows for the sessions it touches, which covers the case
    /// where a row went away through pruning; no other path adjusts the
    /// counter, so the per-row `+1` is the only way the append state changes.
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

    // ── Channel recording ────────────────────────────────────────────────────

    /// How long a channel session is kept after the last turn before the daily
    /// retention sweep removes it. Matches the `channel_history` retention in
    /// `src/channels/history_store.rs`.
    pub const CHANNEL_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

    /// Record a user + assistant turn for a channel conversation, reusing the
    /// open `source='channel'` session for `conversation_key` if one exists, or
    /// opening a fresh one otherwise.
    ///
    /// `title` is the chat title the channel parser already held (Telegram
    /// `chat.title`); the session row gets it on the first turn only. When the
    /// parser did not have one, the caller passes `None` and the title stays
    /// `NULL` until something else sets it.
    ///
    /// Both scrubbers run on every recorded message — `scrub_secret_patterns`
    /// for known token prefixes, `scrub_credentials` for `key=value` style
    /// secrets — before the INSERT. A secret written as a plain sentence is
    /// not recognised; that's the limit the docs spell out.
    ///
    /// All four writes — open-or-find, two messages, the counter and the
    /// title — run in one `IMMEDIATE` transaction so a contention on the same
    /// key cannot leave a half-recorded turn. The caller treats a write
    /// failure as log-and-ignore; it never blocks the channel reply.
    pub fn record_channel_turn(
        &mut self,
        model: &str,
        conversation_key: &str,
        user_message: &str,
        assistant_message: &str,
        title: Option<&str>,
    ) -> Result<String> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let (id, is_new) = match tx.query_row(
            "SELECT id FROM sessions \
                 WHERE conversation_key = ?1 \
                   AND source = 'channel' \
                   AND ended_at IS NULL \
                 ORDER BY started_at DESC, id DESC LIMIT 1",
            params![conversation_key],
            |row| row.get::<_, String>(0),
        ) {
            Ok(id) => (id, false),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                let id = Uuid::new_v4().to_string();
                let started_at = chrono::Utc::now().timestamp();
                tx.execute(
                    "INSERT INTO sessions \
                        (id, model, started_at, source, conversation_key) \
                     VALUES (?1, ?2, ?3, 'channel', ?4)",
                    params![id, model, started_at, conversation_key],
                )?;
                (id, true)
            }
            Err(e) => return Err(e.into()),
        };

        let scrubbed_user = scrub_channel_message(user_message);
        let scrubbed_reply = scrub_channel_message(assistant_message);

        let now = chrono::Utc::now().timestamp();
        for (role, content) in [
            ("user", scrubbed_user.as_str()),
            ("assistant", scrubbed_reply.as_str()),
        ] {
            tx.execute(
                "INSERT INTO messages (session_id, role, content, tool_calls, timestamp) \
                 VALUES (?1, ?2, ?3, NULL, ?4)",
                params![id, role, content, now],
            )?;
        }
        // Bump `message_count` only. `ended_at` is set only by
        // `close_channel_session`; flipping it on every turn would defeat the
        // reuse-by-`ended_at IS NULL` lookup and break the "same conversation
        // stays in one session" contract the plan tests for.
        tx.execute(
            "UPDATE sessions SET message_count = message_count + 2 WHERE id = ?1",
            params![id],
        )?;

        if is_new {
            if let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) {
                tx.execute(
                    "UPDATE sessions SET title = ?1 WHERE id = ?2",
                    params![title, id],
                )?;
            }
        }

        tx.commit()?;
        Ok(id)
    }

    /// Close every open `source='channel'` session for `conversation_key`.
    /// Returns the number of rows closed. Idempotent: a second call is a no-op.
    /// Called by `/new` and `/clear` so the next recorded turn opens a fresh
    /// session for the same conversation.
    pub fn close_channel_session(&mut self, conversation_key: &str) -> Result<usize> {
        let ended_at = chrono::Utc::now().timestamp();
        let closed = self.conn.execute(
            "UPDATE sessions SET ended_at = ?1 \
             WHERE conversation_key = ?2 \
               AND source = 'channel' \
               AND ended_at IS NULL",
            params![ended_at, conversation_key],
        )?;
        Ok(closed)
    }

    /// The id of the open channel session for `conversation_key`, or `None`
    /// when there is none. Used by callers that want to attach metadata to
    /// the existing session rather than mint a new one.
    pub fn open_channel_session_id(&self, conversation_key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM sessions \
             WHERE conversation_key = ?1 \
               AND source = 'channel' \
               AND ended_at IS NULL \
             ORDER BY started_at DESC, id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![conversation_key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// Drop the messages of `source='channel'` sessions whose `timestamp` is
    /// older than `now - retention_secs`, and delete any session left empty
    /// after the drop. Non-channel sessions are untouched. The caller decides
    /// how often to run it (startup + once a day).
    ///
    /// Returns the number of sessions removed.
    pub fn prune_channel_sessions(&mut self, retention_secs: i64) -> Result<usize> {
        let now = chrono::Utc::now().timestamp();
        let cutoff = now.saturating_sub(retention_secs);
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Retention is 30 days after the *last turn*, so prune by message
        // timestamp, not by `started_at`. A session whose only old messages
        // sit before the cutoff is left in place while its stale rows are
        // dropped; a session whose every message is old is empty afterwards
        // and is deleted in the same transaction. The recount below keeps
        // `sessions.message_count` consistent with the messages table in the
        // same transaction — a kept session that lost half its history would
        // otherwise keep the pre-prune count, and `SUM(message_count)` in
        // `stats` would overstate the store.
        tx.execute(
            "DELETE FROM messages \
             WHERE timestamp < ?1 \
               AND session_id IN ( \
                 SELECT id FROM sessions WHERE source = 'channel' \
             )",
            params![cutoff],
        )?;
        // Recount every kept channel session in one statement. The set is
        // bounded by the number of channel sessions with surviving messages,
        // not the number of messages, so the per-row subquery stays cheap.
        tx.execute(
            "UPDATE sessions \
             SET message_count = ( \
               SELECT COUNT(*) FROM messages m WHERE m.session_id = sessions.id \
             ) \
             WHERE source = 'channel'",
            [],
        )?;
        let removed = tx.execute(
            "DELETE FROM sessions \
             WHERE source = 'channel' \
               AND id NOT IN ( \
                 SELECT DISTINCT session_id FROM messages \
                 WHERE session_id IS NOT NULL \
               ) \
               AND started_at < ?1",
            params![cutoff],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// One page of sessions, optionally filtered by `source`. `None` keeps the
    /// pre-existing default (no filter); the channel recording pass is the only
    /// surface that calls `Some(...)` and the operator-facing callers pass
    /// `None` today so their lists are unchanged.
    pub fn list_sessions_paged_with_source(
        &self,
        limit: usize,
        offset: usize,
        source: Option<&str>,
    ) -> Result<Vec<SessionMeta>> {
        let limit_v = i64::try_from(limit).unwrap_or(i64::MAX);
        let offset_v = i64::try_from(offset).unwrap_or(i64::MAX);
        let mut stmt = match source {
            Some(_src) => self.conn.prepare(
                "SELECT id, title, model, started_at, message_count, source \
                 FROM sessions WHERE source = ?1 \
                 ORDER BY started_at DESC, id DESC LIMIT ?2 OFFSET ?3",
            )?,
            None => self.conn.prepare(
                "SELECT id, title, model, started_at, message_count, source \
                 FROM sessions \
                 ORDER BY started_at DESC, id DESC LIMIT ?1 OFFSET ?2",
            )?,
        };
        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<SessionMeta> {
            Ok(SessionMeta {
                id: row.get(0)?,
                title: row.get(1)?,
                model: row.get(2)?,
                started_at: row.get(3)?,
                message_count: row.get(4)?,
                source: row.get(5)?,
            })
        };
        let sessions = match source {
            Some(src) => stmt
                .query_map(params![src, limit_v, offset_v], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map(params![limit_v, offset_v], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(sessions)
    }

    /// One page of sessions visible to an operator by default: every row
    /// except `source='channel'`. The channel recording layer keeps its own
    /// sessions in this database but the operator's `/sessions` and CLI list
    /// never show them unless the caller explicitly asks (typically with
    /// `--source channel`).
    pub fn list_sessions_paged_visible(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SessionMeta>> {
        let limit_v = i64::try_from(limit).unwrap_or(i64::MAX);
        let offset_v = i64::try_from(offset).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model, started_at, message_count, source \
             FROM sessions WHERE source != 'channel' \
             ORDER BY started_at DESC, id DESC LIMIT ?1 OFFSET ?2",
        )?;
        let sessions = stmt
            .query_map(params![limit_v, offset_v], |row| {
                Ok(SessionMeta {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    model: row.get(2)?,
                    started_at: row.get(3)?,
                    message_count: row.get(4)?,
                    source: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(sessions)
    }

    /// Count of sessions that would appear in `list_sessions_paged_visible`.
    /// Pairs with [`Self::count_sessions`] for the operator-facing pages.
    pub fn count_sessions_visible(&self) -> Result<usize> {
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE source != 'channel'",
            [],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(total).unwrap_or(0))
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

    /// The most recent `max_messages` messages for a session, returned
    /// oldest-first so replay order is natural. Bounds the prompt a long
    /// conversation rebuilds on every turn — without this, turn N re-sends
    /// turns 1..N-1 in full. `get_messages` (unbounded) is kept for the
    /// transcript view, which wants everything.
    pub fn get_recent_messages(
        &self,
        session_id: &str,
        max_messages: usize,
    ) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, content, tool_calls, timestamp \
             FROM messages WHERE session_id = ?1 ORDER BY timestamp DESC, id DESC LIMIT ?2",
        )?;
        let lim = i64::try_from(max_messages).unwrap_or(i64::MAX);
        let mut messages = stmt
            .query_map(params![session_id, lim], |row| {
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
        // Query took newest-first for the LIMIT; flip back to chronological.
        messages.reverse();
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

    /// Whether a session row exists. Used by the API to refuse to re-create a
    /// session that was deleted while a turn was in flight.
    pub fn session_exists(&self, id: &str) -> Result<bool> {
        Ok(self.get_session(id)?.is_some())
    }

    /// Cumulative session/message stats computed in SQL, so the count stays
    /// correct past any page size (the old path loaded 10,000 rows and counted
    /// them in Rust, freezing at 10,000).
    pub fn stats(&self) -> Result<SessionStats> {
        let (total_sessions, total_messages): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(message_count), 0) FROM sessions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let latest = self
            .conn
            .query_row(
                "SELECT id, started_at FROM sessions ORDER BY started_at DESC, id DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(SessionStats {
            total_sessions: usize::try_from(total_sessions).unwrap_or(0),
            total_messages,
            latest_session_id: latest.as_ref().map(|l| l.0.clone()),
            latest_session_started_at: latest.map(|l| l.1),
        })
    }

    /// One page of sessions, newest first, skipping `offset` rows.
    ///
    /// Without an offset the API could only ever show the newest 500 sessions;
    /// anything older was invisible in the console with no way to reach it.
    pub fn list_sessions_paged(&self, limit: usize, offset: usize) -> Result<Vec<SessionMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model, started_at, message_count, source \
             FROM sessions ORDER BY started_at DESC, id DESC LIMIT ?1 OFFSET ?2",
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
                    source: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(sessions)
    }

    /// Full-text search across message content using FTS5, ranked by BM25.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        // Match the user's text literally: each whitespace token becomes a
        // quoted phrase (inner `"` doubled), joined by implicit AND. A stray
        // `"`, a leading `*`, an unbalanced paren, or an operator word like
        // `OR`/`NEAR` in user input therefore never reaches the FTS5 parser as
        // syntax — which used to surface as a 500 with the raw SQLite message.
        let match_query = fts_literal_query(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
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
            .query_map(params![match_query, limit as i64], |row| {
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

    /// Full-text search like [`Self::search`], restricted at the SQL level to
    /// one `sessions.source` bucket.
    ///
    /// Predicate folding keeps the query plan the same in both shapes — a
    /// single bound parameter on `s.source` and a folded `=` / `!=`
    /// comparison, never a `LIKE` over a generic argument. `Some(src)` adds
    /// `s.source = ?2` so only rows with that source can match; `None` adds
    /// `s.source != ?2` bound to the literal `"channel"` so the operator's
    /// free-text search never bleeds into channel recordings.
    ///
    /// `search` returned channel rows together with every other source before this
    /// method existed; there was no post-fetch filter. Filtering in SQL is what
    /// keeps `limit` slots honest for the bucket the caller asked for: a
    /// query that used to fill `limit` with channel rows now fills it with the
    /// rows the source predicate lets through.
    pub fn search_scoped(
        &self,
        query: &str,
        limit: usize,
        source: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let match_query = fts_literal_query(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        // Fold the predicate and pick one bound parameter. The source value
        // we bind depends on whether the caller wants to include or exclude
        // the bucket. `None` excludes `channel`; `Some(src)` matches exactly
        // that source.
        let (source_predicate, source_value): (&str, &str) = match source {
            Some(src) => ("s.source = ?2", src),
            None => ("s.source != ?2", "channel"),
        };
        let sql = format!(
            "SELECT m.session_id, s.title, m.id, m.role, m.content, m.timestamp, \
             bm25(messages_fts) as rank \
             FROM messages_fts \
             JOIN messages m ON messages_fts.rowid = m.id \
             JOIN sessions s ON m.session_id = s.id \
             WHERE messages_fts MATCH ?1 \
             AND {source_predicate} \
             ORDER BY rank \
             LIMIT ?3"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let results = stmt
            .query_map(params![match_query, source_value, limit as i64], |row| {
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

    /// Full-text search optionally restricted to a single `conversation_key`.
    ///
    /// `Some(key)` adds `AND s.conversation_key = ?key` to the FTS query so a
    /// scoped caller never sees rows outside its view. The covering
    /// `(conversation_key, started_at DESC)` index makes the filter cheap.
    /// `None` matches the unscoped [`Self::search`] shape row for row.
    pub fn search_with_conversation(
        &self,
        query: &str,
        limit: usize,
        conversation_key: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let match_query = fts_literal_query(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match conversation_key {
            Some(_) => (
                "SELECT m.session_id, s.title, m.id, m.role, m.content, m.timestamp, \
                 bm25(messages_fts) as rank \
                 FROM messages_fts \
                 JOIN messages m ON messages_fts.rowid = m.id \
                 JOIN sessions s ON m.session_id = s.id \
                 WHERE messages_fts MATCH ?1 AND s.conversation_key = ?2 \
                 ORDER BY rank \
                 LIMIT ?3",
                vec![
                    Box::new(match_query),
                    Box::new(conversation_key.unwrap_or("").to_string()),
                    Box::new(limit as i64),
                ],
            ),
            None => (
                "SELECT m.session_id, s.title, m.id, m.role, m.content, m.timestamp, \
                 bm25(messages_fts) as rank \
                 FROM messages_fts \
                 JOIN messages m ON messages_fts.rowid = m.id \
                 JOIN sessions s ON m.session_id = s.id \
                 WHERE messages_fts MATCH ?1 \
                 ORDER BY rank \
                 LIMIT ?2",
                vec![Box::new(match_query), Box::new(limit as i64)],
            ),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let params_iter: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|b| b.as_ref()).collect();
        let results = stmt
            .query_map(params_iter.as_slice(), |row| {
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

    /// Full-text search where any single whitespace token in `query` matches,
    /// optionally restricted to a single `conversation_key`. The fallback
    /// path the tool uses when the all-words pass returns nothing: a row that
    /// carries one of the words is surfaced. Tokens are quoted individually
    /// so an FTS operator in user input never reaches the parser as syntax;
    /// the previous build hand-joined `"a" OR "b"` and passed it through
    /// `fts_literal_query`, which re-quoted the whole expression and made
    /// `OR` a literal word to match.
    pub fn search_any_word_with_conversation(
        &self,
        query: &str,
        limit: usize,
        conversation_key: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let match_query = fts_any_word_query(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match conversation_key {
            Some(_) => (
                "SELECT m.session_id, s.title, m.id, m.role, m.content, m.timestamp, \
                 bm25(messages_fts) as rank \
                 FROM messages_fts \
                 JOIN messages m ON messages_fts.rowid = m.id \
                 JOIN sessions s ON m.session_id = s.id \
                 WHERE messages_fts MATCH ?1 AND s.conversation_key = ?2 \
                 ORDER BY rank \
                 LIMIT ?3",
                vec![
                    Box::new(match_query),
                    Box::new(conversation_key.unwrap_or("").to_string()),
                    Box::new(limit as i64),
                ],
            ),
            None => (
                "SELECT m.session_id, s.title, m.id, m.role, m.content, m.timestamp, \
                 bm25(messages_fts) as rank \
                 FROM messages_fts \
                 JOIN messages m ON messages_fts.rowid = m.id \
                 JOIN sessions s ON m.session_id = s.id \
                 WHERE messages_fts MATCH ?1 \
                 ORDER BY rank \
                 LIMIT ?2",
                vec![Box::new(match_query), Box::new(limit as i64)],
            ),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let params_iter: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|b| b.as_ref()).collect();
        let results = stmt
            .query_map(params_iter.as_slice(), |row| {
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
            conversation_key: None,
        })
    }

    /// Fork a session **without ending the parent**: create a new session with
    /// `parent_session_id` set and a single system message naming the origin.
    /// Unlike [`Self::split_session`] (written for compaction, which ends the
    /// parent), this is a user-initiated "branch from here" — the parent stays
    /// open and continuable. Returns the new child session.
    pub fn fork_session(&self, parent_id: &str, note: &str) -> Result<Session> {
        let parent = self
            .get_session(parent_id)?
            .ok_or_else(|| anyhow::anyhow!("no session with id {parent_id}"))?;
        let new_id = Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now().timestamp();

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO sessions (id, parent_session_id, model, started_at, source) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![new_id, parent_id, parent.model, started_at, parent.source],
        )?;
        tx.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls, timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![new_id, "system", note, Option::<String>::None, started_at],
        )?;
        tx.execute(
            "UPDATE sessions SET message_count = message_count + 1 WHERE id = ?1",
            params![new_id],
        )?;
        tx.commit()?;

        Ok(Session {
            id: new_id,
            title: None,
            parent_session_id: Some(parent_id.to_string()),
            model: parent.model,
            started_at,
            ended_at: None,
            message_count: 1,
            token_count: 0,
            source: parent.source,
            conversation_key: parent.conversation_key,
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

    /// `search_scoped` must fold the source predicate to a single bound
    /// parameter and exclude channel rows from the operator's free-text
    /// search by default. Both shapes share one query plan: a missing source
    /// means "anything except `channel`", and an explicit source means
    /// "exactly that source". Pin both at once so a regression that drops
    /// the predicate on either path shows up next to it.
    #[test]
    fn search_scoped_segregates_channel_rows() {
        let mut s = store();
        // API row, written via the public writer.
        let api_id = s
            .record_api_turn("m", None, "who carries sundial", "owner said it")
            .unwrap();
        // Channel row, written via the channel writer — its body matches the
        // same free-text token so the predicate is the only thing that can
        // keep them apart.
        let channel_id = s
            .record_channel_turn(
                "m",
                "telegram:chat-1",
                "channel sundial",
                "channel reply",
                None,
            )
            .unwrap();

        // Default search excludes the channel row.
        let default_hits = s.search_scoped("sundial", 10, None).unwrap();
        let default_ids: Vec<&str> = default_hits.iter().map(|r| r.session_id.as_str()).collect();
        assert!(
            default_ids.contains(&api_id.as_str()),
            "default search must include the api row"
        );
        assert!(
            !default_ids.contains(&channel_id.as_str()),
            "default search must NOT include the channel row"
        );

        // Source-pinned search returns only the channel row.
        let channel_hits = s.search_scoped("sundial", 10, Some("channel")).unwrap();
        let channel_ids: Vec<&str> = channel_hits.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(channel_hits.len(), 1);
        assert_eq!(channel_ids[0], channel_id);

        // A different source excludes both: the folded predicate is `=`, so
        // a request for `source=api` cannot accidentally return a channel
        // row.
        let api_hits = s.search_scoped("sundial", 10, Some("api")).unwrap();
        let api_ids: Vec<&str> = api_hits.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(api_hits.len(), 1);
        assert_eq!(api_ids[0], api_id);
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
    fn list_sessions_paged_breaks_started_at_ties_by_id() {
        // started_at is second-granular, so ties are routine. Without the
        // secondary `id` key SQLite's order between two OFFSET queries is
        // unspecified — pages could overlap or skip. Five of these rows share
        // one timestamp.
        let store = SessionStore::in_memory().unwrap();
        for i in 0..25 {
            let ts = if i < 5 { 100 } else { i64::from(i) + 100 };
            insert_with_id(&store, &format!("s-{i:03}"), ts);
        }
        let page1 = store.list_sessions_paged(10, 0).unwrap();
        let page2 = store.list_sessions_paged(10, 10).unwrap();
        let page3 = store.list_sessions_paged(10, 20).unwrap();
        let seen: std::collections::HashSet<_> = page1
            .iter()
            .chain(page2.iter())
            .chain(page3.iter())
            .map(|s| s.id.clone())
            .collect();
        assert_eq!(
            seen.len(),
            25,
            "tied rows must not overlap or skip across pages"
        );
    }

    #[test]
    fn search_with_quote_or_star_does_not_error() {
        // FTS5 operator characters in user input must be matched literally, not
        // parsed as query syntax (which used to 500).
        let mut store = SessionStore::in_memory().unwrap();
        store
            .record_api_turn("m", None, "hello world", "an answer")
            .unwrap();
        assert!(store.search("\"", 10).is_ok(), "bare quote must not error");
        assert!(store.search("*", 10).is_ok(), "leading star must not error");
        assert!(
            store.search("(", 10).is_ok(),
            "unbalanced paren must not error"
        );
        assert_eq!(store.search("hello", 10).unwrap().len(), 1);
    }

    #[test]
    fn search_is_literal_not_boolean() {
        // `OR` is a literal token now, not an FTS operator.
        let mut store = SessionStore::in_memory().unwrap();
        store
            .record_api_turn("m", None, "alpha beta", "reply")
            .unwrap();
        assert_eq!(
            store.search("alpha OR gamma", 10).unwrap().len(),
            0,
            "no message contains all three literal tokens"
        );
    }

    #[test]
    fn session_exists_reports_deleted_rows_as_absent() {
        let mut store = SessionStore::in_memory().unwrap();
        let id = store
            .record_api_turn("m", None, "question", "answer")
            .unwrap();
        assert!(store.session_exists(&id).unwrap());
        store.delete_session(&id).unwrap();
        assert!(!store.session_exists(&id).unwrap());
    }

    #[test]
    fn get_recent_messages_returns_the_newest_n_in_order() {
        let mut store = SessionStore::in_memory().unwrap();
        // 25 turns via record_api_turn => 50 messages under one session.
        let mut id = None;
        for i in 0..25 {
            let sid = store
                .record_api_turn("m", id.as_deref(), &format!("q{i}"), &format!("a{i}"))
                .unwrap();
            id = Some(sid);
        }
        let sid = id.unwrap();
        let recent = store.get_recent_messages(&sid, 10).unwrap();
        assert_eq!(recent.len(), 10, "capped at the requested count");
        // Oldest-first within the window: the last two turns' 4 messages are the
        // newest; the window's final message is the last assistant reply.
        assert_eq!(recent.last().unwrap().content, "a24");
        // Ascending by id within the window.
        let ids: Vec<i64> = recent.iter().map(|m| m.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "returned oldest-first");
    }

    #[test]
    fn fork_creates_a_child_and_leaves_the_parent_untouched() {
        let store = SessionStore::in_memory().unwrap();
        let parent = store.new_session("m", "api").unwrap();

        let child = store.fork_session(&parent.id, "Forked here").unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.message_count, 1);
        assert_ne!(child.id, parent.id);

        // The parent must be untouched — unlike split_session, fork does not end
        // it or write to it.
        let after = store.get_session(&parent.id).unwrap().unwrap();
        assert!(after.ended_at.is_none(), "fork must not end the parent");
        assert_eq!(after.message_count, 0, "fork must not write to the parent");

        // The child carries the origin note as its first system message.
        let msgs = store.get_messages(&child.id).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "Forked here");
    }

    #[test]
    fn fork_unknown_parent_is_an_error() {
        let store = SessionStore::in_memory().unwrap();
        assert!(store.fork_session("no-such-id", "x").is_err());
    }

    #[test]
    fn stats_counts_all_rows_not_a_page() {
        let mut store = SessionStore::in_memory().unwrap();
        // Three sessions, each with 2 messages via record_api_turn.
        for i in 0..3 {
            store
                .record_api_turn("m", None, &format!("q{i}"), &format!("a{i}"))
                .unwrap();
        }
        let stats = store.stats().unwrap();
        assert_eq!(stats.total_sessions, 3);
        assert_eq!(stats.total_messages, 6);
        assert!(stats.latest_session_id.is_some());
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

    // ── Channel recording ──────────────────────────────────────────────────

    /// The first channel turn for a key opens a new `source='channel'` session.
    /// Subsequent turns for the same key reuse it. `/new`-style close + reopen
    /// gives the next turn a fresh session.
    #[test]
    fn record_channel_turn_reuses_open_session_for_same_conversation() {
        let mut s = store();

        let first = s
            .record_channel_turn("m", "telegram:chat-1", "first user", "first reply", None)
            .unwrap();
        let second = s
            .record_channel_turn("m", "telegram:chat-1", "second user", "second reply", None)
            .unwrap();

        assert_eq!(first, second, "second turn reuses the open session");
        let sess = s.get_session(&first).unwrap().unwrap();
        assert_eq!(sess.source, "channel");
        assert_eq!(sess.conversation_key.as_deref(), Some("telegram:chat-1"));
        assert_eq!(sess.message_count, 4, "two user + two assistant");
        let msgs = s.get_messages(&first).unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].content, "first user");
        assert_eq!(msgs[1].content, "first reply");
        assert_eq!(msgs[2].content, "second user");
        assert_eq!(msgs[3].content, "second reply");
    }

    /// `/new` and `/clear` close the open session for the conversation. The
    /// next recorded message opens a fresh one — the session id must change.
    #[test]
    fn record_channel_turn_opens_a_new_session_after_close() {
        let mut s = store();

        let before = s
            .record_channel_turn("m", "telegram:chat-1", "u1", "r1", None)
            .unwrap();
        let removed = s.close_channel_session("telegram:chat-1").unwrap();
        assert_eq!(removed, 1, "the open session was closed");
        let after = s
            .record_channel_turn("m", "telegram:chat-1", "u2", "r2", None)
            .unwrap();
        assert_ne!(before, after);
        // The closed session keeps its message_count; the new one starts at 2.
        let old = s.get_session(&before).unwrap().unwrap();
        assert!(old.ended_at.is_some());
        assert_eq!(old.message_count, 2);
        let new = s.get_session(&after).unwrap().unwrap();
        assert_eq!(new.message_count, 2);
        assert!(new.ended_at.is_none());
        // Only the new session has no `ended_at` for this conversation key.
        let open = s
            .open_channel_session_id("telegram:chat-1")
            .unwrap()
            .expect("a fresh open session exists");
        assert_eq!(open, after);
    }

    /// Two different conversation keys get two different sessions — keys must
    /// not collapse across chats. The pin test for the per-conversation key
    /// semantics.
    #[test]
    fn record_channel_turn_keeps_keys_isolated() {
        let mut s = store();

        let a = s
            .record_channel_turn("m", "telegram:chat-1", "ua", "ra", None)
            .unwrap();
        let b = s
            .record_channel_turn("m", "telegram:chat-2", "ub", "rb", None)
            .unwrap();
        assert_ne!(a, b);

        let sess_a = s.get_session(&a).unwrap().unwrap();
        let sess_b = s.get_session(&b).unwrap().unwrap();
        assert_eq!(sess_a.conversation_key.as_deref(), Some("telegram:chat-1"));
        assert_eq!(sess_b.conversation_key.as_deref(), Some("telegram:chat-2"));
    }

    /// Both scrubbers run on every recorded message before the INSERT.
    /// `scrub_secret_patterns` redacts known-prefix tokens; `scrub_credentials`
    /// redacts known KV-style secrets. A token-shaped secret in user text and
    /// a KV-style secret in the reply must both come back as `[REDACTED]` on
    /// read.
    #[test]
    fn record_channel_turn_scrubs_secrets_in_both_messages() {
        let mut s = store();

        // A token with the `sk-` prefix in the user text, a JSON credential
        // pair in the reply text. Both scrubbers run, so both are redacted.
        let user_text = "here is my key: sk-abcdef1234567890XYZ";
        let reply_text = r#"saved with api_key: "supersecretvalue1234""#;

        let id = s
            .record_channel_turn("m", "telegram:chat-1", user_text, reply_text, None)
            .unwrap();
        let msgs = s.get_messages(&id).unwrap();
        let stored_user = msgs[0].content.clone();
        let stored_reply = msgs[1].content.clone();

        assert!(
            !stored_user.contains("abcdef1234567890"),
            "token-shaped value must be scrubbed: {stored_user}"
        );
        assert!(
            !stored_reply.contains("supersecretvalue1234"),
            "KV secret must be scrubbed: {stored_reply}"
        );
        assert!(
            stored_user.contains("REDACTED"),
            "the redacted marker must be present: {stored_user}"
        );
        assert!(
            stored_reply.contains("REDACTED"),
            "the redacted marker must be present: {stored_reply}"
        );
    }

    /// A non-ASCII quoted secret (CJK characters in the value) does not panic
    /// the recorder. The value-pass scrub takes a fixed-byte prefix of the
    /// captured secret; a prefix that crosses a UTF-8 char boundary panics
    /// inside `replace_all`'s closure. The closure runs on the message text
    /// inside the same transaction, before the `INSERT`, so a panic there
    /// fails the `INSERT` and leaves the in-progress turn with no recorded
    /// row, and a second turn must still record — a clean recorder answers
    /// the next call normally.
    #[test]
    fn record_channel_turn_survives_a_non_ascii_quoted_secret() {
        let mut s = store();

        let secret = r#"password: "密码密码密码密码""#;
        let id = s
            .record_channel_turn("m", "telegram:chat-1", secret, "ok", None)
            .expect("a non-ASCII quoted secret must not panic the recorder");
        let msgs = s.get_messages(&id).unwrap();
        assert!(
            !msgs[0].content.contains("密码密码密码密码"),
            "the full secret content must not be in storage: {msgs:?}"
        );
        assert!(
            msgs[0].content.contains("REDACTED"),
            "the redaction marker must be present: {msgs:?}"
        );

        // The regression case: a panic in the first call's scrub closure would
        // fail the `INSERT` for that turn. A clean recorder answers the next
        // call normally — the store is not shared behind a `Mutex` here, so
        // there is no poison state to recover from; the assertion just
        // confirms the second call also runs end to end.
        let id2 = s
            .record_channel_turn("m", "telegram:chat-1", "second", "second-reply", None)
            .expect("the second turn must still record after a non-ASCII secret");
        let msgs2 = s.get_messages(&id2).unwrap();
        assert!(
            msgs2.iter().any(|m| m.content == "second"),
            "second turn must be visible: {msgs2:?}"
        );
    }

    /// An inbound message that carries `[IMAGE:data:image/png;base64,<payload>]`
    /// is recorded with the payload replaced by a short placeholder. The base64
    /// bytes themselves are stored for thirty days and indexed for FTS unless the
    /// recording scrubber catches them; the row only keeps the kind and the fact
    /// that an image was attached.
    #[test]
    fn record_channel_turn_redacts_a_base64_image_payload_in_the_message() {
        let mut s = store();
        let payload = "A".repeat(2048);
        let user_msg = format!("here is a picture [IMAGE:data:image/png;base64,{payload}]");
        let reply = format!("got it [IMAGE:data:image/png;base64,{payload}]");

        let id = s
            .record_channel_turn("m", "telegram:chat-1", &user_msg, &reply, None)
            .unwrap();
        let msgs = s.get_messages(&id).unwrap();

        for (label, row) in [("user", &msgs[0]), ("assistant", &msgs[1])] {
            assert!(
                !row.content.contains(&payload),
                "{label} row must not contain the base64 payload: {row:?}"
            );
            assert!(
                row.content.contains("[IMAGE:"),
                "{label} row must keep the [IMAGE:] kind marker: {row:?}"
            );
            assert!(
                !row.content.contains("base64"),
                "{label} row must drop the base64 hint: {row:?}"
            );
        }
    }

    /// An attachment marker that names a path (not a data URI) is kept
    /// verbatim — paths are not payloads and there is nothing to withhold.
    #[test]
    fn record_channel_turn_keeps_path_only_attachment_markers() {
        let mut s = store();
        let user_msg = "see this [IMAGE:/w/foo.png] and [DOCUMENT:/w/notes.txt]";

        let id = s
            .record_channel_turn("m", "telegram:chat-1", user_msg, "ok", None)
            .unwrap();
        let msgs = s.get_messages(&id).unwrap();
        assert_eq!(
            msgs[0].content, user_msg,
            "path-only markers must survive: {msgs:?}"
        );
    }

    /// A title carried on the first turn becomes the session title. The
    /// second turn does not overwrite it. Channels carry the title directly
    /// when the parser already has it (e.g. Telegram `chat.title`); without
    /// one, the caller passes `None` and the title is `NULL` until set.
    #[test]
    fn record_channel_turn_first_turn_carries_chat_title() {
        let mut s = store();

        let id = s
            .record_channel_turn("m", "telegram:chat-1", "hi", "hello", Some("Family Chat"))
            .unwrap();
        let sess = s.get_session(&id).unwrap().unwrap();
        assert_eq!(sess.title.as_deref(), Some("Family Chat"));

        // Subsequent turns don't overwrite the title.
        s.record_channel_turn("m", "telegram:chat-1", "another", "reply", None)
            .unwrap();
        let after = s.get_session(&id).unwrap().unwrap();
        assert_eq!(after.title.as_deref(), Some("Family Chat"));
    }

    /// Channel sessions older than the retention window are removed. Sessions
    /// younger than it stay. Non-channel sessions are untouched no matter how
    /// old.
    #[test]
    fn prune_channel_sessions_removes_only_source_channel_past_retention() {
        let mut s = store();

        // A channel session aged past retention — both its session row and
        // every message it carries, since retention is "30 days after the
        // last turn" and a chat whose last turn was that long ago has nothing
        // left to keep.
        let old_id = s
            .record_channel_turn("m", "telegram:chat-old", "u", "r", None)
            .unwrap();
        s.conn
            .execute(
                "UPDATE sessions SET started_at = 1 WHERE id = ?1",
                params![old_id],
            )
            .unwrap();
        s.conn
            .execute(
                "UPDATE messages SET timestamp = 1 WHERE session_id = ?1",
                params![old_id],
            )
            .unwrap();

        // A fresh channel session (now() within retention).
        s.record_channel_turn("m", "telegram:chat-fresh", "u", "r", None)
            .unwrap();

        // A TUI session aged past retention — must NOT be removed.
        let tui_id = s.new_session("m", "tui").unwrap();
        s.conn
            .execute(
                "UPDATE sessions SET started_at = 1 WHERE id = ?1",
                params![&tui_id.id],
            )
            .unwrap();
        s.conn
            .execute(
                "UPDATE messages SET timestamp = 1 WHERE session_id = ?1",
                params![&tui_id.id],
            )
            .unwrap();

        let cutoff = 31_i64 * 24 * 60 * 60;
        let removed = s.prune_channel_sessions(cutoff).unwrap();
        assert_eq!(removed, 1, "only the aged channel session");

        assert!(s.get_session(&old_id).unwrap().is_none());
        // The fresh channel session and the TUI session both survive.
        assert!(s.count_sessions().unwrap() >= 2);
        assert!(s.get_session(&tui_id.id).unwrap().is_some());
    }

    /// A TUI session of any age must never be removed by
    /// `prune_channel_sessions`. Pin test for the source filter.
    #[test]
    fn prune_channel_sessions_leaves_tui_sessions_untouched() {
        let mut s = store();
        let tui_id = s.new_session("m", "tui").unwrap();
        s.conn
            .execute(
                "UPDATE sessions SET started_at = 0 WHERE id = ?1",
                params![&tui_id.id],
            )
            .unwrap();
        // 31-day retention in seconds; the row's started_at is 0.
        let removed = s.prune_channel_sessions(31_i64 * 24 * 60 * 60).unwrap();
        assert_eq!(removed, 0);
        assert!(s.get_session(&tui_id.id).unwrap().is_some());
    }

    /// The source filter on the message `DELETE` matters: a TUI session
    /// whose messages predate the cutoff must survive. The existing
    /// `prune_channel_sessions_removes_only_source_channel_past_retention`
    /// test backdates a TUI session with no messages, so dropping
    /// `source = 'channel'` from the `DELETE FROM messages` clause failed no
    /// assertion. This test seeds the TUI session with old rows and asserts
    /// they are still there after the prune — a regression that drops the
    /// source filter on the message `DELETE` would delete every TUI and API
    /// message older than the cutoff, which is a real-world data-loss bug.
    #[test]
    fn prune_channel_sessions_keeps_old_messages_of_a_tui_session() {
        let mut s = store();
        let tui_id = s.new_session("m", "tui").unwrap();
        // Seed two messages on the TUI session, backdated past the cutoff.
        s.conn
            .execute(
                "INSERT INTO messages (session_id, role, content, timestamp) \
                 VALUES (?1, 'user', 'old tui user', ?2)",
                params![&tui_id.id, 1_i64],
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO messages (session_id, role, content, timestamp) \
                 VALUES (?1, 'assistant', 'old tui reply', ?2)",
                params![&tui_id.id, 1_i64],
            )
            .unwrap();

        let removed = s.prune_channel_sessions(31_i64 * 24 * 60 * 60).unwrap();
        assert_eq!(removed, 0, "no channel session was removed");

        let msgs = s.get_messages(&tui_id.id).unwrap();
        let contents: Vec<&str> = msgs.iter().map(|m| m.content.as_str()).collect();
        assert!(
            contents.contains(&"old tui user"),
            "the TUI session's old user message must remain: {contents:?}"
        );
        assert!(
            contents.contains(&"old tui reply"),
            "the TUI session's old reply message must remain: {contents:?}"
        );
        assert_eq!(msgs.len(), 2, "no TUI messages pruned");
    }

    /// Retention counts from the last message, not from `started_at`. A chat
    /// that has been silent for thirty days keeps its fresh turn; only the
    /// messages older than the cutoff go, and the session survives with what
    /// it still has.
    #[test]
    fn prune_channel_sessions_keeps_a_session_that_had_a_recent_message() {
        let mut s = store();

        // Open the channel session, then backdate its `started_at` past the
        // retention cutoff — the chat is, on paper, old.
        let id = s
            .record_channel_turn("m", "telegram:chat-1", "old user", "old reply", None)
            .unwrap();
        s.conn
            .execute(
                "UPDATE sessions SET started_at = 1 WHERE id = ?1",
                params![id],
            )
            .unwrap();
        // Mark the first turn's rows as old so they are past the cutoff,
        // then record a fresh turn so the messages table holds both an old
        // and a fresh row, the latter with `timestamp` = now.
        s.conn
            .execute(
                "UPDATE messages SET timestamp = 1 \
                 WHERE session_id = ?1 AND content IN ('old user','old reply')",
                params![id],
            )
            .unwrap();
        s.record_channel_turn("m", "telegram:chat-1", "fresh user", "fresh reply", None)
            .unwrap();

        let cutoff = 31_i64 * 24 * 60 * 60;
        let removed = s
            .prune_channel_sessions(cutoff)
            .expect("prune must succeed");
        assert_eq!(
            removed, 0,
            "a session with a fresh message must not be removed, only its old messages pruned"
        );

        let sess = s.get_session(&id).unwrap().unwrap();
        assert!(
            sess.ended_at.is_none(),
            "the open channel session must still be open"
        );

        let msgs = s.get_messages(&id).unwrap();
        let user_contents: Vec<&str> = msgs
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            user_contents.contains(&"fresh user"),
            "the fresh user message must survive: {msgs:?}"
        );
        assert!(
            !user_contents.contains(&"old user"),
            "the old user message must be pruned: {msgs:?}"
        );
    }

    /// `prune_channel_sessions` keeps `sessions.message_count` consistent with
    /// the rows actually left behind: a session that survives with only the
    /// fresh half of its history must carry the fresh count, so the operator
    /// list (`message_count` is what `list_sessions_*` renders) and
    /// `SUM(message_count)` in `stats` agree with the messages table. Without
    /// the in-transaction recount, the list and the stats keep the old count
    /// and overstate the session; a test that only checked `get_messages`
    /// would miss the drift.
    #[test]
    fn prune_channel_sessions_updates_message_count_for_kept_rows() {
        let mut s = store();

        let id = s
            .record_channel_turn("m", "telegram:chat-1", "first user", "first reply", None)
            .unwrap();
        s.record_channel_turn("m", "telegram:chat-1", "second user", "second reply", None)
            .unwrap();
        // Two messages with `timestamp = 1` (old), two more at `now()` (fresh).
        s.conn
            .execute(
                "UPDATE messages SET timestamp = 1 \
                 WHERE session_id = ?1 AND content IN ('first user','first reply')",
                params![id],
            )
            .unwrap();

        let cutoff = 31_i64 * 24 * 60 * 60;
        let removed = s.prune_channel_sessions(cutoff).unwrap();
        assert_eq!(removed, 0, "the session keeps its fresh messages");

        let sess = s.get_session(&id).unwrap().unwrap();
        assert_eq!(
            sess.message_count, 2,
            "the kept session must report 2 messages (the fresh user + fresh reply); got {}",
            sess.message_count
        );

        let stat_msgs: i64 = s
            .conn
            .query_row(
                "SELECT COALESCE(SUM(message_count), 0) FROM sessions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stat_msgs, 2,
            "stats SUM(message_count) must reflect the pruned count, not the pre-prune 4"
        );
    }

    /// A channel session whose messages are ALL older than the cutoff is
    /// removed along with its messages — a session with no content is not
    /// worth keeping.
    #[test]
    fn prune_channel_sessions_removes_a_session_whose_messages_are_all_old() {
        let mut s = store();

        let id = s
            .record_channel_turn("m", "telegram:chat-1", "old user", "old reply", None)
            .unwrap();
        // Force both the session and its messages past the cutoff.
        s.conn
            .execute(
                "UPDATE sessions SET started_at = 1 WHERE id = ?1",
                params![id],
            )
            .unwrap();
        s.conn
            .execute(
                "UPDATE messages SET timestamp = 1 WHERE session_id = ?1",
                params![id],
            )
            .unwrap();

        let cutoff = 31_i64 * 24 * 60 * 60;
        let removed = s.prune_channel_sessions(cutoff).unwrap();
        assert_eq!(removed, 1, "the emptied session goes away");
        assert!(s.get_session(&id).unwrap().is_none());
        let msgs_left: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(msgs_left, 0, "no orphan messages left behind");
    }

    /// `close_channel_session` returns the number of sessions it closed. A
    /// second call is a no-op (the open session is gone).
    #[test]
    fn close_channel_session_is_idempotent() {
        let mut s = store();
        s.record_channel_turn("m", "telegram:chat-1", "u", "r", None)
            .unwrap();
        let first = s.close_channel_session("telegram:chat-1").unwrap();
        let second = s.close_channel_session("telegram:chat-1").unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 0);
    }

    /// `list_sessions_paged_visible` is the operator-facing default list: it
    /// excludes every row whose `source = 'channel'`, so channel transcripts
    /// stay out of the standard `/sessions`, CLI `session list`, and
    /// `GET /api/v1/sessions` calls until the caller asks for them
    /// explicitly. The other `source` values (tui, api, …) are unaffected.
    #[test]
    fn list_sessions_paged_visible_excludes_channel_sessions() {
        let mut s = store();
        let tui_id = s.new_session("m", "tui").unwrap();
        let api_id = s.new_session("m", "api").unwrap();
        s.record_channel_turn("m", "telegram:chat-1", "u", "r", None)
            .unwrap();

        let visible = s.list_sessions_paged_visible(50, 0).unwrap();
        let visible_ids: Vec<&str> = visible.iter().map(|s| s.id.as_str()).collect();
        assert!(
            visible_ids.contains(&tui_id.id.as_str()),
            "tui session must remain visible"
        );
        assert!(
            visible_ids.contains(&api_id.id.as_str()),
            "api session must remain visible"
        );
        assert!(
            !visible_ids
                .iter()
                .any(|id| s.get_session(id).unwrap().unwrap().source == "channel"),
            "no channel row in the visible list"
        );

        // And the total count agrees with the page count.
        assert_eq!(s.count_sessions_visible().unwrap(), 2);
    }

    /// `list_sessions_paged_with_source(_, _, Some("channel"))` returns
    /// channel sessions and nothing else, which is the surface-level
    /// `?source=channel` filter.
    #[test]
    fn list_sessions_paged_with_source_returns_only_channel_rows() {
        let mut s = store();
        s.new_session("m", "tui").unwrap();
        s.record_channel_turn("m", "telegram:chat-1", "u", "r", None)
            .unwrap();
        s.record_channel_turn("m", "discord:chat-2", "u", "r", None)
            .unwrap();

        let only_channel = s
            .list_sessions_paged_with_source(50, 0, Some("channel"))
            .unwrap();
        assert_eq!(only_channel.len(), 2);
        assert!(only_channel.iter().all(|s| s.source == "channel"));
    }

    /// `search_with_conversation(Some(key))` returns only matches whose session
    /// `conversation_key` equals `key`. A session with a different key is
    /// invisible to the call even when its message text contains the query,
    /// because the conversation scope is the only thing being narrowed.
    #[test]
    fn search_with_conversation_some_returns_only_rows_of_given_key() {
        let mut s = store();

        let other = s
            .record_channel_turn("m", "telegram:chat-1", "matching word", "r", None)
            .unwrap();
        let mine = s
            .record_channel_turn("m", "telegram:chat-2", "matching word", "r", None)
            .unwrap();

        let scoped = s
            .search_with_conversation("matching", 10, Some("telegram:chat-2"))
            .unwrap();
        let session_ids: Vec<&str> = scoped.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(
            session_ids,
            vec![mine.as_str()],
            "only the matching conversation key surfaces, the other is filtered: got {session_ids:?}"
        );
        assert!(
            !session_ids.contains(&other.as_str()),
            "the other conversation key is invisible: got {session_ids:?}"
        );
    }

    /// `search_with_conversation(None)` keeps the pre-existing FTS behaviour:
    /// every matching row, irrespective of conversation key.
    #[test]
    fn search_with_conversation_none_searches_all_keys() {
        let mut s = store();

        s.record_channel_turn("m", "telegram:chat-1", "matching word", "r", None)
            .unwrap();
        s.record_channel_turn("m", "telegram:chat-2", "matching word", "r", None)
            .unwrap();

        let hits = s.search_with_conversation("matching", 10, None).unwrap();
        assert_eq!(
            hits.len(),
            2,
            "an unscoped search must see both conversations"
        );
    }

    /// `search_any_word_with_conversation` returns rows whose message matches
    /// at least one of the query tokens. A row whose text holds none of the
    /// words is invisible; a row that holds one is surfaced even when the
    /// all-words pass would return nothing.
    #[test]
    fn search_any_word_finds_rows_with_one_matching_token() {
        let mut s = store();

        s.record_channel_turn(
            "m",
            "telegram:chat-1",
            "the quick brown fox jumps",
            "yes",
            None,
        )
        .unwrap();
        s.record_channel_turn(
            "m",
            "telegram:chat-2",
            "an unrelated message here",
            "no",
            None,
        )
        .unwrap();

        // Only chat-1 carries any of `fox`, `strawberry`, `apple`.
        let hits = s
            .search_any_word_with_conversation("strawberry fox apple", 10, None)
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "only the chat with one matching word is found: {hits:?}"
        );
        assert!(
            hits[0].content.contains("fox"),
            "the matching word is the hit: {hits:?}"
        );
    }

    /// `search_any_word_with_conversation(Some(key))` is the any-word fallback
    /// the tool uses under a scoped view: a row from another conversation
    /// must be invisible even when it carries one of the query tokens, because
    /// the FTS query and the conversation filter are both applied together.
    /// The all-words twin (`search_with_conversation`) has the same test
    /// (`search_with_conversation_some_returns_only_rows_of_given_key`); this
    /// test pins the same invariant on the OR-joined any-word statement, so a
    /// regression that drops `AND s.conversation_key = ?2` from that branch
    /// fires here.
    #[test]
    fn search_any_word_with_conversation_some_returns_only_rows_of_given_key() {
        let mut s = store();

        let other = s
            .record_channel_turn("m", "telegram:chat-1", "matching word", "r", None)
            .unwrap();
        let mine = s
            .record_channel_turn("m", "telegram:chat-2", "matching word", "r", None)
            .unwrap();

        // A query that includes "matching" only. The scoped search must
        // surface the row whose conversation_key is chat-2 and nothing else.
        let scoped = s
            .search_any_word_with_conversation("matching", 10, Some("telegram:chat-2"))
            .unwrap();
        let session_ids: Vec<&str> = scoped.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(
            session_ids,
            vec![mine.as_str()],
            "only the matching conversation key surfaces, the other is filtered: got {session_ids:?}"
        );
        assert!(
            !session_ids.contains(&other.as_str()),
            "the other conversation key is invisible: got {session_ids:?}"
        );
    }
}
