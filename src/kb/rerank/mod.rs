//! Reranker trait + factory — Phase 8.
//!
//! The Retriever orchestrator (Phase 7.2) accepts an optional
//! `Arc<dyn Reranker>` so the wiring is in place — callers without a
//! configured reranker pass `None` and the rerank stage is skipped.
//!
//! Three concrete implementations live in submodules:
//! - [`llm`] — OpenRouter chat completions as a JSON-array-of-indices reranker.
//! - [`cohere`] — managed Cohere `v2/rerank` API.
//! - [`vllm`] — self-hosted Cohere-shape `/rerank` sidecar.

use async_trait::async_trait;

use crate::kb::{KbConfig, KbError, KbResult};

pub mod cohere;
pub mod llm;
pub mod vllm;

pub use cohere::CohereReranker;
pub use llm::LlmReranker;
pub use vllm::VllmReranker;

/// One candidate fed into the reranker. `original_rank` + `original_score`
/// let LlmReranker fill remaining slots from the upstream fused order when
/// the model returns fewer indices than `final_k`.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub text: String,
    pub original_rank: usize,
    pub original_score: f32,
}

/// One reranked output slot. `final_rank` is the 0-based position the
/// reranker chose; `score` is provider-specific (LLM = inverted rank,
/// Cohere/vLLM = native relevance score).
#[derive(Debug, Clone)]
pub struct Reranked {
    pub id: String,
    pub final_rank: usize,
    pub score: f32,
}

/// Reranker contract — fed N candidates, returns up to `final_k` reordered
/// by the provider's relevance signal.
#[async_trait]
pub trait Reranker: Send + Sync {
    /// Display name used in tracing + error fallback messages.
    fn name(&self) -> &str;

    /// Rerank candidates against `query`. Implementations should never panic;
    /// on provider failure return `Err` and let the caller fall back to the
    /// upstream fused order.
    async fn rerank(
        &self,
        query: &str,
        candidates: &[Candidate],
        final_k: usize,
    ) -> KbResult<Vec<Reranked>>;
}

/// Build the configured reranker, or return `None` when rerank is disabled
/// or the selected provider failed to initialize.
///
/// Provider selection mirrors `getDefaultReranker` in `rerankers/index.ts`:
/// - `"vllm"` → [`VllmReranker`] against a self-hosted `/rerank` sidecar
///   (reads `KB_RERANK_BASE_URL`, default `http://localhost:8200`).
/// - `"cohere"` → [`CohereReranker`] against the managed `v2/rerank` API
///   (reads `KB_RERANK_API_KEY`, falling back to `COHERE_API_KEY`).
/// - anything else (including empty / unset) → [`LlmReranker`] over
///   OpenRouter chat completions.
pub fn make_reranker(cfg: &KbConfig) -> Option<Box<dyn Reranker>> {
    if !cfg.rerank_enabled {
        return None;
    }
    match cfg.rerank_provider.to_lowercase().as_str() {
        "vllm" => match VllmReranker::from_env(cfg) {
            Ok(r) => Some(Box::new(r)),
            Err(e) => {
                tracing::warn!(
                    target: "kb::rerank",
                    error = %e,
                    "VllmReranker init failed, skipping rerank stage",
                );
                None
            }
        },
        "cohere" => Some(Box::new(CohereReranker::from_env(cfg))),
        _ => Some(Box::new(LlmReranker::new(
            cfg.rerank_model.clone(),
            cfg.openrouter_chat_url.clone(),
        ))),
    }
}

/// Walk `picked` indices into `candidates`, dedupe by candidate id, then
/// fill remaining slots from `candidates` in original-rank order until
/// `final_k` results are produced (or candidates run out).
///
/// `score_fn(rank, candidate)` mints the per-slot score so each provider can
/// stamp its own signal (LLM = `final_k - rank`, Cohere/vLLM =
/// `relevance_score` for picked slots and `0.0` for filler).
///
/// Extracted once the third caller (`vllm`) lined up the exact same tail
/// (rule-of-three); keeps each provider's `rerank` body focused on its
/// transport + score-source.
pub(crate) fn fill_remaining_in_order<F>(
    candidates: &[Candidate],
    picked: &[(usize, f32)],
    final_k: usize,
    filler_score: F,
) -> Vec<Reranked>
where
    F: Fn(usize, &Candidate) -> f32,
{
    let mut out: Vec<Reranked> = Vec::with_capacity(final_k.min(candidates.len()));
    let mut picked_ids = std::collections::HashSet::<String>::new();

    for &(idx, score) in picked {
        if out.len() >= final_k {
            break;
        }
        let Some(cand) = candidates.get(idx) else {
            continue;
        };
        if !picked_ids.insert(cand.id.clone()) {
            continue;
        }
        let rank = out.len();
        out.push(Reranked {
            id: cand.id.clone(),
            final_rank: rank,
            score,
        });
    }

    for cand in candidates {
        if out.len() >= final_k {
            break;
        }
        if !picked_ids.insert(cand.id.clone()) {
            continue;
        }
        let rank = out.len();
        let score = filler_score(rank, cand);
        out.push(Reranked {
            id: cand.id.clone(),
            final_rank: rank,
            score,
        });
    }

    out
}

/// Shared POST-and-parse transport for the rerank HTTP backends (Cohere,
/// vLLM, LLM-as-judge). Builds the request, sends it, maps a non-2xx status
/// to `KbError::ChatApi`, then deserializes the body into the caller's own
/// response type — the three backends' response *shapes* genuinely differ,
/// so only the transport is shared here; each backend keeps its own body
/// construction and response-schema mapping.
///
/// `api_key = None` omits the `Authorization` header (used by `VllmReranker`,
/// which talks to an unauthenticated in-cluster sidecar).
///
/// Extracted once the third backend (`vllm`) lined up the exact same
/// post/status-check/parse shape as `cohere` and `llm` (rule-of-three). A
/// fourth rerank backend should call this from day one rather than
/// re-inlining `post().json().send()` + status-check + `resp.json()`.
pub(in crate::kb::rerank) async fn post_json_rerank<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
    body: &serde_json::Value,
) -> KbResult<T> {
    let mut req = http.post(endpoint).json(body);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(KbError::ChatApi {
            status: status.as_u16(),
            body: truncate(&text, 300),
        });
    }

    Ok(resp.json::<T>().await?)
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

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::post_json_rerank;
    use crate::kb::KbError;

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestBody {
        ok: bool,
    }

    #[tokio::test]
    async fn post_json_rerank_parses_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let body = serde_json::json!({ "query": "q" });
        let parsed: TestBody = post_json_rerank(&http, &server.uri(), None, &body)
            .await
            .expect("2xx response parses into caller's schema");
        assert_eq!(parsed, TestBody { ok: true });
    }

    #[tokio::test]
    async fn post_json_rerank_maps_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(1)
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let body = serde_json::json!({ "query": "q" });
        let err = post_json_rerank::<TestBody>(&http, &server.uri(), None, &body)
            .await
            .expect_err("non-2xx must surface as ChatApi");
        match err {
            KbError::ChatApi { status, body } => {
                assert_eq!(status, 500);
                assert!(body.contains("boom"), "body = {body}");
            }
            other => panic!("expected KbError::ChatApi(500), got {other:?}"),
        }
    }
}
