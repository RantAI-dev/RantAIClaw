//! Cross-document entity resolution. The canonical key is what merges the same
//! entity across documents into one global node.
use crate::kb::intelligence::types::EntityType;

/// `normalize(name):type` — lowercase, trim, collapse internal whitespace,
/// strip surrounding punctuation. Default `exact` resolution strategy.
pub fn canonical_key(name: &str, entity_type: &EntityType) -> String {
    let normalized = name
        .trim()
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    format!("{normalized}:{}", entity_type.as_str())
}
