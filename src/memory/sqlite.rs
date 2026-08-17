use super::embeddings::EmbeddingProvider;
use super::traits::{Memory, MemoryCategory, MemoryEntry};
use super::vector;
use anyhow::Context;
use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use uuid::Uuid;

/// Maximum allowed open timeout (seconds) to avoid unreasonable waits.
const SQLITE_OPEN_TIMEOUT_CAP_SECS: u64 = 300;

/// Re-render an RFC3339 timestamp in UTC, whatever offset it carries.
///
/// Returns `None` for anything that is not RFC3339 so callers can keep the
/// original rather than substitute a guess.
fn to_utc_rfc3339(raw: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|ts| ts.with_timezone(&Utc).to_rfc3339())
}

/// SQLite-backed persistent memory — the brain
///
/// Full-stack search engine:
/// - **Vector DB**: embeddings stored as BLOB, cosine similarity search
/// - **Keyword Search**: FTS5 virtual table with BM25 scoring
/// - **Hybrid Merge**: weighted fusion of vector + keyword results
/// - **Embedding Cache**: LRU-evicted cache to avoid redundant API calls
/// - **Safe Reindex**: temp DB → seed → sync → atomic swap → rollback
pub struct SqliteMemory {
    conn: Arc<Mutex<Connection>>,
    db_path: PathBuf,
    embedder: Arc<dyn EmbeddingProvider>,
    vector_weight: f32,
    keyword_weight: f32,
    cache_max: usize,
}

impl SqliteMemory {
    pub fn new(workspace_dir: &Path) -> anyhow::Result<Self> {
        Self::with_embedder(
            workspace_dir,
            Arc::new(super::embeddings::NoopEmbedding),
            0.7,
            0.3,
            10_000,
            None,
        )
    }

    /// Build SQLite memory with optional open timeout.
    ///
    /// If `open_timeout_secs` is `Some(n)`, opening the database is limited to `n` seconds
    /// (capped at 300). Useful when the DB file may be locked or on slow storage.
    /// `None` = wait indefinitely (default).
    pub fn with_embedder(
        workspace_dir: &Path,
        embedder: Arc<dyn EmbeddingProvider>,
        vector_weight: f32,
        keyword_weight: f32,
        cache_max: usize,
        open_timeout_secs: Option<u64>,
    ) -> anyhow::Result<Self> {
        let db_path = workspace_dir.join("memory").join("brain.db");

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Self::open_connection(&db_path, open_timeout_secs)?;

        // ── Production-grade PRAGMA tuning ──────────────────────
        // WAL mode: concurrent reads during writes, crash-safe
        // normal sync: 2× write speed, still durable on WAL
        // mmap 8 MB: let the OS page-cache serve hot reads
        // cache 2 MB: keep ~500 hot pages in-process
        // temp_store memory: temp tables never hit disk
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous  = NORMAL;
             PRAGMA mmap_size    = 8388608;
             PRAGMA cache_size   = -2000;
             PRAGMA temp_store   = MEMORY;",
        )?;

