use async_trait::async_trait;

/// Trait for embedding providers — convert text to vectors
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Provider name
    fn name(&self) -> &str;

    /// Embedding dimensions
    fn dimensions(&self) -> usize;

    /// Embed a batch of texts into vectors
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>>;

    /// Embed a single text
    async fn embed_one(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut results = self.embed(&[text]).await?;
        results
            .pop()
            .ok_or_else(|| anyhow::anyhow!("Empty embedding result"))
    }
}

// ── Noop provider (keyword-only fallback) ────────────────────

pub struct NoopEmbedding;

#[async_trait]
impl EmbeddingProvider for NoopEmbedding {
    fn name(&self) -> &str {
        "none"
    }

    fn dimensions(&self) -> usize {
        0
    }

    async fn embed(&self, _texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(Vec::new())
    }
}

// ── OpenAI-compatible embedding provider ─────────────────────

pub struct OpenAiEmbedding {
    base_url: String,
    api_key: String,
    model: String,
    dims: usize,
}

impl OpenAiEmbedding {
    pub fn new(base_url: &str, api_key: &str, model: &str, dims: usize) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            dims,
        }
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client("memory.embeddings")
    }

    fn has_explicit_api_path(&self) -> bool {
        let Ok(url) = reqwest::Url::parse(&self.base_url) else {
            return false;
        };

        let path = url.path().trim_end_matches('/');
        !path.is_empty() && path != "/"
    }

    fn has_embeddings_endpoint(&self) -> bool {
        let Ok(url) = reqwest::Url::parse(&self.base_url) else {
            return false;
        };

        url.path().trim_end_matches('/').ends_with("/embeddings")
    }

    fn embeddings_url(&self) -> String {
        if self.has_embeddings_endpoint() {
            return self.base_url.clone();
        }

        if self.has_explicit_api_path() {
            format!("{}/embeddings", self.base_url)
        } else {
            format!("{}/v1/embeddings", self.base_url)
        }
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbedding {
    fn name(&self) -> &str {
        "openai"
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let body = serde_json::json!({
            "model": self.model,
            "input": texts,
        });

        let resp = self
            .http_client()
            .post(self.embeddings_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Embedding API error {status}: {text}");
        }

        let json: serde_json::Value = resp.json().await?;
        let data = json
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| anyhow::anyhow!("Invalid embedding response: missing 'data'"))?;

        let mut embeddings = Vec::with_capacity(data.len());
        for item in data {
            let embedding = item
                .get("embedding")
                .and_then(|e| e.as_array())
                .ok_or_else(|| anyhow::anyhow!("Invalid embedding item"))?;

            #[allow(clippy::cast_possible_truncation)]
            let vec: Vec<f32> = embedding
                .iter()
                .filter_map(|v| v.as_f64().map(|f| f as f32))
                .collect();

            embeddings.push(vec);
        }

        Ok(embeddings)
    }
}

// ── MiniMax native embedding provider ────────────────────────

pub struct MiniMaxEmbedding {
    base_url: String,
    api_key: String,
    group_id: String,
    model: String,
    dims: usize,
}

impl MiniMaxEmbedding {
    pub fn new(base_url: &str, api_key: &str, group_id: &str, model: &str, dims: usize) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            group_id: group_id.to_string(),
            model: model.to_string(),
            dims,
        }
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client("memory.embeddings")
    }

    fn embeddings_url(&self) -> String {
        format!("{}/embeddings", self.base_url)
    }
}

/// Build the MiniMax native embeddings request body.
///
/// Note: MiniMax uses its own format (`texts` + `type`), not OpenAI's (`input`).
fn build_request_body(model: &str, texts: &[&str], embed_type: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "texts": texts,
        "type": embed_type,
    })
}

/// Parse a MiniMax embeddings response body.
///
/// Returns the `vectors` array, or an error carrying the `base_resp` status
/// code + message when `status_code != 0`.
fn parse_embedding_response(raw: &str) -> anyhow::Result<Vec<Vec<f32>>> {
    let json: serde_json::Value = serde_json::from_str(raw)?;

    if let Some(base_resp) = json.get("base_resp") {
        let status_code = base_resp
            .get("status_code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        if status_code != 0 {
            let status_msg = base_resp
                .get("status_msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("MiniMax embeddings error {status_code}: {status_msg}");
        }
    }

    let vectors = json
        .get("vectors")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("Invalid embedding response: missing 'vectors'"))?;

    let mut embeddings = Vec::with_capacity(vectors.len());
    for item in vectors {
        let vector = item
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Invalid embedding vector"))?;

        #[allow(clippy::cast_possible_truncation)]
        let vec: Vec<f32> = vector
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();

        embeddings.push(vec);
    }

    Ok(embeddings)
}

