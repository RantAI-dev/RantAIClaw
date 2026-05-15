//! OpenRouter embedding provider — Rust port of `generateEmbedding` /
//! `generateEmbeddings` from `src/lib/rag/embeddings.ts`.
//!
//! Behavior is intentionally identical to the TS source:
//! - Single-query path caches by `{model}|{trimmed_text}` (so swapping models
//!   never returns a stale vector of the wrong space).
//! - Batch path bypasses the cache (ingest chunks are write-once).
//! - Retries up to 3x on HTTP 5xx / 429 with exponential backoff capped at 10s.
//! - Dimension mismatch on any returned vector is a hard error.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use tokio::time::{sleep, Duration};

use crate::kb::embed::{EmbeddingProvider, SharedEmbedCache};
use crate::kb::{KbConfig, KbError, KbResult};

const MAX_RETRIES: u32 = 3;
const RETRY_BASE_MS: u64 = 1_000;
const RETRY_MAX_MS: u64 = 10_000;

/// Cloud-hosted embedding provider (OpenRouter API).
pub struct OpenRouterEmbedding {
    cfg: KbConfig,
    cache: SharedEmbedCache,
    http: Client,
}

impl OpenRouterEmbedding {
    pub fn new(cfg: KbConfig, cache: SharedEmbedCache) -> Self {
        Self {
            cfg,
            cache,
            http: Client::new(),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for OpenRouterEmbedding {
    fn model(&self) -> &str {
        &self.cfg.embedding_model
    }

    fn dim(&self) -> usize {
        self.cfg.embedding_dim
    }

    async fn embed_query(&self, text: &str) -> KbResult<Vec<f32>> {
        let api_key = KbConfig::resolve_key(&self.cfg.embedding_api_key);
        if api_key.is_empty() {
            return Err(KbError::Config(
                "No API key configured: set KB_EMBEDDING_API_KEY or OPENROUTER_API_KEY".into(),
            ));
        }

        // Cache key includes the model so swapping `KB_EMBEDDING_MODEL` never
        // returns a stale vector of the wrong dimension/space.
        let cache_key = format!("{}|{}", self.cfg.embedding_model, text.trim());
        if let Some(hit) = self.cache.lock().await.get(&cache_key) {
            return Ok(hit);
        }

        let body = serde_json::json!({
            "model": &self.cfg.embedding_model,
            "input": text,
        });
        let vectors = embed_via_http(
            &self.http,
            &self.cfg.embedding_base_url,
            Some(&api_key),
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

        let api_key = KbConfig::resolve_key(&self.cfg.embedding_api_key);
        if api_key.is_empty() {
            return Err(KbError::Config(
                "No API key configured: set KB_EMBEDDING_API_KEY or OPENROUTER_API_KEY".into(),
            ));
        }

        embed_many_via_http(
            &self.http,
            &self.cfg.embedding_base_url,
            Some(&api_key),
            &self.cfg.embedding_model,
            texts,
            self.cfg.embed_batch_size,
            self.cfg.embed_concurrency,
            self.cfg.embedding_dim,
        )
        .await
    }
}

/// Shared POST-and-parse helper. Returns the embedding vectors in the order
/// the server returned them; the caller decides whether to validate length
/// against the batch input size.
///
/// `api_key = None` omits the `Authorization` header (TEI sidecar mode).
pub(in crate::kb::embed) async fn embed_via_http(
    http: &Client,
    url: &str,
    api_key: Option<&str>,
    body: &Value,
    expected_dim: usize,
) -> KbResult<Vec<Vec<f32>>> {
    let mut last_err: Option<KbError> = None;
    for attempt in 1..=MAX_RETRIES {
        let mut req = http.post(url).json(body);
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        let send_result = req.send().await;
        let resp = match send_result {
            Ok(r) => r,
            Err(e) => {
                // Network/transport errors mirror TS catch — retry up to MAX.
                last_err = Some(KbError::Http(e));
                if attempt < MAX_RETRIES {
                    sleep(backoff_for(attempt)).await;
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            if is_transient(status.as_u16()) && attempt < MAX_RETRIES {
                tracing::warn!(
                    target: "kb::embed",
                    attempt,
                    max = MAX_RETRIES,
                    status = status.as_u16(),
                    "embedding attempt failed, retrying",
                );
                sleep(backoff_for(attempt)).await;
                continue;
            }
            return Err(KbError::EmbeddingApi {
                status: status.as_u16(),
                body: body_text,
            });
        }

        let parsed: Value = resp.json().await?;
        let arr = parsed
            .get("data")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                KbError::Other(format!(
                    "Invalid embedding response structure: {}",
                    truncate(&parsed.to_string(), 500)
                ))
            })?;
        let mut out = Vec::with_capacity(arr.len());
        for (i, item) in arr.iter().enumerate() {
            let emb = item
                .get("embedding")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    KbError::Other(format!(
                        "Invalid embedding item at index {i}: {}",
                        truncate(&item.to_string(), 200)
                    ))
                })?;
            // f64 → f32 truncation is intentional: embeddings are stored as
            // f32 throughout the KB (sqlite-vec column type, search-time
            // similarity math). NaN-fallback flags malformed numeric values
            // so downstream length/dim checks still catch them.
            #[allow(clippy::cast_possible_truncation)]
            let vec: Vec<f32> = emb
                .iter()
                .map(|n| n.as_f64().map(|f| f as f32).unwrap_or(f32::NAN))
                .collect();
            if vec.len() != expected_dim {
                return Err(KbError::DimensionMismatch {
                    expected: expected_dim,
                    got: vec.len(),
                    index: i,
                });
            }
            out.push(vec);
        }
        return Ok(out);
    }
    Err(last_err.unwrap_or_else(|| KbError::Other("Embedding generation failed".into())))
}

/// Batch dispatcher: chunk inputs, spawn `concurrency` workers that each pull
/// the next batch index from a shared atomic counter, preserve input order.
#[allow(clippy::too_many_arguments)]
pub(in crate::kb::embed) async fn embed_many_via_http(
    http: &Client,
    url: &str,
    api_key: Option<&str>,
    model: &str,
    texts: &[String],
    batch_size: usize,
    concurrency: usize,
    expected_dim: usize,
) -> KbResult<Vec<Vec<f32>>> {
    // Split into batches.
    let batches: Vec<&[String]> = texts.chunks(batch_size.max(1)).collect();
    let n_batches = batches.len();

    // Per-batch result slots — `tokio::sync::Mutex` because workers run on
    // separate Tokio tasks; std Mutex would deadlock on the runtime when
    // an `.await` is held across the lock guard.
    type BatchSlots = Vec<Option<Vec<Vec<f32>>>>;
    let results: Arc<tokio::sync::Mutex<BatchSlots>> =
        Arc::new(tokio::sync::Mutex::new(vec![None; n_batches]));
    let next_idx = Arc::new(AtomicUsize::new(0));

    let mut workers = Vec::new();
    let worker_count = concurrency.max(1).min(n_batches);
    for _ in 0..worker_count {
        let http = http.clone();
        let url = url.to_string();
        let api_key = api_key.map(str::to_string);
        let model = model.to_string();
        let batches_owned: Vec<Vec<String>> = batches.iter().map(|b| b.to_vec()).collect();
        let next = next_idx.clone();
        let results = results.clone();
        workers.push(tokio::spawn(async move {
            loop {
                let idx = next.fetch_add(1, Ordering::SeqCst);
                if idx >= batches_owned.len() {
                    return Ok::<(), KbError>(());
                }
                let batch = &batches_owned[idx];
                let body = serde_json::json!({ "model": model, "input": batch });
                let vectors =
                    embed_via_http(&http, &url, api_key.as_deref(), &body, expected_dim).await?;
                // Length mismatch (server returned different number of vectors
                // than we asked for) is fatal — the order/dim contract breaks.
                if vectors.len() != batch.len() {
                    return Err(KbError::LengthMismatch {
                        kind: "embedding batch",
                        left: vectors.len(),
                        right: batch.len(),
                    });
                }
                results.lock().await[idx] = Some(vectors);
            }
        }));
    }

    for w in workers {
        match w.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(join_err) => {
                return Err(KbError::Other(format!("embed worker panicked: {join_err}")));
            }
        }
    }

    // Flatten in input order.
    let mut guard = results.lock().await;
    let mut flat: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for slot in guard.iter_mut() {
        let batch = slot
            .take()
            .ok_or_else(|| KbError::Other("missing batch result".into()))?;
        flat.extend(batch);
    }
    Ok(flat)
}

fn is_transient(status: u16) -> bool {
    status >= 500 || status == 429
}

fn backoff_for(attempt: u32) -> Duration {
    // 1000 * 2^(attempt-1), capped at 10s. Mirrors `embeddings.ts:105`.
    let ms = RETRY_BASE_MS
        .saturating_mul(1u64 << (attempt - 1))
        .min(RETRY_MAX_MS);
    Duration::from_millis(ms)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}
