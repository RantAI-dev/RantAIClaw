//! Pure-Rust regex extraction of high-precision entity types (no LLM).
use std::sync::LazyLock;

use regex::Regex;

use crate::kb::intelligence::types::EntityType;

static EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").unwrap());
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://[^\s)>\]]+").unwrap());

/// Extract `(name, type)` pairs for the regex-detectable entity types.
/// Dedups identical pairs within the text.
pub fn extract_pattern_entities(text: &str) -> Vec<(String, EntityType)> {
    let mut out: Vec<(String, EntityType)> = Vec::new();
    for m in EMAIL.find_iter(text) {
        out.push((m.as_str().to_string(), EntityType::Email));
    }
    for m in URL.find_iter(text) {
        out.push((
            m.as_str().trim_end_matches('.').to_string(),
            EntityType::Url,
        ));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.as_str().cmp(&b.1.as_str())));
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    out
}
