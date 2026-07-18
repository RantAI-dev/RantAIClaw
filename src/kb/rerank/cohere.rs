//! Cohere v2/rerank provider — Rust port of
//! `src/lib/rag/rerankers/cohere-reranker.ts`.
//!
//! Calls Cohere's managed rerank endpoint with the configured model id
//! (e.g. `rerank-v4.0-pro`, `rerank-v4.0-fast`). Alternative to
//! [`crate::kb::rerank::LlmReranker`].

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use crate::kb::rerank::{fill_remaining_in_order, post_json_rerank, Candidate, Reranked, Reranker};
use crate::kb::{KbConfig, KbError, KbResult};

const DEFAULT_ENDPOINT: &str = "https://api.cohere.com/v2/rerank";

/// Cohere rerank provider. `endpoint` defaults to Cohere's hosted
/// `v2/rerank`; pass `Some(_)` to override (used by `from_env` when
/// `KB_RERANK_BASE_URL` is set and by tests).
pub struct CohereReranker {
    model: String,
    api_key: String,
    endpoint: String,
    http: Client,
}

#[derive(Deserialize)]
struct CohereResponse {
    #[serde(default)]
    results: Vec<CohereResult>,
}

#[derive(Deserialize)]
struct CohereResult {
    index: usize,
    relevance_score: f32,
}

impl CohereReranker {
    pub fn new(model: String, api_key: String, endpoint: Option<String>) -> Self {
        Self {
            model,
            api_key,
            endpoint: endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.into()),
            http: Client::new(),
        }
    }

    /// Resolve config from env. `KB_RERANK_API_KEY` is checked first, then
    /// `COHERE_API_KEY`. An empty key is allowed at construction so the
    /// factory wires successfully — the actual rerank call returns a
    /// `KbError::Config` hard error before any network I/O.
    pub fn from_env(cfg: &KbConfig) -> Self {
        let api_key = std::env::var("KB_RERANK_API_KEY")
            .or_else(|_| std::env::var("COHERE_API_KEY"))
            .unwrap_or_default();
        let endpoint = std::env::var("KB_RERANK_BASE_URL").ok();
        Self::new(cfg.rerank_model.clone(), api_key, endpoint)
    }
}

#[async_trait]
impl Reranker for CohereReranker {
    fn name(&self) -> &str {
        &self.model
    }

    async fn rerank(
        &self,
        query: &str,
        candidates: &[Candidate],
        final_k: usize,
    ) -> KbResult<Vec<Reranked>> {
        if self.api_key.is_empty() {
            return Err(KbError::Config(
                "CohereReranker: apiKey is required (set KB_RERANK_API_KEY or COHERE_API_KEY)"
                    .into(),
            ));
        }

        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        if candidates.len() < final_k {
            return Ok(candidates
                .iter()
                .enumerate()
                .map(|(i, c)| Reranked {
                    id: c.id.clone(),
                    final_rank: i,
                    #[allow(clippy::cast_precision_loss)]
                    score: (final_k - i) as f32,
                })
                .collect());
        }

        let documents: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();
        let body = serde_json::json!({
            "model": &self.model,
            "query": query,
            "documents": documents,
            "top_n": final_k,
        });

        let parsed: CohereResponse =
            post_json_rerank(&self.http, &self.endpoint, Some(&self.api_key), &body).await?;
        let picked: Vec<(usize, f32)> = parsed
            .results
            .into_iter()
            .map(|r| (r.index, r.relevance_score))
            .collect();

        // Filler slots get score 0.0 — matches TS line 74 (`score: 0`).
        Ok(fill_remaining_in_order(
            candidates,
            &picked,
            final_k,
            |_rank, _cand| 0.0,
        ))
    }
}
