//! vLLM / FastAPI rerank sidecar provider — Rust port of
//! `src/lib/rag/rerankers/vllm-reranker.ts`.
//!
//! Self-hosted `/rerank` endpoint that matches Cohere's v2 request/response
//! shape. Used to serve open-weight cross-encoder rerankers (e.g.
//! `nvidia/llama-nemotron-rerank-1b-v2`) next to the platform — no auth,
//! configurable base URL.

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use crate::kb::rerank::{fill_remaining_in_order, Candidate, Reranked, Reranker};
use crate::kb::{KbConfig, KbError, KbResult};

const DEFAULT_BASE_URL: &str = "http://localhost:8200";

/// Self-hosted Cohere-compatible reranker. Constructs the request URL as
/// `{base_url}/rerank`; trailing slashes are stripped so callers can pass
/// either `http://host:port` or `http://host:port/`.
pub struct VllmReranker {
    model: String,
    endpoint: String,
    http: Client,
}

#[derive(Deserialize)]
struct VllmResponse {
    #[serde(default)]
    results: Vec<VllmResult>,
}

#[derive(Deserialize)]
struct VllmResult {
    index: usize,
    relevance_score: f32,
}

impl VllmReranker {
    pub fn new(base_url: String, model: String) -> KbResult<Self> {
        if base_url.is_empty() {
            return Err(KbError::Config("VllmReranker requires base_url".into()));
        }
        let trimmed = base_url.trim_end_matches('/');
        Ok(Self {
            model,
            endpoint: format!("{trimmed}/rerank"),
            http: Client::new(),
        })
    }

    /// Resolve config from env. `KB_RERANK_BASE_URL` defaults to
    /// `http://localhost:8200` so `make_reranker` produces a working
    /// pointer for the default vLLM sidecar layout.
    pub fn from_env(cfg: &KbConfig) -> KbResult<Self> {
        let base_url =
            std::env::var("KB_RERANK_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
        Self::new(base_url, cfg.rerank_model.clone())
    }
}

#[async_trait]
impl Reranker for VllmReranker {
    fn name(&self) -> &str {
        &self.model
    }

    async fn rerank(
        &self,
        query: &str,
        candidates: &[Candidate],
        final_k: usize,
    ) -> KbResult<Vec<Reranked>> {
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

        // Intentionally no `Authorization` header — sidecar runs in-cluster.
        let resp = self.http.post(&self.endpoint).json(&body).send().await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KbError::ChatApi {
                status: status.as_u16(),
                body: truncate(&text, 300),
            });
        }

        let parsed: VllmResponse = resp.json().await?;
        let picked: Vec<(usize, f32)> = parsed
            .results
            .into_iter()
            .map(|r| (r.index, r.relevance_score))
            .collect();

        Ok(fill_remaining_in_order(
            candidates,
            &picked,
            final_k,
            |_rank, _cand| 0.0,
        ))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let cut = s
            .char_indices()
            .take(max)
            .last()
            .map(|(idx, ch)| idx + ch.len_utf8())
            .unwrap_or(0);
        format!("{}…", &s[..cut])
    }
}