        Self::init_schema(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path,
            embedder,
            vector_weight,
            keyword_weight,
            cache_max,
        })
    }

    /// Open SQLite connection, optionally with a timeout (for locked/slow storage).
    fn open_connection(
        db_path: &Path,
        open_timeout_secs: Option<u64>,
    ) -> anyhow::Result<Connection> {
        let path_buf = db_path.to_path_buf();

        let conn = if let Some(secs) = open_timeout_secs {
            let capped = secs.min(SQLITE_OPEN_TIMEOUT_CAP_SECS);
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let result = Connection::open(&path_buf);
                let _ = tx.send(result);
            });
            match rx.recv_timeout(Duration::from_secs(capped)) {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => return Err(e).context("SQLite failed to open database"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    anyhow::bail!("SQLite connection open timed out after {} seconds", capped);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("SQLite open thread exited unexpectedly");
                }
            }
        } else {
            Connection::open(&path_buf).context("SQLite failed to open database")?
        };

        Ok(conn)
    }

    /// Initialize all tables: memories, FTS5, `embedding_cache`
    /// The single source of truth for the memory schema.
    ///
    /// `pub(crate)` so snapshot hydration can create a database the backend can
    /// actually open. A second declaration elsewhere drifts and breaks startup.
    pub(crate) fn init_schema(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "-- Core memories table
            CREATE TABLE IF NOT EXISTS memories (
                id          TEXT PRIMARY KEY,
                key         TEXT NOT NULL UNIQUE,
                content     TEXT NOT NULL,
                category    TEXT NOT NULL DEFAULT 'core',
                embedding   BLOB,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memories_category ON memories(category);
            CREATE INDEX IF NOT EXISTS idx_memories_key ON memories(key);

            -- FTS5 full-text search (BM25 scoring)
            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                key, content, content=memories, content_rowid=rowid
            );

            -- FTS5 triggers: keep in sync with memories table
            CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, key, content)
                VALUES (new.rowid, new.key, new.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content)
                VALUES ('delete', old.rowid, old.key, old.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content)
                VALUES ('delete', old.rowid, old.key, old.content);
                INSERT INTO memories_fts(rowid, key, content)
                VALUES (new.rowid, new.key, new.content);
            END;

            -- Embedding cache with LRU eviction
            CREATE TABLE IF NOT EXISTS embedding_cache (
                content_hash TEXT PRIMARY KEY,
                embedding    BLOB NOT NULL,
                created_at   TEXT NOT NULL,
                accessed_at  TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_cache_accessed ON embedding_cache(accessed_at);",
        )?;

        // Column migrations: read the table definition once, then add whatever is
        // missing. Safe to run repeatedly.
        let table_sql: String = conn
            .prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name='memories'")?
            .query_row([], |row| row.get::<_, String>(0))?;

        if !table_sql.contains("session_id") {
            conn.execute_batch(
                "ALTER TABLE memories ADD COLUMN session_id TEXT;
                 CREATE INDEX IF NOT EXISTS idx_memories_session ON memories(session_id);",
            )?;
        }

        // Which embedder produced `embedding`. Without it, swapping models leaves
        // vectors of a foreign dimensionality that `cosine_similarity` scores 0.0
        // silently. NULL means "written before this column existed".
        if !table_sql.contains("embedding_model") {
            conn.execute_batch(
                "ALTER TABLE memories ADD COLUMN embedding_model TEXT;
                 ALTER TABLE memories ADD COLUMN embedding_dims INTEGER;",
            )?;
        }

        Self::migrate_timestamps_to_utc(conn)?;

        Ok(())
    }

    /// Memory schema version, tracked in `PRAGMA user_version`.
    ///
    /// - `0` — pre-migration: timestamps carry the writing machine's UTC offset.
    /// - `1` — `created_at` / `updated_at` are canonical UTC.
    const SCHEMA_VERSION: i64 = 1;

    /// Rewrite stored timestamps into UTC, once.
    ///
    /// Timestamps used to be written as `Local::now().to_rfc3339()` and compared
    /// lexicographically by the hygiene pass. Two machines with different offsets
    /// — a UTC container writing, a `+07:00` host pruning — make that comparison
    /// disagree with real time, and it errs toward deleting rows early. Canonical
    /// UTC makes the comparison sound again.
    pub(crate) fn migrate_timestamps_to_utc(conn: &Connection) -> anyhow::Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version >= Self::SCHEMA_VERSION {
            return Ok(());
        }

        // Rebuild the FTS index before touching rows. The timestamp rewrite below
        // fires `memories_au`, which deletes the old index entry — and that errors
        // with SQLITE_CORRUPT_VTAB if the entry is missing or misaligned. Databases
        // hydrated by the pre-056 snapshot path can be in exactly that state, since
        // it inserted into the index by hand without the triggers. Rebuilding
        // repairs them and makes the update path safe; it runs once, under the same
        // version guard.
        conn.execute_batch("INSERT INTO memories_fts(memories_fts) VALUES('rebuild');")?;

        let rows: Vec<(String, String, String)> = {
            let mut stmt = conn.prepare("SELECT id, created_at, updated_at FROM memories")?;
            let mapped = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut rewritten = 0_usize;
        for (id, created, updated) in rows {
            let created_utc = to_utc_rfc3339(&created);
            let updated_utc = to_utc_rfc3339(&updated);
            // A value we cannot parse is left exactly as it is — guessing at an
            // instant is worse than an untouched row.
            if created_utc.is_none() && updated_utc.is_none() {
                continue;
            }
            conn.execute(
                "UPDATE memories SET created_at = ?1, updated_at = ?2 WHERE id = ?3",
                params![
                    created_utc.unwrap_or(created),
                    updated_utc.unwrap_or(updated),
                    id
                ],
            )?;
            rewritten += 1;
        }

        if rewritten > 0 {
            tracing::info!("memory schema: normalised {rewritten} timestamp(s) to UTC");
        }
        conn.pragma_update(None, "user_version", Self::SCHEMA_VERSION)?;

        Ok(())
    }

    fn category_to_str(cat: &MemoryCategory) -> String {
        match cat {
            MemoryCategory::Core => "core".into(),
            MemoryCategory::Daily => "daily".into(),
            MemoryCategory::Conversation => "conversation".into(),
            MemoryCategory::Custom(name) => name.clone(),
        }
    }

    fn str_to_category(s: &str) -> MemoryCategory {
        match s {
            "core" => MemoryCategory::Core,
            "daily" => MemoryCategory::Daily,
            "conversation" => MemoryCategory::Conversation,
            other => MemoryCategory::Custom(other.to_string()),
        }
    }

    /// Deterministic cache key for one embedding.
    ///
    /// Keyed on the embedder as well as the text: the same sentence under a
    /// different model is a different vector in a different space, and often a
    /// different length. Hashing text alone meant a model switch served the
    /// previous model's vector as a cache hit.
    ///
    /// Uses SHA-256 (truncated) instead of DefaultHasher, which is
    /// explicitly documented as unstable across Rust versions.
    fn content_hash(model: &str, dims: usize, text: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(model.as_bytes());
        hasher.update(b"|");
        hasher.update(dims.to_string().as_bytes());
        hasher.update(b"|");
        hasher.update(text.as_bytes());
        let hash = hasher.finalize();
        // First 8 bytes → 16 hex chars, matching previous format length
        format!(
            "{:016x}",
            u64::from_be_bytes(
                hash[..8]
                    .try_into()
                    .expect("SHA-256 always produces >= 8 bytes")
            )
        )
    }

    /// Get embedding from cache, or compute + cache it
    async fn get_or_compute_embedding(&self, text: &str) -> anyhow::Result<Option<Vec<f32>>> {
        if self.embedder.dimensions() == 0 {
            return Ok(None); // Noop embedder
        }

        let hash = Self::content_hash(self.embedder.name(), self.embedder.dimensions(), text);
        let now = Utc::now().to_rfc3339();

        // Check cache (offloaded to blocking thread)
        let conn = self.conn.clone();
        let hash_c = hash.clone();
        let now_c = now.clone();
        let cached = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vec<f32>>> {
            let conn = conn.lock();
            let mut stmt =
                conn.prepare("SELECT embedding FROM embedding_cache WHERE content_hash = ?1")?;
            let blob: Option<Vec<u8>> = stmt.query_row(params![hash_c], |row| row.get(0)).ok();
            if let Some(bytes) = blob {
                conn.execute(
                    "UPDATE embedding_cache SET accessed_at = ?1 WHERE content_hash = ?2",
                    params![now_c, hash_c],
                )?;
                return Ok(Some(vector::bytes_to_vec(&bytes)));
            }
            Ok(None)
        })
        .await??;

        if cached.is_some() {
            return Ok(cached);
        }

        // Compute embedding (async I/O)
        let embedding = self.embedder.embed_one(text).await?;
        let bytes = vector::vec_to_bytes(&embedding);

        // Store in cache + LRU eviction (offloaded to blocking thread)
        let conn = self.conn.clone();
        #[allow(clippy::cast_possible_wrap)]
        let cache_max = self.cache_max as i64;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = conn.lock();
            conn.execute(
                "INSERT OR REPLACE INTO embedding_cache (content_hash, embedding, created_at, accessed_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![hash, bytes, now, now],
            )?;
            conn.execute(
                "DELETE FROM embedding_cache WHERE content_hash IN (
                    SELECT content_hash FROM embedding_cache
                    ORDER BY accessed_at ASC
                    LIMIT MAX(0, (SELECT COUNT(*) FROM embedding_cache) - ?1)
                )",
                params![cache_max],
            )?;
            Ok(())
        })
        .await??;

        Ok(Some(embedding))
    }

    /// FTS5 BM25 keyword search
    fn fts5_search(
        conn: &Connection,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<(String, f32)>> {
        let fts_query = Self::build_fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }

        // The session predicate belongs in the query, not after it. Filtering the
        // global top-N afterwards means a conversation's own memories are only
        // findable when they happen to outrank every other conversation's — on a
        // busy database a scoped recall came back empty while matching rows sat
        // in the table.
        let sql = if session_id.is_some() {
            "SELECT m.id, m.key, m.content
             FROM memories_fts f
             JOIN memories m ON m.rowid = f.rowid
             WHERE memories_fts MATCH ?1 AND m.session_id = ?3
             ORDER BY bm25(memories_fts)
             LIMIT ?2"
        } else {
            "SELECT m.id, m.key, m.content
             FROM memories_fts f
             JOIN memories m ON m.rowid = f.rowid
             WHERE memories_fts MATCH ?1
             ORDER BY bm25(memories_fts)
             LIMIT ?2"
        };

        let mut stmt = conn.prepare(sql)?;
        #[allow(clippy::cast_possible_wrap)]
        let limit_i64 = limit as i64;

        // BM25 orders the hits (it is IDF-aware), but it cannot be the score:
        // its magnitude is corpus-dependent and lands near zero on small
        // stores, where most rows share the query's terms. Query coverage is
        // the absolute [0, 1] relevance — the same measure the LIKE fallback
        // and the markdown backend already use.
        let terms = Self::coverage_terms(query);
        let map_row = |row: &rusqlite::Row| -> rusqlite::Result<(String, f32)> {
            let id: String = row.get(0)?;
            let key: String = row.get(1)?;
            let content: String = row.get(2)?;
            #[allow(clippy::cast_possible_truncation)]
            Ok((id, Self::query_coverage(&terms, &key, &content) as f32))
        };

        let mut results = Vec::new();
        match session_id {
            Some(sid) => {
                let rows = stmt.query_map(params![fts_query, limit_i64, sid], map_row)?;
                for row in rows {
                    results.push(row?);
                }
            }
            None => {
                let rows = stmt.query_map(params![fts_query, limit_i64], map_row)?;
                for row in rows {
                    results.push(row?);
                }
            }
        }
        Ok(results)
    }

    /// Query terms used for coverage scoring — lowercased whitespace tokens,
    /// capped like the LIKE fallback's keyword list.
    fn coverage_terms(query: &str) -> Vec<String> {
        const MAX_COVERAGE_TERMS: usize = 8;
        query
            .split_whitespace()
            .take(MAX_COVERAGE_TERMS)
            .map(str::to_lowercase)
            .collect()
    }

    /// Fraction of the query's terms present in `key`/`content` — the absolute
    /// keyword relevance shared by the FTS path and the LIKE fallback. `1.0`
    /// means the row covers the whole query, not "best of its set".
    #[allow(clippy::cast_precision_loss)]
    fn query_coverage(terms: &[String], key: &str, content: &str) -> f64 {
        if terms.is_empty() {
            return 0.0;
        }
        let haystack = format!("{key} {content}").to_lowercase();
        let matched = terms
            .iter()
            .filter(|term| haystack.contains(term.as_str()))
            .count();
        matched as f64 / terms.len() as f64
    }

    /// Build the FTS5 MATCH expression for a free-text query.
    ///
    /// Each term becomes a quoted string literal so punctuation cannot be read
    /// as FTS5 syntax. A `"` inside a term is escaped by doubling it, per FTS5's
    /// string-literal rules — left raw it closed the literal early and produced
    /// an expression the parser rejected, which silently demoted the whole query
    /// to the substring fallback.
    fn build_fts_query(query: &str) -> String {
        query
            .split_whitespace()
            .map(|w| w.replace('"', "\"\""))
            .filter(|w| !w.is_empty())
            .map(|w| format!("\"{w}\""))
            .collect::<Vec<_>>()
            .join(" OR ")
    }

    /// Vector similarity search: scan embeddings and compute cosine similarity.
    ///
    /// Optional `category` and `session_id` filters reduce full-table scans
    /// when the caller already knows the scope of relevant memories.
    fn vector_search(
        conn: &Connection,
        query_embedding: &[f32],
        limit: usize,
        category: Option<&str>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<(String, f32)>> {
        let mut sql =
            "SELECT id, embedding, embedding_dims FROM memories WHERE embedding IS NOT NULL"
                .to_string();
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        if let Some(cat) = category {
            let _ = write!(sql, " AND category = ?{idx}");
            param_values.push(Box::new(cat.to_string()));
            idx += 1;
        }
        if let Some(sid) = session_id {
            let _ = write!(sql, " AND session_id = ?{idx}");
            param_values.push(Box::new(sid.to_string()));
        }

        let mut stmt = conn.prepare(&sql)?;
        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(AsRef::as_ref).collect();
        let rows = stmt.query_map(params_ref.as_slice(), |row| {
            let id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            let dims: Option<i64> = row.get(2)?;
            Ok((id, blob, dims))
        })?;

        let mut scored: Vec<(String, f32)> = Vec::new();
        let mut foreign = 0_usize;
        for row in rows {
            let (id, blob, stored_dims) = row?;
            // A vector from a different embedder is not comparable: cosine
            // similarity returns 0.0 on a length mismatch without saying so, which
            // reads as "no match" rather than "wrong index". Skip it and count it,
            // so the operator gets told to reindex instead of silently losing
            // vector recall. Rows predating the provenance columns carry NULL and
            // are judged on vector length alone, as before.
            let comparable = match stored_dims {
                Some(d) => usize::try_from(d).is_ok_and(|d| d == query_embedding.len()),
                None => true,
            };
            if !comparable {
                foreign += 1;
                continue;
            }
            let emb = vector::bytes_to_vec(&blob);
            let sim = vector::cosine_similarity(query_embedding, &emb);
            if sim > 0.0 {
                scored.push((id, sim));
            }
        }

        if foreign > 0 {
            tracing::warn!(
                "vector search skipped {foreign} memor{} embedded by a different model; \
                 run `rantaiclaw memory reindex` to re-embed them",
                if foreign == 1 { "y" } else { "ies" }
            );
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    /// Rebuild the FTS index and re-embed anything the live embedder cannot use.
    ///
    /// Two kinds of row qualify: one that never got an embedding — a write that
    /// happened while the provider was unavailable — and one embedded by a
    /// different model, which `vector_search` skips because a vector of another
    /// dimensionality is not comparable. Both are otherwise permanent: nothing
    /// re-embeds on its own.
    ///
    /// Returns how many rows were re-embedded.
    pub async fn reindex(&self) -> anyhow::Result<usize> {
        // Step 1: Rebuild FTS5
        {
            let conn = self.conn.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let conn = conn.lock();
                conn.execute_batch("INSERT INTO memories_fts(memories_fts) VALUES('rebuild');")?;
                Ok(())
            })
            .await??;
        }

        // Step 2: Re-embed all memories that lack embeddings
        if self.embedder.dimensions() == 0 {
            return Ok(0);
        }

        let conn = self.conn.clone();
        #[allow(clippy::cast_possible_wrap)]
        let live_dims = self.embedder.dimensions() as i64;
        let entries: Vec<(String, String)> = tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            // Rows with no embedding, and rows carrying one from a different
            // model — the latter are skipped by vector search and would stay
            // invisible to it forever otherwise.
            let mut stmt = conn.prepare(
                "SELECT id, content FROM memories
                 WHERE embedding IS NULL
                    OR embedding_dims IS NULL
                    OR embedding_dims != ?1",
            )?;
            let rows = stmt.query_map(params![live_dims], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            Ok::<_, anyhow::Error>(rows.filter_map(std::result::Result::ok).collect())
        })
        .await??;

        let mut count = 0;
        for (id, content) in &entries {
            if let Ok(Some(emb)) = self.get_or_compute_embedding(content).await {
                let bytes = vector::vec_to_bytes(&emb);
                let conn = self.conn.clone();
                let id = id.clone();
                let model = self.embedder.name().to_string();
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let conn = conn.lock();
                    // Stamp provenance alongside the vector. Without it the row
                    // would be re-embedded again on the next run and still be
                    // skipped by vector search in between.
                    conn.execute(
                        "UPDATE memories
                         SET embedding = ?1, embedding_model = ?2, embedding_dims = ?3
                         WHERE id = ?4",
                        params![bytes, model, live_dims, id],
                    )?;
                    Ok(())
                })
                .await??;
                count += 1;
            }
        }

        Ok(count)
    }
}

