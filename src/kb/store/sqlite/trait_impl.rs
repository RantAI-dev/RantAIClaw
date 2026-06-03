//! Single `KbStore` trait impl for [`super::SqliteStore`] — pure delegation
//! to per-concern inherent methods (`documents.rs`, `chunks.rs`, `bm25.rs`,
//! `drift.rs`). Keeping the trait impl thin makes each task's commit auditable
//! against one concrete file.

use std::collections::HashMap;

use async_trait::async_trait;

use super::SqliteStore;
use crate::kb::store::{Bm25Hit, KbStore, SearchFilter};
use crate::kb::{Chunk, ChunkId, Document, DocumentId, KbResult, SearchResult};

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

    async fn bm25_search(&self, query: &str, limit: usize) -> KbResult<Vec<Bm25Hit>> {
        self.bm25_search_impl(query, limit).await
    }

    async fn count_by_embedding_model(&self) -> KbResult<Vec<(Option<String>, usize)>> {
        self.count_by_embedding_model_impl().await
    }

    async fn list_chunks_for_re_embed(
        &self,
        batch_size: usize,
        after_id: Option<&str>,
        skip_model: Option<&str>,
    ) -> KbResult<Vec<(ChunkId, String, Option<String>)>> {
        self.list_chunks_for_re_embed_impl(batch_size, after_id, skip_model)
            .await
    }

    async fn update_chunk_embedding(
        &self,
        chunk_id: &ChunkId,
        new_embedding: &[f32],
        new_model: &str,
    ) -> KbResult<()> {
        self.update_chunk_embedding_impl(chunk_id, new_embedding, new_model)
            .await
    }
}
