//! Tests for the KB reranker layer (`src/kb/rerank/`).
//!
//! Task 8.2 covers LlmReranker (OpenRouter chat-as-reranker).
//! Tasks 8.3 / 8.4 will append CohereReranker / VllmReranker tests.
//!
//! Tests that mutate `OPENROUTER_API_KEY` serialize on `ENV_LOCK` from
//! `tests/kb/common.rs` — see the rationale in `embed_test.rs`.

#![allow(clippy::await_holding_lock)]

use rantaiclaw::kb::rerank::{Candidate, CohereReranker, LlmReranker, Reranker, VllmReranker};
use rantaiclaw::kb::KbError;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::kb::common::ENV_LOCK;

#[allow(dead_code)]
struct EnvGuard(Vec<&'static str>);
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for k in &self.0 {
            // SAFETY: serialized via ENV_LOCK in the test that owns the guard.
            unsafe {
                std::env::remove_var(k);
            }
        }
    }
}

fn cands(n: usize) -> Vec<Candidate> {
    (0..n)
        .map(|i| Candidate {
            id: format!("c{i}"),
            text: format!("passage {i}"),
            original_rank: i,
            original_score: 1.0 - (i as f32) * 0.1,
        })
        .collect()
}

/// Build a mock-server chat completions response whose assistant message
/// content is `raw` verbatim. Mirrors OpenRouter's response envelope.
fn chat_body(raw: &str) -> serde_json::Value {
    json!({
        "choices": [{
            "message": { "role": "assistant", "content": raw }
        }]
    })
}

#[tokio::test]
async fn llm_rerank_returns_all_when_fewer_candidates_than_final_k() {
    // No HTTP path hit — short-circuits before fetch. We still need the env
    // key to be present, since the key check happens first.
    let _lock = ENV_LOCK.lock().unwrap();
    // SAFETY: serialized via ENV_LOCK and removed by EnvGuard on drop.
    unsafe {
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
    }
    let _guard = EnvGuard(vec!["OPENROUTER_API_KEY"]);

    let r = LlmReranker::new("test-model".into(), "http://127.0.0.1:1/never".into());
    let c = cands(3);
    let out = r.rerank("q", &c, 5).await.expect("short-circuit ok");
    assert_eq!(out.len(), 3, "fewer candidates than final_k returns all");
    assert_eq!(out[0].id, "c0");
    assert_eq!(out[1].id, "c1");
    assert_eq!(out[2].id, "c2");
    // Score = final_k - rank.
    assert!((out[0].score - 5.0).abs() < 1e-6);
    assert!((out[1].score - 4.0).abs() < 1e-6);
    assert!((out[2].score - 3.0).abs() < 1e-6);
}

#[tokio::test]
async fn llm_rerank_parses_index_array() {
    let _lock = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
    }
    let _guard = EnvGuard(vec!["OPENROUTER_API_KEY"]);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_body("[2, 0, 1]")))
        .expect(1)
        .mount(&server)
        .await;

    let r = LlmReranker::new("test-model".into(), server.uri());
    let c = cands(3);
    let out = r.rerank("q", &c, 3).await.expect("rerank ok");
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].id, "c2", "model put c2 first");
    assert_eq!(out[1].id, "c0");
    assert_eq!(out[2].id, "c1");
    // Score sequence: final_k - rank = 3, 2, 1.
    assert!((out[0].score - 3.0).abs() < 1e-6);
    assert!((out[1].score - 2.0).abs() < 1e-6);
    assert!((out[2].score - 1.0).abs() < 1e-6);
}

#[tokio::test]
async fn llm_rerank_handles_malformed_response() {
    let _lock = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
    }
    let _guard = EnvGuard(vec!["OPENROUTER_API_KEY"]);

    let server = MockServer::start().await;
    // No JSON array in the content at all — picked list ends up empty.
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(chat_body("sorry, can't comply")),
        )
        .mount(&server)
        .await;

    let r = LlmReranker::new("test-model".into(), server.uri());
    let c = cands(5);
    let out = r.rerank("q", &c, 3).await.expect("fallback ok");
    // Falls back to original order, returns final_k items.
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].id, "c0");
    assert_eq!(out[1].id, "c1");
    assert_eq!(out[2].id, "c2");
}