#[async_trait]
impl EmbeddingProvider for MiniMaxEmbedding {
    fn name(&self) -> &str {
        "minimax"
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let body = build_request_body(&self.model, texts, "db");

        let mut request = self
            .http_client()
            .post(self.embeddings_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json");

        if !self.group_id.is_empty() {
            request = request.query(&[("GroupId", &self.group_id)]);
        }

        let resp = request.json(&body).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Embedding API error {status}: {text}");
        }

        let raw = resp.text().await?;
        parse_embedding_response(&raw)
    }
}

// ── Factory ──────────────────────────────────────────────────

pub fn create_embedding_provider(
    provider: &str,
    api_key: Option<&str>,
    model: &str,
    dims: usize,
) -> Box<dyn EmbeddingProvider> {
    match provider {
        "openai" => {
            let key = api_key.unwrap_or("");
            Box::new(OpenAiEmbedding::new(
                "https://api.openai.com",
                key,
                model,
                dims,
            ))
        }
        "openrouter" => {
            let key = api_key.unwrap_or("");
            Box::new(OpenAiEmbedding::new(
                "https://openrouter.ai/api/v1",
                key,
                model,
                dims,
            ))
        }
        "minimax" => {
            let key = api_key.unwrap_or("");
            let group_id = std::env::var("MINIMAX_GROUP_ID").unwrap_or_default();
            if group_id.is_empty() {
                tracing::warn!(
                    "MiniMax embeddings usually require MINIMAX_GROUP_ID; \
                     proceeding without a GroupId query param"
                );
            }
            let base_url = std::env::var("MINIMAX_EMBED_BASE_URL")
                .unwrap_or_else(|_| "https://api.minimax.io/v1".to_string());
            Box::new(MiniMaxEmbedding::new(
                &base_url, key, &group_id, model, dims,
            ))
        }
        name if name.starts_with("custom:") => {
            let base_url = name.strip_prefix("custom:").unwrap_or("");
            let key = api_key.unwrap_or("");
            Box::new(OpenAiEmbedding::new(base_url, key, model, dims))
        }
        _ => Box::new(NoopEmbedding),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_name() {
        let p = NoopEmbedding;
        assert_eq!(p.name(), "none");
        assert_eq!(p.dimensions(), 0);
    }

    #[tokio::test]
    async fn noop_embed_returns_empty() {
        let p = NoopEmbedding;
        let result = p.embed(&["hello"]).await.unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn factory_none() {
        let p = create_embedding_provider("none", None, "model", 1536);
        assert_eq!(p.name(), "none");
    }

    #[test]
    fn factory_openai() {
        let p = create_embedding_provider("openai", Some("key"), "text-embedding-3-small", 1536);
        assert_eq!(p.name(), "openai");
        assert_eq!(p.dimensions(), 1536);
    }

    #[test]
    fn factory_openrouter() {
        let p = create_embedding_provider(
            "openrouter",
            Some("sk-or-test"),
            "openai/text-embedding-3-small",
            1536,
        );
        assert_eq!(p.name(), "openai"); // uses OpenAiEmbedding internally
        assert_eq!(p.dimensions(), 1536);
    }

    #[test]
    fn factory_custom_url() {
        let p = create_embedding_provider("custom:http://localhost:1234", None, "model", 768);
        assert_eq!(p.name(), "openai"); // uses OpenAiEmbedding internally
        assert_eq!(p.dimensions(), 768);
    }

    // ── Edge cases ───────────────────────────────────────────────

    #[tokio::test]
    async fn noop_embed_one_returns_error() {
        let p = NoopEmbedding;
        // embed returns empty vec → pop() returns None → error
        let result = p.embed_one("hello").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn noop_embed_empty_batch() {
        let p = NoopEmbedding;
        let result = p.embed(&[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn noop_embed_multiple_texts() {
        let p = NoopEmbedding;
        let result = p.embed(&["a", "b", "c"]).await.unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn factory_empty_string_returns_noop() {
        let p = create_embedding_provider("", None, "model", 1536);
        assert_eq!(p.name(), "none");
    }

    #[test]
    fn factory_unknown_provider_returns_noop() {
        let p = create_embedding_provider("cohere", None, "model", 1536);
        assert_eq!(p.name(), "none");
    }

    #[test]
    fn factory_custom_empty_url() {
        // "custom:" with no URL — should still construct without panic
        let p = create_embedding_provider("custom:", None, "model", 768);
        assert_eq!(p.name(), "openai");
    }

    #[test]
    fn factory_openai_no_api_key() {
        let p = create_embedding_provider("openai", None, "text-embedding-3-small", 1536);
        assert_eq!(p.name(), "openai");
        assert_eq!(p.dimensions(), 1536);
    }

    #[test]
    fn openai_trailing_slash_stripped() {
        let p = OpenAiEmbedding::new("https://api.openai.com/", "key", "model", 1536);
        assert_eq!(p.base_url, "https://api.openai.com");
    }

    #[test]
    fn openai_dimensions_custom() {
        let p = OpenAiEmbedding::new("http://localhost", "k", "m", 384);
        assert_eq!(p.dimensions(), 384);
    }

    #[test]
    fn embeddings_url_openrouter() {
        let p = OpenAiEmbedding::new(
            "https://openrouter.ai/api/v1",
            "key",
            "openai/text-embedding-3-small",
            1536,
        );
        assert_eq!(
            p.embeddings_url(),
            "https://openrouter.ai/api/v1/embeddings"
        );
    }

    #[test]
    fn embeddings_url_standard_openai() {
        let p = OpenAiEmbedding::new("https://api.openai.com", "key", "model", 1536);
        assert_eq!(p.embeddings_url(), "https://api.openai.com/v1/embeddings");
    }

    #[test]
    fn embeddings_url_base_with_v1_no_duplicate() {
        let p = OpenAiEmbedding::new("https://api.example.com/v1", "key", "model", 1536);
        assert_eq!(p.embeddings_url(), "https://api.example.com/v1/embeddings");
    }

    #[test]
    fn embeddings_url_non_v1_api_path_uses_raw_suffix() {
        let p = OpenAiEmbedding::new(
            "https://api.example.com/api/coding/v3",
            "key",
            "model",
            1536,
        );
        assert_eq!(
            p.embeddings_url(),
            "https://api.example.com/api/coding/v3/embeddings"
        );
    }

    // ── MiniMax ──────────────────────────────────────────────────

    #[test]
    fn factory_minimax_name_and_dims() {
        let p = create_embedding_provider("minimax", Some("key"), "embo-01", 1536);
        assert_eq!(p.name(), "minimax");
        assert_eq!(p.dimensions(), 1536);
    }

    #[test]
    fn minimax_build_request_body_native_format() {
        let body = build_request_body("embo-01", &["a", "b"], "db");
        assert_eq!(
            body,
            serde_json::json!({
                "model": "embo-01",
                "texts": ["a", "b"],
                "type": "db",
            })
        );
    }

    #[test]
    fn minimax_parse_embedding_response_success() {
        let raw = r#"{"vectors":[[0.1,0.2],[0.3,0.4]],"base_resp":{"status_code":0,"status_msg":"success"}}"#;
        let vectors = parse_embedding_response(raw).unwrap();
        assert_eq!(vectors, vec![vec![0.1_f32, 0.2], vec![0.3, 0.4]]);
    }

    #[test]
    fn minimax_parse_embedding_response_error_carries_code_and_msg() {
        let raw = r#"{"base_resp":{"status_code":1004,"status_msg":"invalid api key"}}"#;
        let err = parse_embedding_response(raw).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("1004"), "missing code: {msg}");
        assert!(msg.contains("invalid api key"), "missing status_msg: {msg}");
    }

    #[test]
    fn minimax_embeddings_url_appends_endpoint() {
        let p = MiniMaxEmbedding::new("https://api.minimax.io/v1", "key", "", "embo-01", 1536);
        assert_eq!(p.embeddings_url(), "https://api.minimax.io/v1/embeddings");
    }

    #[tokio::test]
    async fn minimax_embed_empty_batch() {
        let p = MiniMaxEmbedding::new("https://api.minimax.io/v1", "key", "", "embo-01", 1536);
        let result = p.embed(&[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn embeddings_url_custom_full_endpoint() {
        let p = OpenAiEmbedding::new(
            "https://my-api.example.com/api/v2/embeddings",
            "key",
            "model",
            1536,
        );
        assert_eq!(
            p.embeddings_url(),
            "https://my-api.example.com/api/v2/embeddings"
        );
    }
}
