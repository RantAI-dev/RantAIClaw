//! KB document intelligence — entity + relation extraction → cross-document graph.
pub mod extract;
pub mod resolve;
pub mod types;

use std::collections::HashMap;

use crate::kb::intelligence::extract::pattern::extract_pattern_entities;
use crate::kb::intelligence::extract::EntityRelationExtractor;
use crate::kb::intelligence::resolve::{canonical_key, normalize_name};
use crate::kb::intelligence::types::{Entity, EntityMention, ExtractSource, Relation};
use crate::kb::store::IntelligenceStore;
use crate::kb::KbResult;

/// Counts returned for logging / API response.
///
/// `entities` answers "how many distinct entities does THIS document
/// contribute" — distinct `canonical_key`s in this extraction run, the same
/// key `store_intelligence` dedups rows on, so the number matches the
/// Entities tab beside it. It still legitimately differs from the graph's
/// `total_entities` (corpus-wide, cross-document) — two questions, two
/// answers. `relations` counts relation rows actually handed to the store,
/// not raw model output.
#[derive(Debug, Clone)]
pub struct IntelligenceSummary {
    pub entities: usize,
    pub relations: usize,
    /// Chunks the extractor failed on. Non-zero with zero entities means
    /// extraction FAILED, not "no entities" (plan 109).
    pub failed_chunks: usize,
    /// First failure reason (short, no upstream body, no credential).
    pub error: Option<String>,
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
    let mut entities: Vec<Entity> = Vec::new();
    let mut mentions: Vec<EntityMention> = Vec::new();

    // 1) LLM entities — one mention per (entity, chunk). The chunk index MUST
    // be stored: GraphRAG's chunk join matches on
    // `m.chunk_index = c.chunk_index`, and a NULL index never matches, so a
    // document-level mention is invisible to retrieval (the pre-fix state:
    // only pattern entities could ever surface a chunk).
    let llm = extractor.extract(chunks).await?;
    for (chunk_index, name, ty, conf) in &llm.entities {
        let id = new_id();
        entities.push(Entity {
            id: id.clone(),
            canonical_key: canonical_key(name, ty),
            name: name.clone(),
            entity_type: ty.clone(),
            confidence: *conf,
            metadata: serde_json::json!({}),
        });
        mentions.push(EntityMention {
            id: new_id(),
            entity_id: id.clone(),
            document_id: document_id.to_string(),
            chunk_index: Some(i64::try_from(*chunk_index).unwrap_or(0)),
            context: None,
            source: ExtractSource::Llm,
        });
        // Keyed by the same normalization entity dedup uses. Two entities
        // sharing a name but not a type collide here; or_insert keeps the
        // first — the same tie-break canonical_key dedup applies, and better
        // than dropping the edge.
        entity_id_by_name.entry(normalize_name(name)).or_insert(id);
    }

    // 2) Pattern entities — per chunk (chunk_index = Some(idx)).
    for (idx, chunk) in chunks.iter().enumerate() {
        let chunk_index = i64::try_from(idx).unwrap_or(0);
        for (name, ty) in extract_pattern_entities(chunk) {
            let id = new_id();
            entities.push(Entity {
                id: id.clone(),
                canonical_key: canonical_key(&name, &ty),
                name: name.clone(),
                entity_type: ty.clone(),
                confidence: 1.0,
                metadata: serde_json::json!({}),
            });
            mentions.push(EntityMention {
                id: new_id(),
                entity_id: id.clone(),
                document_id: document_id.to_string(),
                chunk_index: Some(chunk_index),
                context: None,
                source: ExtractSource::Pattern,
            });
            entity_id_by_name.entry(normalize_name(&name)).or_insert(id);
        }
    }

    // 3) Relations (from the LLM), wired by NORMALIZED entity name — the
    // same normalization canonical_key uses for entity dedup, so a casing or
    // punctuation mismatch between the model's entities and relations arrays
    // no longer deletes the edge.
    let mut relations: Vec<Relation> = Vec::new();
    let mut dropped_relations = 0usize;
    for (src, tgt, rty, conf) in &llm.relations {
        match (
            entity_id_by_name.get(&normalize_name(src)),
            entity_id_by_name.get(&normalize_name(tgt)),
        ) {
            (Some(s), Some(t)) => {
                relations.push(Relation {
                    id: new_id(),
                    source_entity_id: s.clone(),
                    target_entity_id: t.clone(),
                    relation_type: rty.clone(),
                    confidence: *conf,
                    document_id: document_id.to_string(),
                    metadata: serde_json::json!({}),
                });
            }
            _ => dropped_relations += 1,
        }
    }
    if dropped_relations > 0 {
        // Names are document content — count them, never log them.
        tracing::warn!(
            target: "kb::intelligence",
            document_id,
            dropped = dropped_relations,
            "relations referenced entity names with no extracted entity"
        );
    }

    // One transactional round-trip for the whole batch. The store resolves
    // each entity's provisional id to its surviving (canonical_key-deduped)
    // id and remaps mentions/relations accordingly — see
    // `IntelligenceStore::store_intelligence`.
    store
        .store_intelligence(document_id, &entities, &mentions, &relations)
        .await?;

    // Count what is stored, not what was extracted: dedup by the same
    // canonical_key the store dedups rows on (see the doc comment on
    // IntelligenceSummary). Raw extraction counts drift arbitrarily far
    // from the visible graph — one email in ten chunks is ten extractions
    // and one row.
    let unique_entities = entities
        .iter()
        .map(|e| e.canonical_key.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    Ok(IntelligenceSummary {
        entities: unique_entities,
        relations: relations.len(),
        failed_chunks: llm.failed_chunks,
        error: llm.first_error,
    })
}