#[async_trait]
impl Memory for SqliteMemory {
    fn name(&self) -> &str {
        "sqlite"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // Compute embedding (async, before blocking work).
        //
        // A provider failure degrades rather than failing the write. Memory is an
        // auxiliary capability: refusing to store a message because a third-party
        // embedding endpoint is rate-limiting loses the message outright, and the
        // callers make that invisible — auto-save discards the error. The row is
        // still keyword-searchable, and its provenance columns stay NULL, which is
        // exactly what a later re-embedding pass looks for. Documented departure
        // from fail-fast, announced in the log rather than silent.
        let embedding_bytes = match self.get_or_compute_embedding(content).await {
            Ok(emb) => emb.map(|e| vector::vec_to_bytes(&e)),
            Err(e) => {
                tracing::warn!("embedding unavailable, storing memory without a vector: {e}");
                None
            }
        };

        let conn = self.conn.clone();
        let key = key.to_string();
        let content = content.to_string();
        let sid = session_id.map(String::from);
        // Record which embedder produced the vector, so a later model switch can
        // be detected instead of silently scoring every comparison 0.0.
        let (emb_model, emb_dims) = if embedding_bytes.is_some() {
            (
                Some(self.embedder.name().to_string()),
                Some(self.embedder.dimensions()),
            )
        } else {
            (None, None)
        };

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = conn.lock();
            let now = Utc::now().to_rfc3339();
            let cat = Self::category_to_str(&category);
            let id = Uuid::new_v4().to_string();
            let emb_dims = emb_dims.map(|d| i64::try_from(d).unwrap_or(i64::MAX));

            conn.execute(
                "INSERT INTO memories (id, key, content, category, embedding, created_at, updated_at, session_id, embedding_model, embedding_dims)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(key) DO UPDATE SET
                    content = excluded.content,
                    category = excluded.category,
                    embedding = excluded.embedding,
                    embedding_model = excluded.embedding_model,
                    embedding_dims = excluded.embedding_dims,
                    updated_at = excluded.updated_at,
                    session_id = excluded.session_id",
                params![
                    id,
                    key,
                    content,
                    cat,
                    embedding_bytes,
                    now,
                    now,
                    sid,
                    emb_model,
                    emb_dims
                ],
            )?;
            Ok(())
        })
        .await?
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Compute query embedding (async, before blocking work). As in `store`, a
        // provider failure degrades to keyword-only rather than failing the read —
        // `build_memory_context` swallows an `Err`, so propagating one here reads
        // to the user as "the agent has no memory".
        let query_embedding = match self.get_or_compute_embedding(query).await {
            Ok(emb) => emb,
            Err(e) => {
                tracing::warn!("embedding unavailable, recalling by keyword only: {e}");
                None
            }
        };

        let conn = self.conn.clone();
        let query = query.to_string();
        let sid = session_id.map(String::from);
        let vector_weight = self.vector_weight;
        let keyword_weight = self.keyword_weight;

        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let conn = conn.lock();
            let session_ref = sid.as_deref();

            // FTS5 BM25 keyword search. A failure here is a real failure — an
            // unparseable MATCH expression, a damaged index — and used to be
            // indistinguishable from "nothing matched", which quietly demoted the
            // query to the substring fallback. Say so, then degrade as before.
            let keyword_results = match Self::fts5_search(&conn, &query, limit * 2, session_ref) {
                Ok(hits) => hits,
                Err(e) => {
                    tracing::warn!("memory keyword search failed, falling back: {e}");
                    Vec::new()
                }
            };

            // Vector similarity search (if embeddings available)
            let vector_results = if let Some(ref qe) = query_embedding {
                match Self::vector_search(&conn, qe, limit * 2, None, session_ref) {
                    Ok(hits) => hits,
                    Err(e) => {
                        tracing::warn!("memory vector search failed, using keyword results: {e}");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };

            // Hybrid merge
            let merged = if vector_results.is_empty() {
                // `fts5_search` already scored each hit by query coverage —
                // absolute [0, 1], the same scale the hybrid path uses.
                keyword_results
                    .iter()
                    .map(|(id, score)| vector::ScoredResult {
                        id: id.clone(),
                        vector_score: None,
                        keyword_score: Some(*score),
                        final_score: *score,
                    })
                    .collect::<Vec<_>>()
            } else {
                vector::hybrid_merge(
                    &vector_results,
                    &keyword_results,
                    vector_weight,
                    keyword_weight,
                    limit,
                )
            };

            // Fetch full entries for merged results in a single query
            // instead of N round-trips (N+1 pattern).
            let mut results = Vec::new();
            if !merged.is_empty() {
                let placeholders: String = (1..=merged.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT id, key, content, category, created_at, session_id \
                     FROM memories WHERE id IN ({placeholders})"
                );
                let mut stmt = conn.prepare(&sql)?;
                let id_params: Vec<Box<dyn rusqlite::types::ToSql>> = merged
                    .iter()
                    .map(|s| Box::new(s.id.clone()) as Box<dyn rusqlite::types::ToSql>)
                    .collect();
                let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                    id_params.iter().map(AsRef::as_ref).collect();
                let rows = stmt.query_map(params_ref.as_slice(), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                })?;

                let mut entry_map = std::collections::HashMap::new();
                for row in rows {
                    let (id, key, content, cat, ts, sid) = row?;
                    entry_map.insert(id, (key, content, cat, ts, sid));
                }

                for scored in &merged {
                    if let Some((key, content, cat, ts, sid)) = entry_map.remove(&scored.id) {
                        let entry = MemoryEntry {
                            id: scored.id.clone(),
                            key,
                            content,
                            category: Self::str_to_category(&cat),
                            timestamp: ts,
                            session_id: sid,
                            score: Some(f64::from(scored.final_score)),
                        };
                        if let Some(filter_sid) = session_ref {
                            if entry.session_id.as_deref() != Some(filter_sid) {
                                continue;
                            }
                        }
                        results.push(entry);
                    }
                }
            }

            // If hybrid returned nothing, fall back to LIKE search.
            // Cap keyword count so we don't create too many SQL shapes,
            // which helps prepared-statement cache efficiency.
            if results.is_empty() {
                const MAX_LIKE_KEYWORDS: usize = 8;
                // Kept alongside the `%…%` patterns so each row can be scored by
                // how many of these terms it actually contains.
                let keyword_terms: Vec<String> = query
                    .split_whitespace()
                    .take(MAX_LIKE_KEYWORDS)
                    .map(str::to_lowercase)
                    .collect();
                let keywords: Vec<String> =
                    keyword_terms.iter().map(|w| format!("%{w}%")).collect();
                if !keywords.is_empty() {
                    let conditions: Vec<String> = keywords
                        .iter()
                        .enumerate()
                        .map(|(i, _)| {
                            format!("(content LIKE ?{} OR key LIKE ?{})", i * 2 + 1, i * 2 + 2)
                        })
                        .collect();
                    let where_clause = conditions.join(" OR ");
                    // Scope in SQL here too, for the same reason as the FTS path:
                    // `ORDER BY updated_at DESC LIMIT n` applied globally can fill
                    // the whole limit with other sessions' rows before the filter
                    // ever runs.
                    let limit_idx = keywords.len() * 2 + 1;
                    let (scope_clause, session_idx) = if session_ref.is_some() {
                        (
                            format!(" AND session_id = ?{}", limit_idx + 1),
                            Some(limit_idx + 1),
                        )
                    } else {
                        (String::new(), None)
                    };
                    let sql = format!(
                        "SELECT id, key, content, category, created_at, session_id FROM memories
                         WHERE ({where_clause}){scope_clause}
                         ORDER BY updated_at DESC
                         LIMIT ?{limit_idx}"
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                    for kw in &keywords {
                        param_values.push(Box::new(kw.clone()));
                        param_values.push(Box::new(kw.clone()));
                    }
                    #[allow(clippy::cast_possible_wrap)]
                    param_values.push(Box::new(limit as i64));
                    if session_idx.is_some() {
                        if let Some(sid) = session_ref {
                            param_values.push(Box::new(sid.to_string()));
                        }
                    }
                    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                        param_values.iter().map(AsRef::as_ref).collect();
                    let rows = stmt.query_map(params_ref.as_slice(), |row| {
                        let key: String = row.get(1)?;
                        let content: String = row.get(2)?;
                        // A substring scan has no ranking of its own. Score it
                        // by query coverage — the same absolute measure the
                        // FTS path uses, so the two paths share one scale.
                        let coverage = Self::query_coverage(&keyword_terms, &key, &content);
                        Ok(MemoryEntry {
                            id: row.get(0)?,
                            key,
                            content,
                            category: Self::str_to_category(&row.get::<_, String>(3)?),
                            timestamp: row.get(4)?,
                            session_id: row.get(5)?,
                            score: Some(coverage),
                        })
                    })?;
                    for row in rows {
                        let entry = row?;
                        if let Some(sid) = session_ref {
                            if entry.session_id.as_deref() != Some(sid) {
                                continue;
                            }
                        }
                        results.push(entry);
                    }
                }
            }

            // Every path out of `recall` now yields absolute [0, 1] scores —
            // saturated BM25, cosine, query coverage — so there is nothing to
            // rescale: a set of weak hits stays weak, which is what lets the
            // relevance floor reject it whole.
            results.truncate(limit);
            Ok(results)
        })
        .await?
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let conn = self.conn.clone();
        let key = key.to_string();

        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<MemoryEntry>> {
            let conn = conn.lock();
            let mut stmt = conn.prepare(
                "SELECT id, key, content, category, created_at, session_id FROM memories WHERE key = ?1",
            )?;

            let mut rows = stmt.query_map(params![key], |row| {
                Ok(MemoryEntry {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    content: row.get(2)?,
                    category: Self::str_to_category(&row.get::<_, String>(3)?),
                    timestamp: row.get(4)?,
                    session_id: row.get(5)?,
                    score: None,
                })
            })?;

            match rows.next() {
                Some(Ok(entry)) => Ok(Some(entry)),
                _ => Ok(None),
            }
        })
        .await?
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        // Callers render `list().len()` as a total. It is not one past this
        // cap — `count()` is — so anything reporting a total has to ask for it
        // rather than trusting the length of this page.
        const DEFAULT_LIST_LIMIT: i64 = 1000;

        let conn = self.conn.clone();
        let category = category.cloned();
        let sid = session_id.map(String::from);

        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MemoryEntry>> {
            let conn = conn.lock();
            let session_ref = sid.as_deref();
            let mut results = Vec::new();

            let row_mapper = |row: &rusqlite::Row| -> rusqlite::Result<MemoryEntry> {
                Ok(MemoryEntry {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    content: row.get(2)?,
                    category: Self::str_to_category(&row.get::<_, String>(3)?),
                    timestamp: row.get(4)?,
                    session_id: row.get(5)?,
                    score: None,
                })
            };

            if let Some(ref cat) = category {
                let cat_str = Self::category_to_str(cat);
                let mut stmt = conn.prepare(
                    "SELECT id, key, content, category, created_at, session_id FROM memories
                     WHERE category = ?1 ORDER BY updated_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![cat_str, DEFAULT_LIST_LIMIT], row_mapper)?;
                for row in rows {
                    let entry = row?;
                    if let Some(sid) = session_ref {
                        if entry.session_id.as_deref() != Some(sid) {
                            continue;
                        }
                    }
                    results.push(entry);
                }
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, key, content, category, created_at, session_id FROM memories
                     ORDER BY updated_at DESC LIMIT ?1",
                )?;
                let rows = stmt.query_map(params![DEFAULT_LIST_LIMIT], row_mapper)?;
                for row in rows {
                    let entry = row?;
                    if let Some(sid) = session_ref {
                        if entry.session_id.as_deref() != Some(sid) {
                            continue;
                        }
                    }
                    results.push(entry);
                }
            }

            Ok(results)
        })
        .await?
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let conn = self.conn.clone();
        let key = key.to_string();

        tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let conn = conn.lock();
            let affected = conn.execute("DELETE FROM memories WHERE key = ?1", params![key])?;
            Ok(affected > 0)
        })
        .await?
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let conn = conn.lock();
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            Ok(count as usize)
        })
        .await?
    }

    async fn health_check(&self) -> bool {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || conn.lock().execute_batch("SELECT 1").is_ok())
            .await
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_sqlite() -> (TempDir, SqliteMemory) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        (tmp, mem)
    }

    #[tokio::test]
    async fn sqlite_name() {
        let (_tmp, mem) = temp_sqlite();
        assert_eq!(mem.name(), "sqlite");
    }

    #[tokio::test]
    async fn sqlite_health() {
        let (_tmp, mem) = temp_sqlite();
        assert!(mem.health_check().await);
    }

    #[tokio::test]
    async fn sqlite_store_and_get() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("user_lang", "Prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();

        let entry = mem.get("user_lang").await.unwrap();
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.key, "user_lang");
        assert_eq!(entry.content, "Prefers Rust");
        assert_eq!(entry.category, MemoryCategory::Core);
    }

    #[tokio::test]
    async fn sqlite_store_upsert() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("pref", "likes Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("pref", "loves Rust", MemoryCategory::Core, None)
            .await
            .unwrap();

        let entry = mem.get("pref").await.unwrap().unwrap();
        assert_eq!(entry.content, "loves Rust");
        assert_eq!(mem.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn sqlite_recall_keyword() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "Rust is fast and safe", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "Python is interpreted", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store(
            "c",
            "Rust has zero-cost abstractions",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let results = mem.recall("Rust", 10, None).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|r| r.content.to_lowercase().contains("rust")));
    }

    #[tokio::test]
    async fn sqlite_recall_multi_keyword() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "Rust is fast", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "Rust is safe and fast", MemoryCategory::Core, None)
            .await
            .unwrap();

        let results = mem.recall("fast safe", 10, None).await.unwrap();
        assert!(!results.is_empty());
        // Entry with both keywords should score higher
        assert!(results[0].content.contains("safe") && results[0].content.contains("fast"));
    }

    #[tokio::test]
    async fn sqlite_recall_no_match() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "Rust rocks", MemoryCategory::Core, None)
            .await
            .unwrap();
        let results = mem.recall("javascript", 10, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn sqlite_forget() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("temp", "temporary data", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        assert_eq!(mem.count().await.unwrap(), 1);

        let removed = mem.forget("temp").await.unwrap();
        assert!(removed);
        assert_eq!(mem.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sqlite_forget_nonexistent() {
        let (_tmp, mem) = temp_sqlite();
        let removed = mem.forget("nope").await.unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn sqlite_list_all() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "one", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "two", MemoryCategory::Daily, None)
            .await
            .unwrap();
        mem.store("c", "three", MemoryCategory::Conversation, None)
            .await
            .unwrap();

        let all = mem.list(None, None).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn sqlite_list_by_category() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "core1", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "core2", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("c", "daily1", MemoryCategory::Daily, None)
            .await
            .unwrap();

        let core = mem.list(Some(&MemoryCategory::Core), None).await.unwrap();
        assert_eq!(core.len(), 2);

        let daily = mem.list(Some(&MemoryCategory::Daily), None).await.unwrap();
        assert_eq!(daily.len(), 1);
    }

    #[tokio::test]
    async fn sqlite_count_empty() {
        let (_tmp, mem) = temp_sqlite();
        assert_eq!(mem.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sqlite_get_nonexistent() {
        let (_tmp, mem) = temp_sqlite();
        assert!(mem.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sqlite_db_persists() {
        let tmp = TempDir::new().unwrap();

        {
            let mem = SqliteMemory::new(tmp.path()).unwrap();
            mem.store("persist", "I survive restarts", MemoryCategory::Core, None)
                .await
                .unwrap();
        }

        // Reopen
        let mem2 = SqliteMemory::new(tmp.path()).unwrap();
        let entry = mem2.get("persist").await.unwrap();
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().content, "I survive restarts");
    }

    #[tokio::test]
    async fn sqlite_category_roundtrip() {
        let (_tmp, mem) = temp_sqlite();
        let categories = [
            MemoryCategory::Core,
            MemoryCategory::Daily,
            MemoryCategory::Conversation,
            MemoryCategory::Custom("project".into()),
        ];

        for (i, cat) in categories.iter().enumerate() {
            mem.store(&format!("k{i}"), &format!("v{i}"), cat.clone(), None)
                .await
                .unwrap();
        }

        for (i, cat) in categories.iter().enumerate() {
            let entry = mem.get(&format!("k{i}")).await.unwrap().unwrap();
            assert_eq!(&entry.category, cat);
        }
    }

    // ── FTS5 search tests ────────────────────────────────────────

    #[tokio::test]
    async fn fts5_bm25_ranking() {
        let (_tmp, mem) = temp_sqlite();
        mem.store(
            "a",
            "Rust is a systems programming language",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        mem.store(
            "b",
            "Python is great for scripting",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        mem.store(
            "c",
            "Rust and Rust and Rust everywhere",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let results = mem.recall("Rust", 10, None).await.unwrap();
        assert!(results.len() >= 2);
        // All results should contain "Rust"
        for r in &results {
            assert!(
                r.content.to_lowercase().contains("rust"),
                "Expected 'rust' in: {}",
                r.content
            );
        }
    }

    #[tokio::test]
    async fn fts5_multi_word_query() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "The quick brown fox jumps", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "A lazy dog sleeps", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("c", "The quick dog runs fast", MemoryCategory::Core, None)
            .await
            .unwrap();

        let results = mem.recall("quick dog", 10, None).await.unwrap();
        assert!(!results.is_empty());
        // "The quick dog runs fast" matches both terms
        assert!(results[0].content.contains("quick"));
    }

    #[tokio::test]
    async fn recall_empty_query_returns_empty() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "data", MemoryCategory::Core, None)
            .await
            .unwrap();
        let results = mem.recall("", 10, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn recall_whitespace_query_returns_empty() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "data", MemoryCategory::Core, None)
            .await
            .unwrap();
        let results = mem.recall("   ", 10, None).await.unwrap();
        assert!(results.is_empty());
    }

    // ── Embedding cache tests ────────────────────────────────────

    #[test]
    fn content_hash_deterministic() {
        let h1 = SqliteMemory::content_hash("test-model", 8, "hello world");
        let h2 = SqliteMemory::content_hash("test-model", 8, "hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_different_inputs() {
        let h1 = SqliteMemory::content_hash("test-model", 8, "hello");
        let h2 = SqliteMemory::content_hash("test-model", 8, "world");
        assert_ne!(h1, h2);
    }

    // ── Schema tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn schema_has_fts5_table() {
        let (_tmp, mem) = temp_sqlite();
        let conn = mem.conn.lock();
        // FTS5 table should exist
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memories_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn schema_has_embedding_cache() {
        let (_tmp, mem) = temp_sqlite();
        let conn = mem.conn.lock();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='embedding_cache'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn schema_memories_has_embedding_column() {
        let (_tmp, mem) = temp_sqlite();
        let conn = mem.conn.lock();
        // Check that embedding column exists by querying it
        let result = conn.execute_batch("SELECT embedding FROM memories LIMIT 0");
        assert!(result.is_ok());
    }

    // ── FTS5 sync trigger tests ──────────────────────────────────

    #[tokio::test]
    async fn fts5_syncs_on_insert() {
        let (_tmp, mem) = temp_sqlite();
        mem.store(
            "test_key",
            "unique_searchterm_xyz",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let conn = mem.conn.lock();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"unique_searchterm_xyz\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn fts5_syncs_on_delete() {
        let (_tmp, mem) = temp_sqlite();
        mem.store(
            "del_key",
            "deletable_content_abc",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        mem.forget("del_key").await.unwrap();

        let conn = mem.conn.lock();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"deletable_content_abc\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn fts5_syncs_on_update() {
        let (_tmp, mem) = temp_sqlite();
        mem.store(
            "upd_key",
            "original_content_111",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        mem.store("upd_key", "updated_content_222", MemoryCategory::Core, None)
            .await
            .unwrap();

        let conn = mem.conn.lock();
        // Old content should not be findable
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"original_content_111\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old, 0);

        // New content should be findable
        let new: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"updated_content_222\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new, 1);
    }

    // ── Open timeout tests ────────────────────────────────────────

    #[test]
    fn open_with_timeout_succeeds_when_fast() {
        let tmp = TempDir::new().unwrap();
        let embedder = Arc::new(super::super::embeddings::NoopEmbedding);
        let mem = SqliteMemory::with_embedder(tmp.path(), embedder, 0.7, 0.3, 1000, Some(5));
        assert!(
            mem.is_ok(),
            "open with 5s timeout should succeed on fast path"
        );
        assert_eq!(mem.unwrap().name(), "sqlite");
    }

    #[tokio::test]
    async fn open_with_timeout_store_recall_unchanged() {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::with_embedder(
            tmp.path(),
            Arc::new(super::super::embeddings::NoopEmbedding),
            0.7,
            0.3,
            1000,
            Some(2),
        )
        .unwrap();
        mem.store(
            "timeout_key",
            "value with timeout",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        let entry = mem.get("timeout_key").await.unwrap().unwrap();
        assert_eq!(entry.content, "value with timeout");
    }

    // ── With-embedder constructor test ───────────────────────────

    #[test]
    fn with_embedder_noop() {
        let tmp = TempDir::new().unwrap();
        let embedder = Arc::new(super::super::embeddings::NoopEmbedding);
        let mem = SqliteMemory::with_embedder(tmp.path(), embedder, 0.7, 0.3, 1000, None);
        assert!(mem.is_ok());
        assert_eq!(mem.unwrap().name(), "sqlite");
    }

    // ── Reindex test ─────────────────────────────────────────────

    #[tokio::test]
    async fn reindex_rebuilds_fts() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("r1", "reindex test alpha", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("r2", "reindex test beta", MemoryCategory::Core, None)
            .await
            .unwrap();

        // Reindex should succeed (noop embedder → 0 re-embedded)
        let count = mem.reindex().await.unwrap();
        assert_eq!(count, 0);

        // FTS should still work after rebuild
        let results = mem.recall("reindex", 10, None).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    // ── Recall limit test ────────────────────────────────────────

    #[tokio::test]
    async fn recall_respects_limit() {
        let (_tmp, mem) = temp_sqlite();
        for i in 0..20 {
            mem.store(
                &format!("k{i}"),
                &format!("common keyword item {i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }

        let results = mem.recall("common keyword", 5, None).await.unwrap();
        assert!(results.len() <= 5);
    }

    // ── Score presence test ──────────────────────────────────────

    #[tokio::test]
    async fn recall_results_have_scores() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("s1", "scored result test", MemoryCategory::Core, None)
            .await
            .unwrap();

        let results = mem.recall("scored", 10, None).await.unwrap();
        assert!(!results.is_empty());
        for r in &results {
            assert!(r.score.is_some(), "Expected score on result: {:?}", r.key);
        }
    }

    // ── Edge cases: FTS5 special characters ──────────────────────

    #[tokio::test]
    async fn recall_with_quotes_in_query() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("q1", "He said hello world", MemoryCategory::Core, None)
            .await
            .unwrap();
        // Quotes in query should not crash FTS5
        let results = mem.recall("\"hello\"", 10, None).await.unwrap();
        // May or may not match depending on FTS5 escaping, but must not error
        assert!(results.len() <= 10);
    }

    #[tokio::test]
    async fn recall_with_asterisk_in_query() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a1", "wildcard test content", MemoryCategory::Core, None)
            .await
            .unwrap();
        let results = mem.recall("wild*", 10, None).await.unwrap();
        assert!(results.len() <= 10);
    }

    #[tokio::test]
    async fn recall_with_parentheses_in_query() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("p1", "function call test", MemoryCategory::Core, None)
            .await
            .unwrap();
        let results = mem.recall("function()", 10, None).await.unwrap();
        assert!(results.len() <= 10);
    }

    #[tokio::test]
    async fn recall_with_sql_injection_attempt() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("safe", "normal content", MemoryCategory::Core, None)
            .await
            .unwrap();
        // Should not crash or leak data
        let results = mem
            .recall("'; DROP TABLE memories; --", 10, None)
            .await
            .unwrap();
        assert!(results.len() <= 10);
        // Table should still exist
        assert_eq!(mem.count().await.unwrap(), 1);
    }

    // ── Edge cases: store ────────────────────────────────────────

    #[tokio::test]
    async fn store_empty_content() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("empty", "", MemoryCategory::Core, None)
            .await
            .unwrap();
        let entry = mem.get("empty").await.unwrap().unwrap();
        assert_eq!(entry.content, "");
    }

    #[tokio::test]
    async fn store_empty_key() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("", "content for empty key", MemoryCategory::Core, None)
            .await
            .unwrap();
        let entry = mem.get("").await.unwrap().unwrap();
        assert_eq!(entry.content, "content for empty key");
    }

    #[tokio::test]
    async fn store_very_long_content() {
        let (_tmp, mem) = temp_sqlite();
        let long_content = "x".repeat(100_000);
        mem.store("long", &long_content, MemoryCategory::Core, None)
            .await
            .unwrap();
        let entry = mem.get("long").await.unwrap().unwrap();
        assert_eq!(entry.content.len(), 100_000);
    }

    #[tokio::test]
    async fn store_unicode_and_emoji() {
        let (_tmp, mem) = temp_sqlite();
        mem.store(
            "emoji_key_🦀",
            "こんにちは 🚀 Ñoño",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        let entry = mem.get("emoji_key_🦀").await.unwrap().unwrap();
        assert_eq!(entry.content, "こんにちは 🚀 Ñoño");
    }

    #[tokio::test]
    async fn store_content_with_newlines_and_tabs() {
        let (_tmp, mem) = temp_sqlite();
        let content = "line1\nline2\ttab\rcarriage\n\nnewparagraph";
        mem.store("whitespace", content, MemoryCategory::Core, None)
            .await
            .unwrap();
        let entry = mem.get("whitespace").await.unwrap().unwrap();
        assert_eq!(entry.content, content);
    }

    // ── Edge cases: recall ───────────────────────────────────────

    #[tokio::test]
    async fn recall_single_character_query() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "x marks the spot", MemoryCategory::Core, None)
            .await
            .unwrap();
        // Single char may not match FTS5 but LIKE fallback should work
        let results = mem.recall("x", 10, None).await.unwrap();
        // Should not crash; may or may not find results
        assert!(results.len() <= 10);
    }

    #[tokio::test]
    async fn recall_limit_zero() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "some content", MemoryCategory::Core, None)
            .await
            .unwrap();
        let results = mem.recall("some", 0, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn recall_limit_one() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "matching content alpha", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "matching content beta", MemoryCategory::Core, None)
            .await
            .unwrap();
        let results = mem.recall("matching content", 1, None).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn recall_matches_by_key_not_just_content() {
        let (_tmp, mem) = temp_sqlite();
        mem.store(
            "rust_preferences",
            "User likes systems programming",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        // "rust" appears in key but not content — LIKE fallback checks key too
        let results = mem.recall("rust", 10, None).await.unwrap();
        assert!(!results.is_empty(), "Should match by key");
    }

    #[tokio::test]
    async fn recall_unicode_query() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("jp", "日本語のテスト", MemoryCategory::Core, None)
            .await
            .unwrap();
        let results = mem.recall("日本語", 10, None).await.unwrap();
        assert!(!results.is_empty());
    }

    // ── Edge cases: schema idempotency ───────────────────────────

    #[tokio::test]
    async fn schema_idempotent_reopen() {
        let tmp = TempDir::new().unwrap();
        {
            let mem = SqliteMemory::new(tmp.path()).unwrap();
            mem.store("k1", "v1", MemoryCategory::Core, None)
                .await
                .unwrap();
        }
        // Open again — init_schema runs again on existing DB
        let mem2 = SqliteMemory::new(tmp.path()).unwrap();
        let entry = mem2.get("k1").await.unwrap();
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().content, "v1");
        // Store more data — should work fine
        mem2.store("k2", "v2", MemoryCategory::Daily, None)
            .await
            .unwrap();
        assert_eq!(mem2.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn schema_triple_open() {
        let tmp = TempDir::new().unwrap();
        let _m1 = SqliteMemory::new(tmp.path()).unwrap();
        let _m2 = SqliteMemory::new(tmp.path()).unwrap();
        let m3 = SqliteMemory::new(tmp.path()).unwrap();
        assert!(m3.health_check().await);
    }

    // ── Edge cases: forget + FTS5 consistency ────────────────────

    #[tokio::test]
    async fn forget_then_recall_no_ghost_results() {
        let (_tmp, mem) = temp_sqlite();
        mem.store(
            "ghost",
            "phantom memory content",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();
        mem.forget("ghost").await.unwrap();
        let results = mem.recall("phantom memory", 10, None).await.unwrap();
        assert!(
            results.is_empty(),
            "Deleted memory should not appear in recall"
        );
    }

    #[tokio::test]
    async fn forget_and_re_store_same_key() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("cycle", "version 1", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.forget("cycle").await.unwrap();
        mem.store("cycle", "version 2", MemoryCategory::Core, None)
            .await
            .unwrap();
        let entry = mem.get("cycle").await.unwrap().unwrap();
        assert_eq!(entry.content, "version 2");
        assert_eq!(mem.count().await.unwrap(), 1);
    }

    // ── Edge cases: reindex ──────────────────────────────────────

    #[tokio::test]
    async fn reindex_empty_db() {
        let (_tmp, mem) = temp_sqlite();
        let count = mem.reindex().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn reindex_twice_is_safe() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("r1", "reindex data", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.reindex().await.unwrap();
        let count = mem.reindex().await.unwrap();
        assert_eq!(count, 0); // Noop embedder → nothing to re-embed
                              // Data should still be intact
        let results = mem.recall("reindex", 10, None).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    // ── Edge cases: content_hash ─────────────────────────────────

    #[test]
    fn content_hash_empty_string() {
        let h = SqliteMemory::content_hash("test-model", 8, "");
        assert!(!h.is_empty());
        assert_eq!(h.len(), 16); // 16 hex chars
    }

    #[test]
    fn content_hash_unicode() {
        let h1 = SqliteMemory::content_hash("test-model", 8, "🦀");
        let h2 = SqliteMemory::content_hash("test-model", 8, "🦀");
        assert_eq!(h1, h2);
        let h3 = SqliteMemory::content_hash("test-model", 8, "🚀");
        assert_ne!(h1, h3);
    }

    #[test]
    fn content_hash_long_input() {
        let long = "a".repeat(1_000_000);
        let h = SqliteMemory::content_hash("test-model", 8, &long);
        assert_eq!(h.len(), 16);
    }

    // ── reindex ───────────────────────────────────────────────────

    /// A row embedded by another model is skipped by vector search, because a
    /// vector of a different dimensionality is not comparable. Nothing
    /// re-embeds on its own, so switching models used to empty vector recall
    /// permanently.
    #[tokio::test]
    async fn reindex_re_embeds_rows_from_a_foreign_model() {
        let tmp = TempDir::new().unwrap();

        let small = stub_memory(tmp.path(), "stub-4", 4);
        small
            .store(
                "legacy",
                "embedded by the old model",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        drop(small);

        let large = stub_memory(tmp.path(), "stub-8", 8);
        let re_embedded = large.reindex().await.unwrap();
        assert_eq!(
            re_embedded, 1,
            "the foreign-dimensioned row must be rebuilt"
        );

        let conn = large.conn.lock();
        let dims: Option<i64> = conn
            .query_row(
                "SELECT embedding_dims FROM memories WHERE key = 'legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            dims,
            Some(8),
            "provenance must be stamped with the new model"
        );
    }

    /// A write that happened while the provider was down leaves no vector and
    /// NULL provenance — exactly what this is for.
    #[tokio::test]
    async fn reindex_fills_in_rows_that_never_got_an_embedding() {
        let tmp = TempDir::new().unwrap();

        let degraded = failing_embedder_memory(tmp.path());
        degraded
            .store(
                "stored_while_down",
                "a durable fact",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        drop(degraded);

        let healthy = stub_memory(tmp.path(), "stub-8", 8);
        assert_eq!(healthy.reindex().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn reindex_is_quiet_when_everything_already_matches() {
        let tmp = TempDir::new().unwrap();
        let mem = stub_memory(tmp.path(), "stub-8", 8);
        mem.store("k", "already embedded", MemoryCategory::Core, None)
            .await
            .unwrap();

        assert_eq!(
            mem.reindex().await.unwrap(),
            0,
            "a matching row must not be re-embedded on every run"
        );
    }

    // ── Recall path hardening ─────────────────────────────────────

    /// An embedder that always fails, standing in for a rate-limited or
    /// unreachable provider.
    struct FailingEmbedder;

    #[async_trait]
    impl super::super::embeddings::EmbeddingProvider for FailingEmbedder {
        fn name(&self) -> &str {
            "failing-stub"
        }
        fn dimensions(&self) -> usize {
            8
        }
        async fn embed(&self, _texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            anyhow::bail!("embedding provider unavailable (429)")
        }
    }

    fn failing_embedder_memory(dir: &Path) -> SqliteMemory {
        SqliteMemory::with_embedder(dir, Arc::new(FailingEmbedder), 0.7, 0.3, 1000, None).unwrap()
    }

    /// The keyword search took the global top-N and only then dropped rows from
    /// other sessions, so a conversation's own memories were findable only when
    /// they outranked every other conversation's.
    #[tokio::test]
    async fn scoped_recall_finds_rows_outside_the_global_top_n() {
        let (_tmp, mem) = temp_sqlite();

        for i in 0..40 {
            mem.store(
                &format!("noise_{i}"),
                "shared topic shared topic shared topic",
                MemoryCategory::Conversation,
                Some("session-a"),
            )
            .await
            .unwrap();
        }
        mem.store(
            "needle",
            "shared topic",
            MemoryCategory::Conversation,
            Some("session-b"),
        )
        .await
        .unwrap();

        let hits = mem
            .recall("shared topic", 5, Some("session-b"))
            .await
            .unwrap();

        assert_eq!(
            hits.len(),
            1,
            "expected the session-b row, got {:?}",
            hits.iter().map(|e| &e.key).collect::<Vec<_>>()
        );
        assert_eq!(hits[0].key, "needle");
    }

    /// Isolates the keyword path. The integration test above can be satisfied by
    /// the substring fallback, which this PR also scopes — so on its own it
    /// proves the pair works, not that FTS filters. This one asks `fts5_search`
    /// directly.
    #[tokio::test]
    async fn fts5_search_applies_the_session_filter() {
        let (_tmp, mem) = temp_sqlite();
        for i in 0..40 {
            mem.store(
                &format!("noise_{i}"),
                "shared topic shared topic shared topic",
                MemoryCategory::Conversation,
                Some("session-a"),
            )
            .await
            .unwrap();
        }
        mem.store(
            "needle",
            "shared topic",
            MemoryCategory::Conversation,
            Some("session-b"),
        )
        .await
        .unwrap();

        let conn = mem.conn.lock();
        let hits = SqliteMemory::fts5_search(&conn, "shared topic", 10, Some("session-b")).unwrap();

        assert_eq!(
            hits.len(),
            1,
            "keyword search must filter by session in SQL, not after the limit"
        );
    }

    #[tokio::test]
    async fn scoped_recall_still_excludes_other_sessions() {
        let (_tmp, mem) = temp_sqlite();
        mem.store(
            "a",
            "shared topic",
            MemoryCategory::Conversation,
            Some("session-a"),
        )
        .await
        .unwrap();
        mem.store(
            "b",
            "shared topic",
            MemoryCategory::Conversation,
            Some("session-b"),
        )
        .await
        .unwrap();

        let hits = mem.recall("shared", 10, Some("session-a")).await.unwrap();
        assert_eq!(hits.len(), 1, "scope filter must not become a no-op");
        assert_eq!(hits[0].key, "a");
    }

    /// A `"` inside a term closed the FTS5 string literal early, so the whole
    /// expression failed to parse and the query silently dropped to the
    /// substring fallback.
    #[tokio::test]
    async fn quote_in_query_still_uses_fts() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("quoted", "deployment runbook", MemoryCategory::Core, None)
            .await
            .unwrap();

        // `runbook"` is a whole token to FTS5 once the quote is escaped, but is
        // not a substring of the stored content — so a hit here cannot have come
        // from the LIKE fallback.
        let hits = mem.recall("runbook\"", 10, None).await.unwrap();

        assert_eq!(
            hits.len(),
            1,
            "expected the FTS hit, got {:?}",
            hits.iter().map(|e| &e.key).collect::<Vec<_>>()
        );
        assert_eq!(hits[0].key, "quoted");
    }

    #[test]
    fn build_fts_query_escapes_embedded_quotes() {
        assert_eq!(SqliteMemory::build_fts_query("a\"b"), "\"a\"\"b\"");
        assert_eq!(
            SqliteMemory::build_fts_query("one two"),
            "\"one\" OR \"two\""
        );
        assert_eq!(SqliteMemory::build_fts_query("   "), "");
    }

    /// An embedding outage used to fail the write outright, and auto-save
    /// discards the error — so the message was lost without a trace.
    #[tokio::test]
    async fn store_survives_embedding_provider_failure() {
        let tmp = TempDir::new().unwrap();
        let mem = failing_embedder_memory(tmp.path());

        mem.store(
            "survivor",
            "user prefers concise answers",
            MemoryCategory::Core,
            None,
        )
        .await
        .expect("store must not fail when the embedding provider does");

        let stored = mem.get("survivor").await.unwrap().expect("row must exist");
        assert_eq!(stored.content, "user prefers concise answers");

        // Provenance stays NULL, which is what a later re-embedding pass looks for.
        let conn = mem.conn.lock();
        let dims: Option<i64> = conn
            .query_row(
                "SELECT embedding_dims FROM memories WHERE key = 'survivor'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dims, None, "a degraded write must not claim provenance");
    }

    #[tokio::test]
    async fn recall_survives_embedding_provider_failure() {
        let tmp = TempDir::new().unwrap();
        let mem = failing_embedder_memory(tmp.path());

        mem.store("k", "rust ownership model", MemoryCategory::Core, None)
            .await
            .unwrap();

        let hits = mem
            .recall("ownership", 10, None)
            .await
            .expect("recall must not fail when the embedding provider does");
        assert_eq!(hits.len(), 1, "keyword results must still come back");
        assert_eq!(hits[0].key, "k");
    }

    // ── Score contract ────────────────────────────────────────────

    /// Keyword hits score by query coverage — absolute, corpus-independent.
    /// A row covering the whole query scores 1.0 on its own merits (BM25's
    /// magnitude is near zero on a small store and cannot be the score); a
    /// row covering half the query scores 0.5 even when it is the best hit.
    #[tokio::test]
    async fn keyword_only_hits_score_by_absolute_query_coverage() {
        let (_tmp, mem) = temp_sqlite();
        for (k, c) in [
            ("a", "rust ownership and borrowing"),
            ("b", "rust lifetimes"),
            ("c", "unrelated note about gardening"),
        ] {
            mem.store(k, c, MemoryCategory::Core, None).await.unwrap();
        }

        let hits = mem.recall("rust", 10, None).await.unwrap();
        assert!(!hits.is_empty(), "expected keyword hits");
        let best = hits.iter().filter_map(|e| e.score).fold(0.0_f64, f64::max);
        assert!(
            (best - 1.0).abs() < 1e-6,
            "full query coverage scores 1.0, got {best}"
        );
        for e in &hits {
            let s = e.score.unwrap();
            assert!((0.0..=1.0).contains(&s), "{} out of range: {s}", e.key);
        }

        // Partial coverage stays partial — the best hit is NOT rescaled up.
        let partial = mem.recall("rust gardening", 10, None).await.unwrap();
        let best = partial
            .iter()
            .filter_map(|e| e.score)
            .fold(0.0_f64, f64::max);
        assert!(
            (best - 0.5).abs() < 1e-6,
            "half coverage must score 0.5 even as the best hit, got {best}"
        );
    }

    /// The LIKE fallback is an unranked substring scan. It used to claim a flat
    /// 1.0, which made the weakest retrieval path outrank every BM25 hit. Score
    /// it by how much of the query each row actually covers.
    #[tokio::test]
    async fn like_fallback_ranks_by_query_coverage() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("both", "telemetry subsystem", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("one", "telemetry only", MemoryCategory::Core, None)
            .await
            .unwrap();

        // Partial words: FTS5 matches whole tokens, so this reaches the fallback.
        let hits = mem.recall("eleme ubsyst", 10, None).await.unwrap();
        assert_eq!(hits.len(), 2, "both rows contain at least one fragment");

        let score_of = |key: &str| {
            hits.iter()
                .find(|e| e.key == key)
                .and_then(|e| e.score)
                .unwrap_or_else(|| panic!("missing {key}"))
        };
        assert!(
            (score_of("both") - 1.0).abs() < 1e-6,
            "row covering both fragments should be the best hit, got {}",
            score_of("both")
        );
        assert!(
            score_of("one") < score_of("both"),
            "partial coverage must rank below full coverage: {} vs {}",
            score_of("one"),
            score_of("both")
        );
    }

    // ── Embedding provenance + UTC timestamp migration ────────────

    /// Deterministic embedder with a declared identity, so tests can simulate
    /// swapping models without any network.
    struct StubEmbedder {
        label: &'static str,
        dims: usize,
    }

    #[async_trait]
    impl super::super::embeddings::EmbeddingProvider for StubEmbedder {
        fn name(&self) -> &str {
            self.label
        }
        fn dimensions(&self) -> usize {
            self.dims
        }
        async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    #[allow(clippy::cast_precision_loss)]
                    (0..self.dims)
                        .map(|i| ((t.len() + i) % 7) as f32 + 1.0)
                        .collect()
                })
                .collect())
        }
    }

    fn stub_memory(dir: &Path, label: &'static str, dims: usize) -> SqliteMemory {
        SqliteMemory::with_embedder(
            dir,
            Arc::new(StubEmbedder { label, dims }),
            0.7,
            0.3,
            1000,
            None,
        )
        .unwrap()
    }

    /// Write a database in the shape a pre-migration build left behind: no
    /// provenance columns, `user_version` still 0, timestamps carrying an offset.
    fn legacy_db(dir: &Path, created: &str, updated: &str) {
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        let conn = Connection::open(dir.join("memory").join("brain.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY, key TEXT NOT NULL UNIQUE, content TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'core', embedding BLOB,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, session_id TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at)
             VALUES ('legacy-1', 'legacy_key', 'legacy content', 'conversation', ?1, ?2)",
            params![created, updated],
        )
        .unwrap();
    }

    fn stored_timestamps(dir: &Path) -> (String, String) {
        let conn = Connection::open(dir.join("memory").join("brain.db")).unwrap();
        conn.query_row(
            "SELECT created_at, updated_at FROM memories WHERE id = 'legacy-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn legacy_local_offset_timestamps_migrate_to_utc() {
        let tmp = TempDir::new().unwrap();
        legacy_db(
            tmp.path(),
            "2026-08-05T10:00:00+07:00",
            "2026-08-05T11:30:00+07:00",
        );

        let _mem = SqliteMemory::new(tmp.path()).unwrap();

        let (created, updated) = stored_timestamps(tmp.path());
        assert_eq!(
            created, "2026-08-05T03:00:00+00:00",
            "created_at not in UTC"
        );
        assert_eq!(
            updated, "2026-08-05T04:30:00+00:00",
            "updated_at not in UTC"
        );
    }

    #[test]
    fn timestamp_migration_runs_once() {
        let tmp = TempDir::new().unwrap();
        legacy_db(
            tmp.path(),
            "2026-08-05T10:00:00+07:00",
            "2026-08-05T10:00:00+07:00",
        );
        let _mem = SqliteMemory::new(tmp.path()).unwrap();

        // A later write in some other offset must survive a reopen untouched —
        // the migration is a one-time normalisation, not a recurring rewrite.
        {
            let conn = Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
            conn.execute(
                "UPDATE memories SET updated_at = '2026-08-06T09:00:00+02:00' WHERE id = 'legacy-1'",
                [],
            )
            .unwrap();
        }
        let _mem2 = SqliteMemory::new(tmp.path()).unwrap();

        let (_, updated) = stored_timestamps(tmp.path());
        assert_eq!(
            updated, "2026-08-06T09:00:00+02:00",
            "migration re-ran on an already-migrated database"
        );
    }

    #[test]
    fn unparseable_timestamp_is_left_alone() {
        let tmp = TempDir::new().unwrap();
        legacy_db(tmp.path(), "not-a-date", "also-not-a-date");

        let _mem = SqliteMemory::new(tmp.path()).unwrap();

        let (created, updated) = stored_timestamps(tmp.path());
        assert_eq!(created, "not-a-date");
        assert_eq!(updated, "also-not-a-date");
    }

    #[test]
    fn embedding_cache_key_separates_models() {
        let same = SqliteMemory::content_hash("model-a", 8, "shared text");
        let other_model = SqliteMemory::content_hash("model-b", 8, "shared text");
        let other_dims = SqliteMemory::content_hash("model-a", 16, "shared text");

        assert_ne!(
            same, other_model,
            "a different model must not hit the same cache entry"
        );
        assert_ne!(
            same, other_dims,
            "a different dimensionality must not hit the same cache entry"
        );
    }

    /// A vector produced by another embedder is not comparable: `cosine_similarity`
    /// returns 0.0 on a length mismatch without signalling why. Those rows must be
    /// skipped rather than silently scored as "no match".
    #[tokio::test]
    async fn vector_search_skips_foreign_dimensioned_rows() {
        let tmp = TempDir::new().unwrap();

        let small = stub_memory(tmp.path(), "stub-4", 4);
        small
            .store(
                "k4",
                "embedded by the 4-dim model",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        drop(small);

        let large = stub_memory(tmp.path(), "stub-8", 8);
        large
            .store(
                "k8",
                "embedded by the 8-dim model",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();

        let query = large
            .get_or_compute_embedding("embedded by the 8-dim model")
            .await
            .unwrap()
            .expect("stub embedder should produce a vector");

        let conn = large.conn.lock();
        let hits = SqliteMemory::vector_search(&conn, &query, 10, None, None).unwrap();

        let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            hits.len(),
            1,
            "only the row matching the live embedder is comparable, got {ids:?}"
        );
    }

    // ── Edge cases: category helpers ─────────────────────────────

    #[test]
    fn category_roundtrip_custom_with_spaces() {
        let cat = MemoryCategory::Custom("my custom category".into());
        let s = SqliteMemory::category_to_str(&cat);
        assert_eq!(s, "my custom category");
        let back = SqliteMemory::str_to_category(&s);
        assert_eq!(back, cat);
    }

    #[test]
    fn category_roundtrip_empty_custom() {
        let cat = MemoryCategory::Custom(String::new());
        let s = SqliteMemory::category_to_str(&cat);
        assert_eq!(s, "");
        let back = SqliteMemory::str_to_category(&s);
        assert_eq!(back, MemoryCategory::Custom(String::new()));
    }

    // ── Edge cases: list ─────────────────────────────────────────

    #[tokio::test]
    async fn list_custom_category() {
        let (_tmp, mem) = temp_sqlite();
        mem.store(
            "c1",
            "custom1",
            MemoryCategory::Custom("project".into()),
            None,
        )
        .await
        .unwrap();
        mem.store(
            "c2",
            "custom2",
            MemoryCategory::Custom("project".into()),
            None,
        )
        .await
        .unwrap();
        mem.store("c3", "other", MemoryCategory::Core, None)
            .await
            .unwrap();

        let project = mem
            .list(Some(&MemoryCategory::Custom("project".into())), None)
            .await
            .unwrap();
        assert_eq!(project.len(), 2);
    }

    #[tokio::test]
    async fn list_empty_db() {
        let (_tmp, mem) = temp_sqlite();
        let all = mem.list(None, None).await.unwrap();
        assert!(all.is_empty());
    }

    // ── Session isolation ─────────────────────────────────────────

    #[tokio::test]
    async fn store_and_recall_with_session_id() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("k1", "session A fact", MemoryCategory::Core, Some("sess-a"))
            .await
            .unwrap();
        mem.store("k2", "session B fact", MemoryCategory::Core, Some("sess-b"))
            .await
            .unwrap();
        mem.store("k3", "no session fact", MemoryCategory::Core, None)
            .await
            .unwrap();

        // Recall with session-a filter returns only session-a entry
        let results = mem.recall("fact", 10, Some("sess-a")).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "k1");
        assert_eq!(results[0].session_id.as_deref(), Some("sess-a"));
    }

    #[tokio::test]
    async fn recall_no_session_filter_returns_all() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("k1", "alpha fact", MemoryCategory::Core, Some("sess-a"))
            .await
            .unwrap();
        mem.store("k2", "beta fact", MemoryCategory::Core, Some("sess-b"))
            .await
            .unwrap();
        mem.store("k3", "gamma fact", MemoryCategory::Core, None)
            .await
            .unwrap();

        // Recall without session filter returns all matching entries
        let results = mem.recall("fact", 10, None).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn cross_session_recall_isolation() {
        let (_tmp, mem) = temp_sqlite();
        mem.store(
            "secret",
            "session A secret data",
            MemoryCategory::Core,
            Some("sess-a"),
        )
        .await
        .unwrap();

        // Session B cannot see session A data
        let results = mem.recall("secret", 10, Some("sess-b")).await.unwrap();
        assert!(results.is_empty());

        // Session A can see its own data
        let results = mem.recall("secret", 10, Some("sess-a")).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn list_with_session_filter() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("k1", "a1", MemoryCategory::Core, Some("sess-a"))
            .await
            .unwrap();
        mem.store("k2", "a2", MemoryCategory::Conversation, Some("sess-a"))
            .await
            .unwrap();
        mem.store("k3", "b1", MemoryCategory::Core, Some("sess-b"))
            .await
            .unwrap();
        mem.store("k4", "none1", MemoryCategory::Core, None)
            .await
            .unwrap();

        // List with session-a filter
        let results = mem.list(None, Some("sess-a")).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|e| e.session_id.as_deref() == Some("sess-a")));

        // List with session-a + category filter
        let results = mem
            .list(Some(&MemoryCategory::Core), Some("sess-a"))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "k1");
    }

    #[tokio::test]
    async fn schema_migration_idempotent_on_reopen() {
        let tmp = TempDir::new().unwrap();

        // First open: creates schema + migration
        {
            let mem = SqliteMemory::new(tmp.path()).unwrap();
            mem.store("k1", "before reopen", MemoryCategory::Core, Some("sess-x"))
                .await
                .unwrap();
        }

        // Second open: migration runs again but is idempotent
        {
            let mem = SqliteMemory::new(tmp.path()).unwrap();
            let results = mem.recall("reopen", 10, Some("sess-x")).await.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].key, "k1");
            assert_eq!(results[0].session_id.as_deref(), Some("sess-x"));
        }
    }

    // ── §4.1 Concurrent write contention tests ──────────────

    #[tokio::test]
    async fn sqlite_concurrent_writes_no_data_loss() {
        let (_tmp, mem) = temp_sqlite();
        let mem = std::sync::Arc::new(mem);

        let mut handles = Vec::new();
        for i in 0..10 {
            let mem = std::sync::Arc::clone(&mem);
            handles.push(tokio::spawn(async move {
                mem.store(
                    &format!("concurrent_key_{i}"),
                    &format!("value_{i}"),
                    MemoryCategory::Core,
                    None,
                )
                .await
                .unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let count = mem.count().await.unwrap();
        assert_eq!(
            count, 10,
            "all 10 concurrent writes must succeed without data loss"
        );
    }

    #[tokio::test]
    async fn sqlite_concurrent_read_write_no_panic() {
        let (_tmp, mem) = temp_sqlite();
        let mem = std::sync::Arc::new(mem);

        // Pre-populate
        mem.store("shared_key", "initial", MemoryCategory::Core, None)
            .await
            .unwrap();

        let mut handles = Vec::new();

        // Concurrent reads
        for _ in 0..5 {
            let mem = std::sync::Arc::clone(&mem);
            handles.push(tokio::spawn(async move {
                let _ = mem.get("shared_key").await.unwrap();
            }));
        }

        // Concurrent writes
        for i in 0..5 {
            let mem = std::sync::Arc::clone(&mem);
            handles.push(tokio::spawn(async move {
                mem.store(
                    &format!("key_{i}"),
                    &format!("val_{i}"),
                    MemoryCategory::Core,
                    None,
                )
                .await
                .unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        // Should have 6 total entries (1 pre-existing + 5 new)
        assert_eq!(mem.count().await.unwrap(), 6);
    }

    // ── §4.2 Reindex / corruption recovery tests ────────────

    #[tokio::test]
    async fn sqlite_reindex_preserves_data() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "Rust is fast", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "Python is interpreted", MemoryCategory::Core, None)
            .await
            .unwrap();

        mem.reindex().await.unwrap();

        let count = mem.count().await.unwrap();
        assert_eq!(count, 2, "reindex must preserve all entries");

        let entry = mem.get("a").await.unwrap();
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().content, "Rust is fast");
    }

    #[tokio::test]
    async fn sqlite_reindex_idempotent() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("x", "test data", MemoryCategory::Core, None)
            .await
            .unwrap();

        // Multiple reindex calls should be safe
        mem.reindex().await.unwrap();
        mem.reindex().await.unwrap();
        mem.reindex().await.unwrap();

        assert_eq!(mem.count().await.unwrap(), 1);
    }
}
