//! Chunk insert + vector-search portion of [`super::SqliteStore`].
//!
//! Vector storage flows through the sqlite-vec `vec0` virtual table, joined
//! back to `chunk` + `document` on the chunk row id. The dimension contract is
//! enforced **before** any INSERT runs — see [`SqliteStore::store_chunks_impl`]
//! — to match the TS guard in `vector-store.ts:502-534`.

use std::collections::HashMap;

use rusqlite::params;

use super::SqliteStore;
use crate::kb::store::SearchFilter;
use crate::kb::{Chunk, ChunkId, ChunkMetadata, DocumentId, KbError, KbResult, SearchResult};

/// Serialize a slice of `f32`s into the little-endian byte layout the
/// sqlite-vec `vec0` virtual table expects. The crate's 0.1 release does
/// not re-export the C `serialize_float32` helper, but the wire format is
/// just `len * 4` bytes of little-endian f32 (see `sqlite-vec.c:704-727`).
fn serialize_float32(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(std::mem::size_of_val(v));
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

impl SqliteStore {
    pub(crate) async fn store_chunks_impl(
        &self,
        document_id: &DocumentId,
        chunks: &[Chunk],
        embeddings: &[Vec<f32>],
        embedding_model: &str,
    ) -> KbResult<()> {
        if chunks.len() != embeddings.len() {
            return Err(KbError::LengthMismatch {
                kind: "chunks vs embeddings",
                left: chunks.len(),
                right: embeddings.len(),
            });
        }
        // Up-front dimension validation — fail before touching the DB so a
        // partial insert can't leave the FTS/vec tables out of sync.
        for (i, emb) in embeddings.iter().enumerate() {
            if emb.len() != self.embedding_dim {
                return Err(KbError::DimensionMismatch {
                    expected: self.embedding_dim,
                    got: emb.len(),
                    index: i,
                });
            }
        }
        if chunks.is_empty() {
            return Ok(());
        }

        let conn = self.conn.clone();
        let document_id = document_id.0.clone();
        let chunks_owned: Vec<Chunk> = chunks.to_vec();
        let embeddings_owned: Vec<Vec<u8>> =
            embeddings.iter().map(|v| serialize_float32(v)).collect();
        let embedding_model = embedding_model.to_string();

        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let mut conn = conn.blocking_lock();
            let tx = conn.transaction()?;
            {
                let mut insert_chunk = tx.prepare(
                    "INSERT INTO chunk (
                        id, document_id, content, chunk_index, metadata_json,
                        contextual_prefix, embedding_model, created_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                )?;
                let mut insert_vec =
                    tx.prepare("INSERT INTO chunk_vec (rowid, embedding) VALUES (?1, ?2)")?;

                for (chunk, embedding_bytes) in chunks_owned.iter().zip(embeddings_owned.iter()) {
                    let chunk_id = format!("{}_{}", document_id, chunk.metadata.chunk_index);
                    let metadata_json = serde_json::to_string(&chunk.metadata)?;
                    insert_chunk.execute(params![
                        chunk_id,
                        document_id,
                        chunk.content,
                        chunk.metadata.chunk_index as i64,
                        metadata_json,
                        chunk.metadata.contextual_prefix,
                        embedding_model,
                    ])?;
                    // chunk.rowid is auto-assigned (INTEGER PRIMARY KEY alias
                    // is not used because `id` is TEXT). Pull the just-inserted
                    // rowid so the vec0 row links 1:1 to the chunk row.
                    let rowid = tx.last_insert_rowid();
                    insert_vec.execute(params![rowid, embedding_bytes])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    pub(crate) async fn delete_chunks_by_document_impl(
        &self,
        document_id: &DocumentId,
    ) -> KbResult<()> {
        let conn = self.conn.clone();
        let document_id = document_id.0.clone();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let mut conn = conn.blocking_lock();
            let tx = conn.transaction()?;
            {
                // chunk_vec is not linked by FK to chunk (vec0 doesn't support
                // FK enforcement). Mirror the delete manually via the rowids
                // we're about to drop from chunk.
                let mut stmt = tx.prepare("SELECT rowid FROM chunk WHERE document_id = ?1")?;
                let rowids: Vec<i64> = stmt
                    .query_map(params![document_id], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                drop(stmt);
                let mut del_vec = tx.prepare("DELETE FROM chunk_vec WHERE rowid = ?1")?;
                for rid in &rowids {
                    del_vec.execute(params![rid])?;
                }
                drop(del_vec);
                tx.execute(
                    "DELETE FROM chunk WHERE document_id = ?1",
                    params![document_id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(crate) async fn chunk_count_impl(&self, document_id: &DocumentId) -> KbResult<usize> {
        let conn = self.conn.clone();
        let document_id = document_id.0.clone();
        tokio::task::spawn_blocking(move || -> KbResult<usize> {
            let conn = conn.blocking_lock();
            // Join document to filter soft-deleted parents — chunks for a
            // soft-deleted doc must be logically invisible (matches the read
            // path in search_by_vector / get_document).
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM chunk c
                 JOIN document d ON d.id = c.document_id
                 WHERE c.document_id = ?1 AND d.deleted_at IS NULL",
                params![document_id],
                |row| row.get(0),
            )?;
            Ok(count.max(0) as usize)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(crate) async fn chunk_counts_impl(
        &self,
        ids: &[DocumentId],
    ) -> KbResult<HashMap<DocumentId, usize>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.conn.clone();
        let ids: Vec<String> = ids.iter().map(|d| d.0.clone()).collect();
        tokio::task::spawn_blocking(move || -> KbResult<HashMap<DocumentId, usize>> {
            let conn = conn.blocking_lock();
            let mut out = HashMap::new();
            // Same soft-delete guard as chunk_count_impl — missing-doc and
            // soft-deleted-doc both collapse to 0 so callers can rely on the
            // logical invariant `chunk_count(doc) == 0 iff doc invisible`.
            let mut stmt = conn.prepare(
                "SELECT COUNT(*) FROM chunk c
                 JOIN document d ON d.id = c.document_id
                 WHERE c.document_id = ?1 AND d.deleted_at IS NULL",
            )?;
            for id in &ids {
                let count: i64 = stmt.query_row(params![id], |row| row.get(0))?;
                out.insert(DocumentId(id.clone()), count.max(0) as usize);
            }
            Ok(out)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    /// Paginated walk over chunks for re-embedding (Phase 9.2 / TS
    /// `bulk-re-embed.ts`). Joins `document` to skip soft-deleted parents.
    /// When `skip_model` is `Some(m)`, rows tagged with `m` are excluded —
    /// NULL-tagged rows are always included (treated as "needs re-embed").
    pub(crate) async fn list_chunks_for_re_embed_impl(
        &self,
        batch_size: usize,
        after_id: Option<&str>,
        skip_model: Option<&str>,
    ) -> KbResult<Vec<(ChunkId, String, Option<String>)>> {
        if batch_size == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let after_id = after_id.map(str::to_string);
        let skip_model = skip_model.map(str::to_string);
        let batch_size_i = batch_size as i64;

        tokio::task::spawn_blocking(
            move || -> KbResult<Vec<(ChunkId, String, Option<String>)>> {
                let conn = conn.blocking_lock();
                // Build SQL with optional predicates inlined as `?` placeholders.
                // Lexical ordering on TEXT id is deterministic, which is enough
                // for pagination — the bulk driver never relies on a particular
                // visit order, only that each chunk is visited exactly once.
                let mut sql = String::from(
                    "SELECT c.id, c.content, c.embedding_model
                     FROM chunk c
                     JOIN document d ON d.id = c.document_id
                     WHERE d.deleted_at IS NULL",
                );
                let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                if let Some(skip) = &skip_model {
                    sql.push_str(" AND (c.embedding_model IS NULL OR c.embedding_model != ?)");
                    params.push(Box::new(skip.clone()));
                }
                if let Some(after) = &after_id {
                    sql.push_str(" AND c.id > ?");
                    params.push(Box::new(after.clone()));
                }
                sql.push_str(" ORDER BY c.id LIMIT ?");
                params.push(Box::new(batch_size_i));

                let mut stmt = conn.prepare(&sql)?;
                let param_refs: Vec<&dyn rusqlite::ToSql> =
                    params.iter().map(|b| b.as_ref()).collect();
                let rows = stmt
                    .query_map(param_refs.as_slice(), |row| {
                        let id: String = row.get(0)?;
                        let content: String = row.get(1)?;
                        let model: Option<String> = row.get(2)?;
                        Ok((ChunkId(id), content, model))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            },
        )
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    /// Replace an existing chunk's embedding vector and model tag. Validates
    /// the new vector against [`SqliteStore::embedding_dim`] before touching
    /// the DB so a bad dim can't half-write `chunk_vec` vs `chunk`. The
    /// two-statement UPDATE runs inside a single transaction so a crash
    /// between them can't desync the metadata and vector tables.
    pub(crate) async fn update_chunk_embedding_impl(
        &self,
        chunk_id: &ChunkId,
        new_embedding: &[f32],
        new_model: &str,
    ) -> KbResult<()> {
        if new_embedding.len() != self.embedding_dim {
            return Err(KbError::DimensionMismatch {
                expected: self.embedding_dim,
                got: new_embedding.len(),
                index: 0,
            });
        }
        let conn = self.conn.clone();
        let chunk_id_s = chunk_id.0.clone();
        let bytes = serialize_float32(new_embedding);
        let new_model_s = new_model.to_string();

        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let mut conn = conn.blocking_lock();
            let tx = conn.transaction()?;
            // Look up the chunk's rowid — `chunk_vec.rowid` mirrors
            // `chunk.rowid` (set explicitly at insert time in
            // `store_chunks_impl`). Missing rowid → NotFound.
            let rowid: i64 = match tx.query_row(
                "SELECT rowid FROM chunk WHERE id = ?1",
                params![chunk_id_s],
                |row| row.get(0),
            ) {
                Ok(r) => r,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    return Err(KbError::NotFound(format!("chunk {chunk_id_s}")));
                }
                Err(e) => return Err(e.into()),
            };
            tx.execute(
                "UPDATE chunk_vec SET embedding = ?1 WHERE rowid = ?2",
                params![bytes, rowid],
            )?;
            tx.execute(
                "UPDATE chunk SET embedding_model = ?1 WHERE id = ?2",
                params![new_model_s, chunk_id_s],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    pub(crate) async fn search_by_vector_impl(
        &self,
        query: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> KbResult<Vec<SearchResult>> {
        if query.len() != self.embedding_dim {
            return Err(KbError::DimensionMismatch {
                expected: self.embedding_dim,
                got: query.len(),
                index: 0,
            });
        }
        let conn = self.conn.clone();
        let query_bytes = serialize_float32(query);
        let limit_i = limit as i64;
        let filter = filter.clone();

        tokio::task::spawn_blocking(move || -> KbResult<Vec<SearchResult>> {
            let conn = conn.blocking_lock();

            // Resolve a candidate set of allowed document IDs once, so the
            // per-row `WHERE` stays cheap. Mirrors the TS pre-filter pattern
            // in vector-store.ts:71-82.
            let allowed_doc_ids = resolve_allowed_documents(&conn, &filter)?;

            // Over-fetch from vec0 then post-filter in Rust. vec0's MATCH
            // operator does not accept arbitrary WHERE clauses on joined
            // tables, so we pull a larger KNN window and filter afterwards.
            // Heuristic factor of 4 keeps the result set bounded under
            // typical document/category filters.
            let knn_limit = (limit_i.saturating_mul(4)).max(limit_i).max(8);

            let mut stmt = conn.prepare(
                "SELECT v.rowid, v.distance
                 FROM chunk_vec v
                 WHERE v.embedding MATCH ?1 AND k = ?2
                 ORDER BY v.distance",
            )?;
            let mut rows = stmt.query(params![query_bytes, knn_limit])?;

            let mut hits: Vec<(i64, f32)> = Vec::new();
            while let Some(row) = rows.next()? {
                let rid: i64 = row.get(0)?;
                let dist: f32 = row.get(1)?;
                hits.push((rid, dist));
            }
            drop(rows);
            drop(stmt);

            // Join back to chunk + document and apply filters.
            let mut chunk_stmt = conn.prepare(
                "SELECT c.id, c.document_id, c.content, c.metadata_json,
                        c.contextual_prefix, c.chunk_index,
                        d.title, d.categories_json, d.subcategory
                 FROM chunk c
                 JOIN document d ON d.id = c.document_id
                 WHERE c.rowid = ?1 AND d.deleted_at IS NULL",
            )?;

            let mut results = Vec::with_capacity(limit);
            for (rid, dist) in hits {
                let mut crow = chunk_stmt.query(params![rid])?;
                let Some(r) = crow.next()? else {
                    continue;
                };
                let chunk_id: String = r.get("id")?;
                let document_id: String = r.get("document_id")?;
                if let Some(allowed) = &allowed_doc_ids {
                    if !allowed.contains(&document_id) {
                        continue;
                    }
                }
                let categories_json: String = r.get("categories_json")?;
                let categories: Vec<String> =
                    serde_json::from_str(&categories_json).unwrap_or_default();
                let metadata_json: String = r.get("metadata_json")?;
                let metadata: ChunkMetadata =
                    serde_json::from_str(&metadata_json).unwrap_or_else(|_| ChunkMetadata {
                        document_title: r.get("title").unwrap_or_default(),
                        category: categories.first().cloned().unwrap_or_default(),
                        subcategory: r.get("subcategory").ok(),
                        section: None,
                        chunk_index: 0,
                        contextual_prefix: r.get("contextual_prefix").ok().flatten(),
                    });

                // `chunk_vec` is created with `distance_metric=cosine` (see
                // schema.rs), so `dist` here is cosine distance in [0, 2].
                // cosine_similarity = 1 - cosine_distance — this conversion
                // is only correct because the vec0 column is cosine; if the
                // distance metric ever changes, revisit this line first.
                let similarity = 1.0 - dist;
                if let Some(min) = filter.min_similarity {
                    if similarity < min {
                        continue;
                    }
                }

                results.push(SearchResult {
                    id: ChunkId(chunk_id),
                    document_id: DocumentId(document_id),
                    document_title: r.get("title")?,
                    content: r.get("content")?,
                    categories,
                    subcategory: r.get("subcategory")?,
                    section: metadata.section.clone(),
                    similarity,
                    contextual_prefix: metadata.contextual_prefix.clone(),
                });
                if results.len() >= limit {
                    break;
                }
            }
            Ok(results)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }
}

/// Resolve the set of document IDs allowed by `filter`. `None` means "no
/// restriction" — when category and group_ids and document_ids are all empty,
/// we skip the pre-filter entirely.
///
/// The category / group / document-id filters are **unioned** (a document is in
/// scope if it matches ANY of them), so a chat can search its conversation
/// attachments (`category`) together with one or more selected knowledge bases
/// (`group_ids`) in a single call.
fn resolve_allowed_documents(
    conn: &rusqlite::Connection,
    filter: &SearchFilter,
) -> KbResult<Option<std::collections::HashSet<String>>> {
    let no_category = filter.category.is_none();
    let no_groups = filter.group_ids.is_empty();
    let no_docs = filter
        .document_ids
        .as_ref()
        .map(|v| v.is_empty())
        .unwrap_or(true);
    if no_category && no_groups && no_docs {
        return Ok(None);
    }

    let mut allowed: Option<std::collections::HashSet<String>> = None;

    if let Some(category) = &filter.category {
        // Use `json_each` for proper array-element membership. The previous
        // `LIKE '%' || ?1 || '%'` matched substrings ("A" matched "FAQ") and
        // expanded SQL wildcards (`_`, `%`) in the user-supplied param —
        // either of those could silently broaden category access. `json_each`
        // is the JSON-aware equivalent of `category IN (...)`.
        let mut stmt = conn.prepare(
            "SELECT id FROM document
             WHERE deleted_at IS NULL
               AND EXISTS (SELECT 1 FROM json_each(categories_json) WHERE value = ?1)",
        )?;
        let ids: std::collections::HashSet<String> = stmt
            .query_map(params![category], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        allowed = Some(match allowed {
            Some(prev) => prev.union(&ids).cloned().collect(),
            None => ids,
        });
    }

    if !filter.group_ids.is_empty() {
        // Inline `IN (?,?,?)` — group_ids is bounded by call sites.
        let placeholders = std::iter::repeat_n("?", filter.group_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql =
            format!("SELECT document_id FROM document_group WHERE group_id IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = filter
            .group_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let ids: std::collections::HashSet<String> = stmt
            .query_map(params.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        allowed = Some(match allowed {
            Some(prev) => prev.union(&ids).cloned().collect(),
            None => ids,
        });
    }

    if let Some(doc_ids) = &filter.document_ids {
        let ids: std::collections::HashSet<String> = doc_ids.iter().map(|d| d.0.clone()).collect();
        allowed = Some(match allowed {
            Some(prev) => prev.union(&ids).cloned().collect(),
            None => ids,
        });
    }

    Ok(allowed)
}
