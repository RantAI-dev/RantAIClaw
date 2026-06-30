//! KB document intelligence — entity + relation extraction → cross-document graph.
pub mod extract;
pub mod resolve;
pub mod types;

use std::collections::HashMap;

use crate::kb::intelligence::extract::pattern::extract_pattern_entities;
use crate::kb::intelligence::extract::EntityRelationExtractor;
use crate::kb::intelligence::resolve::canonical_key;
use crate::kb::intelligence::types::{Entity, EntityMention, ExtractSource, Relation};
use crate::kb::store::IntelligenceStore;
use crate::kb::KbResult;

/// Counts returned for logging / API response.
#[derive(Debug, Clone, Copy)]
pub struct IntelligenceSummary {
    pub entities: usize,
    pub relations: usize,
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Idempotent: clears the document's prior mentions/relations, runs extraction
/// (LLM + regex pattern), resolves entities globally by canonical key, then stores
/// mentions + relations. `_resolution` is the strategy string (currently only "exact").
pub async fn extract_document_intelligence(
    store: &dyn IntelligenceStore,
    extractor: &dyn EntityRelationExtractor,
    document_id: &str,
    chunks: &[&str],
    _resolution: &str,
) -> KbResult<IntelligenceSummary> {
    // Idempotent re-extract: drop this doc's prior mentions/relations first.
    store.delete_document_intelligence(document_id).await?;

    let mut entity_id_by_name: HashMap<String, String> = HashMap::new();
    let mut n_ent = 0usize;

    // 1) LLM entities — aggregated for the whole document (chunk_index = None).
    let llm = extractor.extract(chunks).await?;
    for (name, ty, conf) in &llm.entities {
        let entity = Entity {
            id: new_id(),
            canonical_key: canonical_key(name, ty),
            name: name.clone(),
            entity_type: ty.clone(),
            confidence: *conf,
            metadata: serde_json::json!({}),
        };
        let id = store.upsert_entity(&entity).await?;
        store
            .add_mention(&EntityMention {
                id: new_id(),
                entity_id: id.clone(),
                document_id: document_id.to_string(),
                chunk_index: None,
                context: None,
                source: ExtractSource::Llm,
            })
            .await?;
        entity_id_by_name.entry(name.clone()).or_insert(id);
        n_ent += 1;
    }

    // 2) Pattern entities — per chunk (chunk_index = Some(idx)).
    for (idx, chunk) in chunks.iter().enumerate() {
        let chunk_index = i64::try_from(idx).unwrap_or(0);
        for (name, ty) in extract_pattern_entities(chunk) {
            let entity = Entity {
                id: new_id(),
                canonical_key: canonical_key(&name, &ty),
                name: name.clone(),
                entity_type: ty.clone(),
                confidence: 1.0,
                metadata: serde_json::json!({}),
            };
            let id = store.upsert_entity(&entity).await?;
            store
                .add_mention(&EntityMention {
                    id: new_id(),
                    entity_id: id.clone(),
                    document_id: document_id.to_string(),
                    chunk_index: Some(chunk_index),
                    context: None,
                    source: ExtractSource::Pattern,
                })
                .await?;
            entity_id_by_name.entry(name).or_insert(id);
            n_ent += 1;
        }
    }

    // 3) Relations (from the LLM), wired by entity name.
    let mut n_rel = 0usize;
    for (src, tgt, rty, conf) in &llm.relations {
        if let (Some(s), Some(t)) = (entity_id_by_name.get(src), entity_id_by_name.get(tgt)) {
            store
                .add_relation(&Relation {
                    id: new_id(),
                    source_entity_id: s.clone(),
                    target_entity_id: t.clone(),
                    relation_type: rty.clone(),
                    confidence: *conf,
                    document_id: document_id.to_string(),
                    metadata: serde_json::json!({}),
                })
                .await?;
            n_rel += 1;
        }
    }

    Ok(IntelligenceSummary {
        entities: n_ent,
        relations: n_rel,
    })
}
