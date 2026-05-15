//! LLM-as-reranker — stub. Full impl + tests land in Task 8.2.

use async_trait::async_trait;

use crate::kb::rerank::{Candidate, Reranked, Reranker};
use crate::kb::KbResult;

/// LLM-as-reranker provider. `chat_url` defaults to the OpenRouter chat
/// completions endpoint via `KbConfig::openrouter_chat_url`.
pub struct LlmReranker {
    model: String,
    #[allow(dead_code)] // populated by factory; consumed in Task 8.2
    chat_url: String,
}

impl LlmReranker {
    pub fn new(model: String, chat_url: String) -> Self {
        Self { model, chat_url }
    }
}

#[async_trait]
impl Reranker for LlmReranker {
    fn name(&self) -> &str {
        &self.model
    }

    async fn rerank(
        &self,
        _query: &str,
        _candidates: &[Candidate],
        _final_k: usize,
    ) -> KbResult<Vec<Reranked>> {
        // Phase 8.2 will replace this stub.
        Ok(Vec::new())
    }
}
