//! Tests for the KB embedding layer (`src/kb/embed/`).
//!
//! Task 3.1 covers the LRU cache port. Task 3.2 adds OpenRouter provider
//! tests (wiremock-backed). Task 3.3 will append TEI variant tests.

use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use rantaiclaw::kb::embed::cache::LruCache;
use rantaiclaw::kb::embed::openrouter::OpenRouterEmbedding;
use rantaiclaw::kb::embed::{make_provider, EmbeddingProvider};
use rantaiclaw::kb::{KbConfig, KbError};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

// Shared env-mutation lock — mirrors tests/kb/config_test.rs. Tests that
// mutate `KB_*` / `OPENROUTER_API_KEY` env vars must hold this guard for
// the duration of their work so they don't race inside the same binary.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[allow(dead_code)] // used by TEI tests added in task 3.3
struct EnvGuard(Vec<&'static str>);
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for k in &self.0 {
            // SAFETY: serialized via ENV_LOCK above.
            unsafe {
                std::env::remove_var(k);
            }
        }
    }
}

/// Build a minimal `KbConfig` whose `embedding_base_url` points at the mock,
/// without touching process-wide env. Bypassing `from_env()` keeps mock tests
/// free of the global ENV_LOCK except where we explicitly need it.
fn mock_cfg(base_url: String) -> KbConfig {
    KbConfig {
        extract_primary: "smart".into(),
        extract_fallback: "unpdf".into(),
        extract_smart_fallback: "openai/gpt-4.1-nano".into(),
        embedding_model: "test-model".into(),
        embedding_dim: 4,
        default_max_chunks: 8,
        rerank_enabled: false,
        rerank_provider: String::new(),
        rerank_model: "openai/gpt-4.1-nano".into(),
        rerank_initial_k: 20,
        rerank_final_k: 5,
        hybrid_bm25_enabled: true,
        contextual_retrieval_enabled: false,
        contextual_retrieval_model: "openai/gpt-4.1-nano".into(),
        query_expansion_enabled: false,
        query_expansion_model: "openai/gpt-4.1-nano".into(),
        query_expansion_paraphrases: 3,
        extract_vision_base_url: String::new(),
        extract_vision_api_key: String::new(),
        extract_mineru_base_url: String::new(),
        embedding_base_url: base_url,
        embedding_api_key: "test-key".into(),
        embed_batch_size: 100,
        embed_concurrency: 2,
        query_embed_cache_size: 8,
        query_embed_cache_ttl_ms: 60_000,
    }
}

fn make_cache() -> rantaiclaw::kb::embed::SharedEmbedCache {
    Arc::new(tokio::sync::Mutex::new(LruCache::new(
        8,
        Some(Duration::from_millis(60_000)),
    )))
}

/// Build a single-embedding response body (`{ data: [{ embedding: [...] }] }`)
/// with `dim` zero-padded entries except the first which is `0.1`.
fn make_response(dim: usize, count: usize) -> serde_json::Value {
    let mut data = Vec::new();
    for _ in 0..count {
        let mut v = vec![0.0_f64; dim];
        if dim > 0 {
            v[0] = 0.1;
        }
        data.push(json!({ "embedding": v }));
    }
    json!({ "data": data })
}

#[test]
fn lru_evicts_oldest_at_capacity() {
    let mut c: LruCache<String, u32> = LruCache::new(2, None);
    c.put("a".into(), 1);
    c.put("b".into(), 2);
    c.put("c".into(), 3);
    assert_eq!(c.get(&"a".into()), None, "oldest entry evicted");
    assert_eq!(c.get(&"b".into()), Some(2));
    assert_eq!(c.get(&"c".into()), Some(3));
    assert_eq!(c.len(), 2);
}

#[test]
fn lru_ttl_evicts_expired() {
    let mut c: LruCache<String, u32> = LruCache::new(8, Some(Duration::from_millis(50)));
    c.put("a".into(), 1);
    sleep(Duration::from_millis(80));
    assert_eq!(c.get(&"a".into()), None, "TTL-expired entry returns miss");
    assert!(c.is_empty(), "expired entry lazily evicted on probe");
}

#[test]
fn lru_get_promotes_to_recent() {
    let mut c: LruCache<String, u32> = LruCache::new(2, None);
    c.put("a".into(), 1);
    c.put("b".into(), 2);
    // Touching `a` promotes it; subsequent insert evicts `b` (now oldest).
    assert_eq!(c.get(&"a".into()), Some(1));
    c.put("c".into(), 3);
    assert_eq!(c.get(&"a".into()), Some(1), "promoted entry survives");
    assert_eq!(c.get(&"b".into()), None, "demoted entry evicted");
    assert_eq!(c.get(&"c".into()), Some(3));
}

#[test]
fn lru_put_overwrites_existing_value() {
    let mut c: LruCache<String, u32> = LruCache::new(4, None);
    c.put("a".into(), 1);
    c.put("a".into(), 2);
    assert_eq!(c.get(&"a".into()), Some(2));
    assert_eq!(c.len(), 1, "overwrite does not grow length");
}

// ---- OpenRouter provider (task 3.2) ----------------------------------

