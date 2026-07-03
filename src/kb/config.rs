use crate::kb::{KbError, KbResult};
use std::env;

// Field shape mirrors TS `RagConfig` 1:1 for env-var parity. The four
// `*_enabled` bools are independent feature toggles, not a state machine,
// so the clippy::pedantic suggestion to collapse them does not apply here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct KbConfig {
    pub extract_primary: String,
    pub extract_fallback: String,
    pub extract_smart_fallback: String,
    pub embedding_model: String,
    pub embedding_dim: usize,
    pub default_max_chunks: usize,
    pub rerank_enabled: bool,
    pub rerank_provider: String,
    pub rerank_model: String,
    pub rerank_initial_k: usize,
    pub rerank_final_k: usize,
    pub hybrid_bm25_enabled: bool,
    pub contextual_retrieval_enabled: bool,
    pub contextual_retrieval_model: String,
    pub query_expansion_enabled: bool,
    pub query_expansion_model: String,
    pub query_expansion_paraphrases: usize,
    pub standalone_query_enabled: bool,
    pub extract_vision_base_url: String,
    pub extract_vision_api_key: String,
    pub extract_mineru_base_url: String,
    pub embedding_base_url: String,
    pub embedding_api_key: String,
    pub embed_batch_size: usize,
    pub embed_concurrency: usize,
    pub query_embed_cache_size: usize,
    pub query_embed_cache_ttl_ms: u64,
    /// OpenRouter chat-completions endpoint shared by query_expansion,
    /// contextual retrieval, and standalone_query rewriter. Reads
    /// `KB_OPENROUTER_CHAT_URL` from env; tests + on-prem deployments
    /// override this. Mirrors the same env-override pattern as
    /// `KB_EMBEDDING_BASE_URL`.
    pub openrouter_chat_url: String,
    /// Enable document intelligence (graph extraction, entity linking).
    /// Reads `KB_INTELLIGENCE_ENABLED`; off by default.
    pub intelligence_enabled: bool,
    /// Model used by the intelligence extractor (entity/relation extraction).
    /// Reads `KB_INTELLIGENCE_MODEL`; defaults to `openai/gpt-4.1-nano`.
    pub intelligence_model: String,
    /// Resolution mode for intelligence extraction (`exact` or `fuzzy`).
    /// Reads `KB_INTELLIGENCE_RESOLUTION`; defaults to `exact`.
    pub intelligence_resolution: String,
    /// Maximum nodes retained in the in-memory knowledge graph.
    /// Reads `KB_GRAPH_MAX_NODES`; defaults to `200`.
    pub graph_max_nodes: usize,
    /// Enable GraphRAG: augment retrieval with chunks reached via the entity
    /// graph (query → matched entities → 1-hop neighbours → their chunks),
    /// merged into the existing RRF fusion. Reads `KB_GRAPHRAG_ENABLED`; off by
    /// default. Requires the intelligence graph to be populated to have effect.
    pub graphrag_enabled: bool,
    /// Cap on 1-hop neighbour entities expanded per query during GraphRAG.
    /// Reads `KB_GRAPHRAG_MAX_NEIGHBORS`; defaults to `20`.
    pub graphrag_max_neighbors: usize,
}

