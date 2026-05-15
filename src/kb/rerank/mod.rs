//! Reranker trait + factory — Phase 8.
//!
//! This module currently exposes only the trait surface. Concrete
//! implementations (LlmReranker, CohereReranker, VllmReranker) land in
//! Phase 8 of the KB port. The Retriever orchestrator (Phase 7.2) accepts
//! an optional `Arc<dyn Reranker>` so the wiring is in place before the
//! implementations exist — callers without a configured reranker simply
//! pass `None` and the rerank stage is skipped.

use async_trait::async_trait;

use crate::kb::KbResult;

/// One candidate fed into the reranker. `original_rank` + `original_score`
/// let LlmReranker fill remaining slots from the upstream fused order when
/// the model returns fewer indices than `final_k`.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub text: String,
    pub original_rank: usize,
    pub original_score: f32,
}

/// One reranked output slot. `final_rank` is the 0-based position the
/// reranker chose; `score` is provider-specific (LLM = inverted rank,
/// Cohere/vLLM = native relevance score).
#[derive(Debug, Clone)]
pub struct Reranked {
    pub id: String,
    pub final_rank: usize,
    pub score: f32,
}

/// Reranker contract — fed N candidates, returns up to `final_k` reordered
/// by the provider's relevance signal.
#[async_trait]
pub trait Reranker: Send + Sync {
    /// Display name used in tracing + error fallback messages.
    fn name(&self) -> &str;

    /// Rerank candidates against `query`. Implementations should never panic;
    /// on provider failure return `Err` and let the caller fall back to the
    /// upstream fused order.
    async fn rerank(
        &self,
        query: &str,
        candidates: &[Candidate],
        final_k: usize,
    ) -> KbResult<Vec<Reranked>>;
}