#[tokio::test]
async fn llm_rerank_skips_out_of_range_indices() {
    let _lock = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
    }
    let _guard = EnvGuard(vec!["OPENROUTER_API_KEY"]);

    let server = MockServer::start().await;
    // Index 10 doesn't exist — should be dropped silently; 0 + 1 used,
    // then fill from original order (c2, c3...).
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_body("[10, 0, 1]")))
        .mount(&server)
        .await;

    let r = LlmReranker::new("test-model".into(), server.uri());
    let c = cands(5);
    let out = r.rerank("q", &c, 4).await.expect("rerank ok");
    assert_eq!(out.len(), 4);
    assert_eq!(out[0].id, "c0", "10 dropped, 0 first");
    assert_eq!(out[1].id, "c1");
    assert_eq!(out[2].id, "c2", "fill in original rank order");
    assert_eq!(out[3].id, "c3");
}

#[tokio::test]
async fn llm_rerank_returns_error_on_missing_api_key() {
    let _lock = ENV_LOCK.lock().unwrap();
    // SAFETY: serialized via ENV_LOCK.
    unsafe {
        std::env::remove_var("OPENROUTER_API_KEY");
    }

    let r = LlmReranker::new("test-model".into(), "http://127.0.0.1:1/unused".into());
    let err = r
        .rerank("q", &cands(5), 3)
        .await
        .expect_err("missing key must fail-fast");
    match err {
        KbError::Config(msg) => {
            assert!(msg.contains("OPENROUTER_API_KEY"), "msg = {msg}");
        }
        other => panic!("expected KbError::Config, got {other:?}"),
    }
}

#[tokio::test]
async fn llm_rerank_returns_error_on_4xx() {
    let _lock = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("OPENROUTER_API_KEY", "test-key");
    }
    let _guard = EnvGuard(vec!["OPENROUTER_API_KEY"]);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .expect(1)
        .mount(&server)
        .await;

    let r = LlmReranker::new("test-model".into(), server.uri());
    let err = r
        .rerank("q", &cands(5), 3)
        .await
        .expect_err("401 must surface as ChatApi");
    match err {
        KbError::ChatApi { status, body } => {
            assert_eq!(status, 401);
            assert!(body.contains("unauthorized"), "body = {body}");
        }
        other => panic!("expected KbError::ChatApi(401), got {other:?}"),
    }
}

// ---- CohereReranker (task 8.3) --------------------------------------

/// Cohere mock-response envelope.
fn cohere_body(results: &[(usize, f32)]) -> serde_json::Value {
    let items: Vec<_> = results
        .iter()
        .map(|(i, s)| json!({ "index": i, "relevance_score": s }))
        .collect();
    json!({ "results": items })
}

#[tokio::test]
async fn cohere_rerank_short_circuits_when_few_candidates() {
    // No HTTP, no env — apiKey present is enough to pass the fail-fast.
    let r = CohereReranker::new(
        "rerank-v4.0-pro".into(),
        "test-key".into(),
        Some("http://127.0.0.1:1/never".into()),
    );
    let c = cands(2);
    let out = r.rerank("q", &c, 5).await.expect("short-circuit ok");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].id, "c0");
    assert_eq!(out[1].id, "c1");
    assert!((out[0].score - 5.0).abs() < 1e-6);
    assert!((out[1].score - 4.0).abs() < 1e-6);
}

#[tokio::test]
async fn cohere_rerank_uses_relevance_score() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(cohere_body(&[(2, 0.9), (0, 0.8)])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let r = CohereReranker::new(
        "rerank-v4.0-pro".into(),
        "test-key".into(),
        Some(server.uri()),
    );
    let c = cands(5);
    let out = r.rerank("q", &c, 3).await.expect("rerank ok");
    assert_eq!(out.len(), 3);
    // Picked slots use the provider's relevance score directly.
    assert_eq!(out[0].id, "c2");
    assert!((out[0].score - 0.9).abs() < 1e-6);
    assert_eq!(out[1].id, "c0");
    assert!((out[1].score - 0.8).abs() < 1e-6);
    // Filler slot picks next-in-original-order (c1, not c3/c4) with score 0.
    assert_eq!(out[2].id, "c1");
    assert!(out[2].score.abs() < 1e-6, "filler score must be 0.0");
}

