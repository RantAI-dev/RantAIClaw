//! Cross-document entity resolution. The canonical key is what merges the same
//! entity across documents into one global node.
use crate::kb::intelligence::types::EntityType;

/// The name half of [`canonical_key`]. Public so relation wiring uses the
/// exact same normalization entity dedup uses — a divergence here silently
/// drops edges: the LLM routinely varies casing/punctuation between its
/// `entities` and `relations` arrays, and a relation whose endpoint doesn't
/// match an entity byte-for-byte was discarded (plan 094).
pub fn normalize_name(name: &str) -> String {
    name.trim()
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// `normalize(name):type` — lowercase, trim, collapse internal whitespace,
/// strip surrounding punctuation. Default `exact` resolution strategy.
pub fn canonical_key(name: &str, entity_type: &EntityType) -> String {
    format!("{}:{}", normalize_name(name), entity_type.as_str())
}
