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

use crate::kb::{Chunk, ChunkId, Document, DocumentId, KbResult, SearchResult};

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
}
