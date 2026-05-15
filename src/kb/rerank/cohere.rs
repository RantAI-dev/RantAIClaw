//! Cohere v2/rerank provider — stub. Full impl + tests land in Task 8.3.

use async_trait::async_trait;

use crate::kb::rerank::{Candidate, Reranked, Reranker};
use crate::kb::{KbConfig, KbResult};

const DEFAULT_ENDPOINT: &str = "https://api.cohere.com/v2/rerank";

/// Cohere rerank provider.
pub struct CohereReranker {
    model: String,
    #[allow(dead_code)] // consumed in Task 8.3
    api_key: String,
    #[allow(dead_code)] // consumed in Task 8.3
    endpoint: String,
}

impl CohereReranker {
    pub fn new(model: String, api_key: String, endpoint: Option<String>) -> Self {
        Self {
            model,
            api_key,
            endpoint: endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.into()),
        }
    }

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
        _query: &str,
        _candidates: &[Candidate],
        _final_k: usize,
    ) -> KbResult<Vec<Reranked>> {
        // Phase 8.3 will replace this stub.
        Ok(Vec::new())
    }
}
