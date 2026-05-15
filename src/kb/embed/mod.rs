//! KB embedding layer — provider trait, LRU cache, and concrete providers.
//!
//! Today only [`openrouter::OpenRouterEmbedding`] is wired through
//! [`make_provider`]; the TEI sidecar variant lands in task 3.3. The trait
//! mirrors what the TS RAG pipeline calls (`generateEmbedding` /
//! `generateEmbeddings` in `src/lib/rag/embeddings.ts`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::kb::embed::cache::LruCache;
use crate::kb::{KbConfig, KbResult};

pub mod cache;
pub mod openrouter;

/// Single-query embedding cache, shared across one provider instance.
pub type SharedEmbedCache = Arc<Mutex<LruCache<String, Vec<f32>>>>;

/// Embedding backend abstraction. Trait is intentionally narrow: providers
/// just need to expose model id, dimension, and the two TS entry points.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// The configured model id (e.g. `qwen/qwen3-embedding-8b`).
    fn model(&self) -> &str;
    /// Expected output vector length. Hard contract — providers MUST error
    /// if a returned vector differs (see [`crate::kb::KbError::DimensionMismatch`]).
    fn dim(&self) -> usize;
    /// Embed a single text. Cached via the shared LRU.
    async fn embed_query(&self, text: &str) -> KbResult<Vec<f32>>;
    /// Embed many texts. Bypasses the cache (ingest chunks are write-once)
    /// and parallelizes with `cfg.embed_concurrency` workers. Output order
    /// matches input order.
    async fn embed_many(&self, texts: &[String]) -> KbResult<Vec<Vec<f32>>>;
}

/// Build a provider. Only OpenRouter today (task 3.3 will add TEI dispatch).
pub fn make_provider(cfg: &KbConfig) -> KbResult<Arc<dyn EmbeddingProvider>> {
    let cache: SharedEmbedCache = Arc::new(Mutex::new(LruCache::new(
        cfg.query_embed_cache_size,
        Some(Duration::from_millis(cfg.query_embed_cache_ttl_ms)),
    )));
    Ok(Arc::new(openrouter::OpenRouterEmbedding::new(
        cfg.clone(),
        cache,
    )))
}
