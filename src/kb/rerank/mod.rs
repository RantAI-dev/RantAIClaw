//! Reranker trait + factory — Phase 8.
//!
//! The Retriever orchestrator (Phase 7.2) accepts an optional
//! `Arc<dyn Reranker>` so the wiring is in place — callers without a
//! configured reranker pass `None` and the rerank stage is skipped.
//!
//! Three concrete implementations live in submodules:
//! - [`llm`] — OpenRouter chat completions as a JSON-array-of-indices reranker.
//! - [`cohere`] — managed Cohere `v2/rerank` API.
//! - [`vllm`] — self-hosted Cohere-shape `/rerank` sidecar.

use async_trait::async_trait;

use crate::kb::{KbConfig, KbResult};

pub mod cohere;
pub mod llm;
pub mod vllm;

pub use cohere::CohereReranker;
pub use llm::LlmReranker;
pub use vllm::VllmReranker;

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

/// Build the configured reranker, or return `None` when rerank is disabled
/// or the selected provider failed to initialize.
///
/// Provider selection mirrors `getDefaultReranker` in `rerankers/index.ts`:
/// - `"vllm"` → [`VllmReranker`] against a self-hosted `/rerank` sidecar
///   (reads `KB_RERANK_BASE_URL`, default `http://localhost:8200`).
/// - `"cohere"` → [`CohereReranker`] against the managed `v2/rerank` API
///   (reads `KB_RERANK_API_KEY`, falling back to `COHERE_API_KEY`).
/// - anything else (including empty / unset) → [`LlmReranker`] over
///   OpenRouter chat completions.
pub fn make_reranker(cfg: &KbConfig) -> Option<Box<dyn Reranker>> {
    if !cfg.rerank_enabled {
        return None;
    }
    match cfg.rerank_provider.to_lowercase().as_str() {
        "vllm" => match VllmReranker::from_env(cfg) {
            Ok(r) => Some(Box::new(r)),
            Err(e) => {
                tracing::warn!(
                    target: "kb::rerank",
                    error = %e,
                    "VllmReranker init failed, skipping rerank stage",
                );
                None
            }
        },
        "cohere" => Some(Box::new(CohereReranker::from_env(cfg))),
        _ => Some(Box::new(LlmReranker::new(
            cfg.rerank_model.clone(),
            cfg.openrouter_chat_url.clone(),
        ))),
    }
}

/// Walk `picked` indices into `candidates`, dedupe by candidate id, then
/// fill remaining slots from `candidates` in original-rank order until
/// `final_k` results are produced (or candidates run out).
///
/// `score_fn(rank, candidate)` mints the per-slot score so each provider can
/// stamp its own signal (LLM = `final_k - rank`, Cohere/vLLM =
/// `relevance_score` for picked slots and `0.0` for filler).
///
/// Extracted once the third caller (`vllm`) lined up the exact same tail
/// (rule-of-three); keeps each provider's `rerank` body focused on its
/// transport + score-source.
pub(crate) fn fill_remaining_in_order<F>(
    candidates: &[Candidate],
    picked: &[(usize, f32)],
    final_k: usize,
    filler_score: F,
) -> Vec<Reranked>
where
    F: Fn(usize, &Candidate) -> f32,
{
    let mut out: Vec<Reranked> = Vec::with_capacity(final_k.min(candidates.len()));
    let mut picked_ids = std::collections::HashSet::<String>::new();

    for &(idx, score) in picked {
        if out.len() >= final_k {
            break;
        }
        let Some(cand) = candidates.get(idx) else {
            continue;
        };
        if !picked_ids.insert(cand.id.clone()) {
            continue;
        }
        let rank = out.len();
        out.push(Reranked {
            id: cand.id.clone(),
            final_rank: rank,
            score,
        });
    }

    for cand in candidates {
        if out.len() >= final_k {
            break;
        }
        if !picked_ids.insert(cand.id.clone()) {
            continue;
        }
        let rank = out.len();
        let score = filler_score(rank, cand);
        out.push(Reranked {
            id: cand.id.clone(),
            final_rank: rank,
            score,
        });
    }

    out
}
