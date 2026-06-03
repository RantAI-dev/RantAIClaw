//! TEI sidecar embedding provider.
//!
//! TEI = huggingface/text-embeddings-inference. Speaks the same OpenAI-shaped
//! `/embeddings` request body as OpenRouter, so most of the implementation is
//! shared via [`super::openrouter::embed_via_http`] / `embed_many_via_http`.
//! The only behavioral difference: when no API key is configured (neither
//! `KB_EMBEDDING_API_KEY` nor `OPENROUTER_API_KEY`), we send the request
//! *without* an `Authorization` header. TEI sidecars typically run on a
//! private network unauthenticated; sending a bogus `Bearer ` header would
//! cause some TEI builds to 401.

use async_trait::async_trait;
use reqwest::Client;

use crate::kb::embed::openrouter::{embed_many_via_http, embed_via_http};
use crate::kb::embed::{EmbeddingProvider, SharedEmbedCache};
use crate::kb::{KbConfig, KbError, KbResult};

/// Sidecar-hosted embedding provider (TEI / on-prem OpenAI-shaped endpoint).
pub struct TeiEmbedding {
    cfg: KbConfig,
    cache: SharedEmbedCache,
    http: Client,
}

impl TeiEmbedding {
    pub fn new(cfg: KbConfig, cache: SharedEmbedCache) -> Self {
        Self {
            cfg,
            cache,
            http: Client::new(),
        }
    }

    /// Resolve the auth header value. Empty → `None` (omit header).
    fn auth(&self) -> Option<String> {
        let key = KbConfig::resolve_key(&self.cfg.embedding_api_key);
        if key.is_empty() {
            None
        } else {
            Some(key)
        }
    }
}

#[async_trait]
impl EmbeddingProvider for TeiEmbedding {
    fn model(&self) -> &str {
        &self.cfg.embedding_model
    }

    fn dim(&self) -> usize {
        self.cfg.embedding_dim
    }

    async fn embed_query(&self, text: &str) -> KbResult<Vec<f32>> {
        let cache_key = format!("{}|{}", self.cfg.embedding_model, text.trim());
        if let Some(hit) = self.cache.lock().await.get(&cache_key) {
            return Ok(hit);
        }

        let body = serde_json::json!({
            "model": &self.cfg.embedding_model,
            "input": text,
        });
        let auth = self.auth();
        let vectors = embed_via_http(
            &self.http,
            &self.cfg.embedding_base_url,
            auth.as_deref(),
            &body,
            self.cfg.embedding_dim,
        )
        .await?;
        let vector = vectors
            .into_iter()
            .next()
            .ok_or_else(|| KbError::Other("Empty embedding response from API".into()))?;
        self.cache.lock().await.put(cache_key, vector.clone());
        Ok(vector)
    }

    async fn embed_many(&self, texts: &[String]) -> KbResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let auth = self.auth();
        embed_many_via_http(
            &self.http,
            &self.cfg.embedding_base_url,
            auth.as_deref(),
            &self.cfg.embedding_model,
            texts,
            self.cfg.embed_batch_size,
            self.cfg.embed_concurrency,
            self.cfg.embedding_dim,
        )
        .await
    }
}
