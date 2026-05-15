//! vLLM / FastAPI rerank sidecar provider — stub. Full impl + tests land in Task 8.4.

use async_trait::async_trait;

use crate::kb::rerank::{Candidate, Reranked, Reranker};
use crate::kb::{KbConfig, KbError, KbResult};

const DEFAULT_BASE_URL: &str = "http://localhost:8200";

/// Self-hosted Cohere-compatible reranker. Endpoint is always
/// `{base_url}/rerank`.
pub struct VllmReranker {
    model: String,
    #[allow(dead_code)] // consumed in Task 8.4
    endpoint: String,
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
        })
    }

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
        _query: &str,
        _candidates: &[Candidate],
        _final_k: usize,
    ) -> KbResult<Vec<Reranked>> {
        // Phase 8.4 will replace this stub.
        Ok(Vec::new())
    }
}