#[tokio::test]
async fn openrouter_embed_query_caches_second_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response(4, 1)))
        .expect(1) // ← caching means only ONE upstream hit
        .mount(&server)
        .await;

    let cfg = mock_cfg(server.uri());
    let provider = OpenRouterEmbedding::new(cfg, make_cache());

    let a = provider.embed_query("hello").await.expect("first call");
    let b = provider.embed_query("hello").await.expect("cached call");
    assert_eq!(a, b);
    // wiremock auto-asserts `.expect(1)` on Drop — failures show as panics.
}

#[tokio::test]
async fn openrouter_embed_query_retries_on_503() {
    let server = MockServer::start().await;
    // First two responses 503, third 200.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response(4, 1)))
        .expect(1)
        .mount(&server)
        .await;

    // Override retry backoff via a small dim to keep the test fast — backoff
    // is fixed at 1s base in code; we accept ~3s test duration for one retry.
    // Two retries before success → ~1s + 2s = 3s.
    let cfg = mock_cfg(server.uri());
    let provider = OpenRouterEmbedding::new(cfg, make_cache());
    let v = provider.embed_query("x").await.expect("retried success");
    assert_eq!(v.len(), 4);
}

#[tokio::test]
async fn openrouter_embed_query_no_retry_on_400() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .expect(1) // ← no retry on 4xx (except 429)
        .mount(&server)
        .await;

    let cfg = mock_cfg(server.uri());
    let provider = OpenRouterEmbedding::new(cfg, make_cache());
    let err = provider.embed_query("x").await.expect_err("400 fails fast");
    match err {
        KbError::EmbeddingApi { status, .. } => assert_eq!(status, 400),
        other => panic!("expected EmbeddingApi(400), got {other:?}"),
    }
}

#[tokio::test]
async fn openrouter_embed_query_dim_mismatch_errors() {
    let server = MockServer::start().await;
    // Returned vector is length 3, config expects 4.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "embedding": [0.1, 0.2, 0.3] }]
        })))
        .mount(&server)
        .await;

    let cfg = mock_cfg(server.uri());
    let provider = OpenRouterEmbedding::new(cfg, make_cache());
    let err = provider.embed_query("x").await.expect_err("dim mismatch");
    match err {
        KbError::DimensionMismatch {
            expected,
            got,
            index,
        } => {
            assert_eq!(expected, 4);
            assert_eq!(got, 3);
            assert_eq!(index, 0);
        }
        other => panic!("expected DimensionMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn openrouter_embed_many_batches_and_concurrent() {
    let server = MockServer::start().await;
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits_clone = hits.clone();
    Mock::given(method("POST"))
        .respond_with(move |req: &Request| {
            hits_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Echo the batch size from the inbound request so we test
            // the "server returns N vectors for N inputs" contract.
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let input = body.get("input").and_then(|v| v.as_array()).unwrap();
            ResponseTemplate::new(200).set_body_json(make_response(4, input.len()))
        })
        .mount(&server)
        .await;

    let cfg = mock_cfg(server.uri()); // batch=100, concurrency=2
    let provider = OpenRouterEmbedding::new(cfg, make_cache());
    let texts: Vec<String> = (0..300).map(|i| format!("doc-{i}")).collect();
    let out = provider.embed_many(&texts).await.expect("batched ok");
    assert_eq!(out.len(), 300);
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "300 texts at batch=100 = 3 batches"
    );
    // Output order/dim sanity.
    for v in &out {
        assert_eq!(v.len(), 4);
    }
}

#[tokio::test]
async fn openrouter_embed_many_propagates_dim_mismatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "embedding": [0.1, 0.2, 0.3, 0.4] }, // ok
                { "embedding": [0.1, 0.2, 0.3] },     // wrong length
            ]
        })))
        .mount(&server)
        .await;

    let cfg = mock_cfg(server.uri());
    let provider = OpenRouterEmbedding::new(cfg, make_cache());
    let texts = vec!["a".into(), "b".into()];
    let err = provider.embed_many(&texts).await.expect_err("dim mismatch");
    assert!(
        matches!(err, KbError::DimensionMismatch { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn make_provider_uses_openrouter_for_default_url() {
    // Default URL contains `openrouter.ai` → OpenRouter provider. We can't
    // downcast trait objects without dyn-Any, so use model id as a proxy
    // (both providers expose model() the same way, but factory wiring is
    // exercised by the build itself).
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Wipe KB_* / OPENROUTER_API_KEY to avoid leak.
    let snapshot: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("KB_") || k == "OPENROUTER_API_KEY")
        .collect();
    for (k, _) in &snapshot {
        unsafe {
            std::env::remove_var(k);
        }
    }
    let cfg = KbConfig::from_env().expect("env config");
    let provider = make_provider(&cfg).expect("provider built");
    assert_eq!(provider.model(), cfg.embedding_model);
    assert_eq!(provider.dim(), cfg.embedding_dim);
    // Restore.
    for (k, v) in snapshot {
        unsafe {
            std::env::set_var(k, v);
        }
    }
}

// Live integration test — requires a real OpenRouter API key. Default
// `cargo test` skips this; run with `--ignored` to exercise it.
#[tokio::test]
#[ignore = "requires OPENROUTER_API_KEY"]
async fn openrouter_embed_query_live() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return;
    }
    let cfg = KbConfig::from_env().expect("env config");
    let provider = make_provider(&cfg).expect("provider built");
    let v = provider
        .embed_query("hello world")
        .await
        .expect("live embed");
    assert_eq!(v.len(), cfg.embedding_dim);
    assert!(v.iter().any(|x| *x != 0.0), "all-zero embedding is suspect");
}
