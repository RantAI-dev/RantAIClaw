//! Contextual retrieval — stub. Task 7.4 will port `contextual-retrieval.ts`.

use crate::kb::KbConfig;

/// Returns a vec of empty strings (one per chunk). Real implementation in
/// Task 7.4 does an OpenRouter chat call per document.
pub async fn generate_contextual_prefixes(
    _cfg: &KbConfig,
    _full_document: &str,
    chunks: &[String],
) -> Vec<String> {
    chunks.iter().map(|_| String::new()).collect()
}
