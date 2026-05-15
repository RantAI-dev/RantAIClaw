//! Query expansion — stub. Task 7.3 will port `query-expansion.ts`.
//!
//! For now this returns `[query]` so the Retriever orchestrator (Task 7.2)
//! works end-to-end without the LLM round-trip.

use crate::kb::KbConfig;

/// Returns `[query]` (single-element list). Replaced in Task 7.3 with a real
/// OpenRouter-backed paraphrase generator.
pub async fn expand_query(_cfg: &KbConfig, query: &str) -> Vec<String> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    vec![q.to_string()]
}
