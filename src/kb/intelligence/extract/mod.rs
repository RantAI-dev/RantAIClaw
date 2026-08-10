//! Entity/relation extractors.

use crate::kb::intelligence::types::{EntityType, RelationType};
use crate::kb::KbResult;

/// `(chunk_index, name, type, confidence)` tuples for entities, and
/// `(source_name, target_name, type, confidence)` for relations.
///
/// The chunk index is load-bearing: GraphRAG's chunk join
/// (`graph_expand_chunks`) matches mentions to chunks on
/// `m.chunk_index = c.chunk_index`, and `NULL = <int>` is never true in
/// SQL — a mention stored without its chunk index is invisible to
/// retrieval. Relations stay document-level; they are wired by entity
/// name across the whole document.
#[derive(Debug, Default)]
pub struct Extracted {
    pub entities: Vec<(usize, String, EntityType, f32)>,
    pub relations: Vec<(String, String, RelationType, f32)>,
    /// Chunks the extractor could not process at all. A non-zero value with
    /// zero entities means extraction FAILED — it does not mean the document
    /// has no entities. Callers must be able to tell those apart.
    pub failed_chunks: usize,
    /// First failure reason, for operator-facing display. Never contains the
    /// upstream body or any credential.
    pub first_error: Option<String>,
}

#[async_trait::async_trait]
pub trait EntityRelationExtractor: Send + Sync {
    async fn extract(&self, chunks: &[&str]) -> KbResult<Extracted>;
}

pub mod llm;
pub mod pattern;
