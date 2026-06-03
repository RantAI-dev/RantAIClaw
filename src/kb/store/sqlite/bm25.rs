//! BM25 lexical search via SQLite's FTS5 virtual table.
//!
//! SQLite's `bm25()` returns a *lower-is-better* score (smaller distance from
//! the query). The TS side (`search::score(0)` convention) treats higher as
//! better and downstream RRF/rerank code assumes the same. Negate before
//! returning so the contract matches across backends.

use rusqlite::params;

use super::SqliteStore;
use crate::kb::store::Bm25Hit;
use crate::kb::{ChunkId, DocumentId, KbError, KbResult};

impl SqliteStore {
    pub(crate) async fn bm25_search_impl(
        &self,
        query: &str,
        limit: usize,
    ) -> KbResult<Vec<Bm25Hit>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let query = query.to_string();
        let limit_i = limit as i64;

        tokio::task::spawn_blocking(move || -> KbResult<Vec<Bm25Hit>> {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT c.id, c.document_id, c.content, bm25(chunk_fts) AS score
                 FROM chunk_fts
                 JOIN chunk c ON c.rowid = chunk_fts.rowid
                 JOIN document d ON d.id = c.document_id
                 WHERE chunk_fts MATCH ?1 AND d.deleted_at IS NULL
                 ORDER BY score
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![query, limit_i])?;

            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let raw_score: f32 = row.get("score")?;
                out.push(Bm25Hit {
                    id: ChunkId(row.get("id")?),
                    document_id: DocumentId(row.get("document_id")?),
                    content: row.get("content")?,
                    // Negate: SQLite's `bm25()` is lower-is-better, but the
                    // TS retrieval pipeline treats higher as better. A pure
                    // negation preserves rank order and matches downstream
                    // RRF/rerank assumptions.
                    score: -raw_score,
                });
            }
            Ok(out)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }
}