impl KbConfig {
    pub fn from_env() -> KbResult<Self> {
        Ok(Self {
            extract_primary: env::var("KB_EXTRACT_PRIMARY").unwrap_or_else(|_| "smart".into()),
            extract_fallback: env::var("KB_EXTRACT_FALLBACK").unwrap_or_else(|_| "unpdf".into()),
            extract_smart_fallback: env::var("KB_EXTRACT_SMART_FALLBACK")
                .unwrap_or_else(|_| "openai/gpt-4.1-nano".into()),
            embedding_model: env::var("KB_EMBEDDING_MODEL")
                .unwrap_or_else(|_| "qwen/qwen3-embedding-8b".into()),
            embedding_dim: parse_env::<usize>("KB_EMBEDDING_DIM", 4096)?,
            default_max_chunks: parse_env::<usize>("KB_DEFAULT_MAX_CHUNKS", 8)?,
            rerank_enabled: env::var("KB_RERANK_ENABLED").as_deref() == Ok("true"),
            rerank_provider: env::var("KB_RERANK_PROVIDER").unwrap_or_default(),
            rerank_model: env::var("KB_RERANK_MODEL")
                .unwrap_or_else(|_| "openai/gpt-4.1-nano".into()),
            rerank_initial_k: parse_env::<usize>("KB_RERANK_INITIAL_K", 20)?,
            rerank_final_k: parse_env::<usize>("KB_RERANK_FINAL_K", 5)?,
            hybrid_bm25_enabled: env::var("KB_HYBRID_BM25_ENABLED").as_deref() != Ok("false"),
            contextual_retrieval_enabled: env::var("KB_CONTEXTUAL_RETRIEVAL_ENABLED").as_deref()
                == Ok("true"),
            contextual_retrieval_model: env::var("KB_CONTEXTUAL_RETRIEVAL_MODEL")
                .unwrap_or_else(|_| "openai/gpt-4.1-nano".into()),
            query_expansion_enabled: env::var("KB_QUERY_EXPANSION_ENABLED").as_deref()
                == Ok("true"),
            query_expansion_model: env::var("KB_QUERY_EXPANSION_MODEL")
                .unwrap_or_else(|_| "openai/gpt-4.1-nano".into()),
            query_expansion_paraphrases: parse_env::<usize>("KB_QUERY_EXPANSION_PARAPHRASES", 3)?,
            standalone_query_enabled: env::var("KB_STANDALONE_QUERY_ENABLED").as_deref()
                == Ok("true"),
            extract_vision_base_url: env::var("KB_EXTRACT_VISION_BASE_URL")
                .unwrap_or_else(|_| "https://openrouter.ai/api/v1/chat/completions".into()),
            extract_vision_api_key: env::var("KB_EXTRACT_VISION_API_KEY").unwrap_or_default(),
            extract_mineru_base_url: env::var("KB_EXTRACT_MINERU_BASE_URL").unwrap_or_default(),
            embedding_base_url: env::var("KB_EMBEDDING_BASE_URL")
                .unwrap_or_else(|_| "https://openrouter.ai/api/v1/embeddings".into()),
            embedding_api_key: env::var("KB_EMBEDDING_API_KEY").unwrap_or_default(),
            embed_batch_size: parse_env::<usize>("KB_EMBED_BATCH_SIZE", 128)?,
            embed_concurrency: parse_env::<usize>("KB_EMBED_CONCURRENCY", 4)?,
            query_embed_cache_size: parse_env::<usize>("KB_QUERY_EMBED_CACHE_SIZE", 256)?,
            query_embed_cache_ttl_ms: parse_env::<u64>(
                "KB_QUERY_EMBED_CACHE_TTL_MS",
                5 * 60 * 1_000,
            )?,
            openrouter_chat_url: env::var("KB_OPENROUTER_CHAT_URL")
                .unwrap_or_else(|_| "https://openrouter.ai/api/v1/chat/completions".into()),
            intelligence_enabled: env::var("KB_INTELLIGENCE_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            intelligence_model: env::var("KB_INTELLIGENCE_MODEL")
                .unwrap_or_else(|_| "openai/gpt-4.1-nano".into()),
            intelligence_resolution: env::var("KB_INTELLIGENCE_RESOLUTION")
                .unwrap_or_else(|_| "exact".into()),
            graph_max_nodes: parse_env("KB_GRAPH_MAX_NODES", 200)?,
            graphrag_enabled: env::var("KB_GRAPHRAG_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            graphrag_max_neighbors: parse_env::<usize>("KB_GRAPHRAG_MAX_NEIGHBORS", 20)?,
        })
    }

    /// Like [`from_env`], but the two credential fields are taken from the
    /// caller (the resolved `config.knowledge` values) when non-empty, so KB
    /// keys flow from config like `api_key`. All non-key fields still come from
    /// env. Empty/None leaves whatever `from_env` read, and the `resolve_key`
    /// chain still applies its final `OPENROUTER_API_KEY` fallback downstream.
    pub fn from_env_with_keys(embedding: Option<&str>, vision: Option<&str>) -> KbResult<Self> {
        let mut cfg = Self::from_env()?;
        if let Some(k) = embedding.filter(|s| !s.is_empty()) {
            cfg.embedding_api_key = k.to_string();
        }
        if let Some(k) = vision.filter(|s| !s.is_empty()) {
            cfg.extract_vision_api_key = k.to_string();
        }
        Ok(cfg)
    }

    /// Resolve an endpoint API key — falls back to `OPENROUTER_API_KEY` when the
    /// per-endpoint override is empty. Mirrors `resolveApiKey` in TS config.ts.
    pub fn resolve_key(override_key: &str) -> String {
        if !override_key.is_empty() {
            return override_key.into();
        }
        env::var("OPENROUTER_API_KEY").unwrap_or_default()
    }

    /// Like [`resolve_key`] but tries a `secondary` credential (e.g. the
    /// embedding key) before the `OPENROUTER_API_KEY` env var. KB usually
    /// shares one provider, so OCR can reuse the embedding key instead of
    /// failing when only `KB_EMBEDDING_API_KEY` is configured.
    pub fn resolve_key_with_fallback(primary: &str, secondary: &str) -> String {
        if !primary.is_empty() {
            return primary.into();
        }
        Self::resolve_key(secondary)
    }
}

/// Parse an env var as `T` (any `FromStr`), returning `fallback` when the
/// var is unset or empty. Using a generic over `T` instead of always
/// returning `usize` avoids a silent truncation on 32-bit targets when a
/// caller widens to `u64` via `as` (rantaiclaw supports RPi peripherals, so
/// 32-bit is reachable). Parse failure surfaces as `KbError::Config`.
fn parse_env<T>(key: &str, fallback: T) -> KbResult<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(key) {
        Ok(raw) if !raw.is_empty() => raw.parse::<T>().map_err(|e| {
            KbError::Config(format!(
                "{key} must be parseable as {}, got {raw:?}: {e}",
                std::any::type_name::<T>()
            ))
        }),
        _ => Ok(fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_with_keys_overrides_key_fields_only() {
        std::env::remove_var("KB_EMBEDDING_API_KEY");
        std::env::remove_var("KB_EXTRACT_VISION_API_KEY");
        let cfg = KbConfig::from_env_with_keys(Some("cfg-embed"), None).unwrap();
        assert_eq!(cfg.embedding_api_key, "cfg-embed");
        assert_eq!(cfg.extract_vision_api_key, ""); // None + no env → empty (resolver fallback applies later)
        assert_eq!(cfg.embedding_model, "qwen/qwen3-embedding-8b"); // non-key field still from env default
    }
}