#[tokio::test]
async fn cohere_rerank_errors_on_no_api_key() {
    let r = CohereReranker::new(
        "rerank-v4.0-pro".into(),
        String::new(),
        Some("http://127.0.0.1:1/unused".into()),
    );
    let err = r
        .rerank("q", &cands(5), 3)
        .await
        .expect_err("missing key must fail-fast");
    match err {
        KbError::Config(msg) => assert!(msg.contains("apiKey"), "msg = {msg}"),
        other => panic!("expected KbError::Config, got {other:?}"),
    }
}

#[tokio::test]
async fn cohere_rerank_errors_on_4xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .expect(1)
        .mount(&server)
        .await;

    let r = CohereReranker::new(
        "rerank-v4.0-pro".into(),
        "test-key".into(),
        Some(server.uri()),
    );
    let err = r
        .rerank("q", &cands(5), 3)
        .await
        .expect_err("403 must surface as ChatApi");
    match err {
        KbError::ChatApi { status, body } => {
            assert_eq!(status, 403);
            assert!(body.contains("forbidden"), "body = {body}");
        }
        other => panic!("expected KbError::ChatApi(403), got {other:?}"),
    }
}

// ---- VllmReranker (task 8.4) ----------------------------------------

/// vLLM mock-response envelope (same shape as Cohere).
fn vllm_body(results: &[(usize, f32)]) -> serde_json::Value {
    let items: Vec<_> = results
        .iter()
        .map(|(i, s)| json!({ "index": i, "relevance_score": s }))
        .collect();
    json!({ "results": items })
}

#[tokio::test]
async fn vllm_rerank_short_circuits_when_few_candidates() {
    // Use a non-routable URL — short-circuit must not touch the network.
    let r = VllmReranker::new(
        "http://127.0.0.1:1".into(),
        "nemotron-test".into(),
    )
    .expect("non-empty base_url");
    let c = cands(2);
    let out = r.rerank("q", &c, 5).await.expect("short-circuit ok");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].id, "c0");
    assert_eq!(out[1].id, "c1");
    assert!((out[0].score - 5.0).abs() < 1e-6);
    assert!((out[1].score - 4.0).abs() < 1e-6);
}

#[tokio::test]
async fn vllm_rerank_parses_cohere_shape_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rerank"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(vllm_body(&[(3, 0.95), (1, 0.42)])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let r = VllmReranker::new(server.uri(), "nemotron-test".into())
        .expect("non-empty base_url");
    let c = cands(5);
    let out = r.rerank("q", &c, 3).await.expect("rerank ok");
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].id, "c3");
    assert!((out[0].score - 0.95).abs() < 1e-6);
    assert_eq!(out[1].id, "c1");
    assert!((out[1].score - 0.42).abs() < 1e-6);
    // Filler picks next-in-original-order (c0 before c2/c4) with score 0.
    assert_eq!(out[2].id, "c0");
    assert!(out[2].score.abs() < 1e-6);
}

#[test]
fn vllm_rerank_errors_on_empty_base_url() {
    // VllmReranker isn't Debug (holds a reqwest::Client), so use match
    // instead of expect_err.
    match VllmReranker::new(String::new(), "nemotron-test".into()) {
        Ok(_) => panic!("empty base_url must fail at construction"),
        Err(KbError::Config(msg)) => assert!(msg.contains("base_url"), "msg = {msg}"),
        Err(other) => panic!("expected KbError::Config, got {other:?}"),
    }
}

#[tokio::test]
async fn vllm_rerank_errors_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rerank"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream busy"))
        .expect(1)
        .mount(&server)
        .await;

    let r = VllmReranker::new(server.uri(), "nemotron-test".into())
        .expect("non-empty base_url");
    let err = r
        .rerank("q", &cands(5), 3)
        .await
        .expect_err("503 must surface as ChatApi");
    match err {
        KbError::ChatApi { status, body } => {
            assert_eq!(status, 503);
            assert!(body.contains("upstream busy"), "body = {body}");
        }
        other => panic!("expected KbError::ChatApi(503), got {other:?}"),
    }
}
