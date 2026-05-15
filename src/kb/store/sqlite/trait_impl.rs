//! Single `KbStore` trait impl for [`super::SqliteStore`] — pure delegation
//! to per-concern inherent methods (`documents.rs`, `chunks.rs`, `bm25.rs`,
//! `drift.rs`). Keeping the trait impl thin makes each task's commit auditable
//! against one concrete file.

use std::collections::HashMap;

use async_trait::async_trait;

use super::SqliteStore;
use crate::kb::store::{Bm25Hit, KbStore, SearchFilter};
use crate::kb::{Chunk, Document, DocumentId, KbError, KbResult, SearchResult};

#[async_trait]
impl KbStore for SqliteStore {
    async fn create_document(&self, doc: &Document) -> KbResult<()> {
        self.create_document_impl(doc).await
    }

    async fn get_document(&self, id: &DocumentId) -> KbResult<Option<Document>> {
        self.get_document_impl(id).await
    }

    async fn update_document(&self, doc: &Document) -> KbResult<()> {
        self.update_document_impl(doc).await
    }

    async fn delete_document(&self, id: &DocumentId, soft: bool) -> KbResult<()> {
        self.delete_document_impl(id, soft).await
    }

    async fn list_documents(&self, organization_id: Option<&str>) -> KbResult<Vec<Document>> {
        self.list_documents_impl(organization_id).await
    }

    async fn record_retrieval_hits(&self, ids: &[DocumentId]) -> KbResult<()> {
        self.record_retrieval_hits_impl(ids).await
    }

    async fn store_chunks(
        &self,
        document_id: &DocumentId,
        chunks: &[Chunk],
        embeddings: &[Vec<f32>],
        embedding_model: &str,
    ) -> KbResult<()> {
        self.store_chunks_impl(document_id, chunks, embeddings, embedding_model)
            .await
    }

    async fn delete_chunks_by_document(&self, document_id: &DocumentId) -> KbResult<()> {
        self.delete_chunks_by_document_impl(document_id).await
    }

    async fn chunk_count(&self, document_id: &DocumentId) -> KbResult<usize> {
        self.chunk_count_impl(document_id).await
    }

    async fn chunk_counts(&self, ids: &[DocumentId]) -> KbResult<HashMap<DocumentId, usize>> {
        self.chunk_counts_impl(ids).await
    }

    async fn search_by_vector(
        &self,
        query: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> KbResult<Vec<SearchResult>> {
        self.search_by_vector_impl(query, limit, filter).await
    }

    async fn bm25_search(&self, _query: &str, _limit: usize) -> KbResult<Vec<Bm25Hit>> {
        Err(KbError::Other("bm25_search: pending Task 2.6".to_string()))
    }

    async fn count_by_embedding_model(&self) -> KbResult<Vec<(Option<String>, usize)>> {
        Err(KbError::Other(
            "count_by_embedding_model: pending Task 2.7".to_string(),
        ))
    }
}
