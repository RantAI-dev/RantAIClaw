//! Storage backends for the Knowledge Base.
//!
//! The [`KbStore`] trait is the seam between the higher-level KB pipeline
//! (chunking, embedding, retrieval) and the persistence backend. The default
//! implementation in [`sqlite`] uses rusqlite + sqlite-vec + FTS5. A future
//! `lancedb` backend (gated behind `kb-lancedb`) can implement the same trait
//! without touching any caller.
//!
//! Method shapes mirror the TypeScript `vector-store.ts` surface so the port
//! stays line-by-line auditable.

use async_trait::async_trait;

use crate::kb::{
    Chunk, ChunkId, Document, DocumentId, KbGroup, KbGroupSummary, KbResult, SearchResult,
};

pub mod sqlite;

/// Filter applied to vector + hybrid searches. Mirrors the TS
/// `VectorSearchOptions` filter subset (category, group_ids, document_ids,
/// min_similarity). Default = match-all, which matches TS behavior.
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub category: Option<String>,
    pub group_ids: Vec<String>,
    pub document_ids: Option<Vec<DocumentId>>,
    pub min_similarity: Option<f32>,
}

/// Lexical hit returned by BM25 search. Kept narrower than [`SearchResult`]
/// because BM25 does not produce a similarity-comparable score — callers must
/// either rerank or fuse via RRF before mixing with vector hits.
#[derive(Debug, Clone)]
pub struct Bm25Hit {
    pub id: ChunkId,
    pub document_id: DocumentId,
    pub content: String,
    pub score: f32,
}

#[async_trait]
pub trait KbStore: Send + Sync {
    // --- documents ---
    async fn create_document(&self, doc: &Document) -> KbResult<()>;
    async fn get_document(&self, id: &DocumentId) -> KbResult<Option<Document>>;
    async fn update_document(&self, doc: &Document) -> KbResult<()>;
    async fn delete_document(&self, id: &DocumentId, soft: bool) -> KbResult<()>;
    async fn list_documents(&self, organization_id: Option<&str>) -> KbResult<Vec<Document>>;
    async fn record_retrieval_hits(&self, ids: &[DocumentId]) -> KbResult<()>;

    // --- groups ---
    /// Create a new KB group. Generates the id + timestamps server-side.
    async fn create_group(
        &self,
        name: &str,
        description: Option<&str>,
        color: Option<&str>,
    ) -> KbResult<KbGroup>;
    /// List all groups with a denormalized `document_count` from
    /// `document_group`. Ordered newest-first to match `list_documents`.
    async fn list_groups(&self) -> KbResult<Vec<KbGroupSummary>>;
    /// Fetch a single group by id, or `None` when absent.
    async fn get_group(&self, id: &str) -> KbResult<Option<KbGroup>>;
    /// Update only the provided fields (a `None` leaves the column untouched).
    /// Always bumps `updated_at`. Returns `NotFound` when the group is absent.
    async fn update_group(
        &self,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
        color: Option<&str>,
    ) -> KbResult<()>;
    /// Delete a group and its `document_group` membership rows. Returns
    /// `true` when a row was removed, `false` when the id did not exist.
    async fn delete_group(&self, id: &str) -> KbResult<bool>;
    /// Attach a document to a group. Idempotent (`INSERT OR IGNORE`).
    async fn add_document_to_group(&self, document_id: &str, group_id: &str) -> KbResult<()>;
    /// Detach a document from a group. Returns `true` when a membership row
    /// was removed.
    async fn remove_document_from_group(&self, document_id: &str, group_id: &str)
        -> KbResult<bool>;
    /// List the (non-soft-deleted) documents belonging to a group.
    async fn list_group_documents(&self, group_id: &str) -> KbResult<Vec<Document>>;

    // --- chunks ---
    async fn store_chunks(
        &self,
        document_id: &DocumentId,
        chunks: &[Chunk],
        embeddings: &[Vec<f32>],
        embedding_model: &str,
    ) -> KbResult<()>;
    async fn delete_chunks_by_document(&self, document_id: &DocumentId) -> KbResult<()>;
    async fn chunk_count(&self, document_id: &DocumentId) -> KbResult<usize>;
    async fn chunk_counts(
        &self,
        ids: &[DocumentId],
    ) -> KbResult<std::collections::HashMap<DocumentId, usize>>;

    // --- vector search ---
    async fn search_by_vector(
        &self,
        query: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> KbResult<Vec<SearchResult>>;

    // --- BM25 search ---
    async fn bm25_search(&self, query: &str, limit: usize) -> KbResult<Vec<Bm25Hit>>;

    // --- maintenance ---
    async fn count_by_embedding_model(&self) -> KbResult<Vec<(Option<String>, usize)>>;

    /// Paginated walk over chunks for re-embedding. Skips soft-deleted parent
    /// docs. `after_id` is the last chunk id from the previous page (lexical
    /// ordering on chunk.id), or `None` for the first page. `skip_model`, if
    /// `Some(m)`, excludes chunks already tagged with `m` — used by the bulk
    /// re-embed driver to skip already-current chunks. Returns tuples of
    /// `(chunk_id, content, current_embedding_model)`.
    async fn list_chunks_for_re_embed(
        &self,
        batch_size: usize,
        after_id: Option<&str>,
        skip_model: Option<&str>,
    ) -> KbResult<Vec<(ChunkId, String, Option<String>)>>;

    /// Update an existing chunk's embedding vector and `embedding_model`
    /// tag. Validates `new_embedding.len()` against the store's configured
    /// dim (fails fast with [`crate::kb::KbError::DimensionMismatch`]).
    async fn update_chunk_embedding(
        &self,
        chunk_id: &ChunkId,
        new_embedding: &[f32],
        new_model: &str,
    ) -> KbResult<()>;
}

/// Persist a document then its chunks, rolling the document row back if chunk
/// storage fails. `create_document` and `store_chunks` are separate
/// transactions, so a mid-way failure would otherwise leave an orphan 0-chunk
/// document that lists in the UI but is never retrievable. Rolls back via a
/// hard delete. If `create_document` itself fails, nothing was created and the
/// error is returned as-is.
pub async fn store_document_with_chunks(
    store: &dyn KbStore,
    document: &Document,
    chunks: &[Chunk],
    embeddings: &[Vec<f32>],
    embedding_model: &str,
) -> KbResult<()> {
    store.create_document(document).await?;
    if let Err(e) = store
        .store_chunks(&document.id, chunks, embeddings, embedding_model)
        .await
    {
        // Compensating action — best-effort; the original error is what the
        // caller acts on.
        let _ = store.delete_document(&document.id, false).await;
        return Err(e);
    }
    Ok(())
}
