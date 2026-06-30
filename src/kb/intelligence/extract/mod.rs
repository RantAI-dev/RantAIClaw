//! Entity/relation extractors.

use crate::kb::intelligence::types::{EntityType, RelationType};
use crate::kb::KbResult;

/// `(name, type, confidence)` tuples for entities, and
/// `(source_name, target_name, type, confidence)` for relations.
#[derive(Debug, Default)]
pub struct Extracted {
    pub entities: Vec<(String, EntityType, f32)>,
    pub relations: Vec<(String, String, RelationType, f32)>,
}

#[async_trait::async_trait]
pub trait EntityRelationExtractor: Send + Sync {
    async fn extract(&self, chunks: &[&str]) -> KbResult<Extracted>;
}

pub mod llm;
pub mod pattern;
