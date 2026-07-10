//! HTTP integration tests for the `/api/v1/kb/*` surface (Phase 11).
//!
//! Each test:
//! 1. Stands up a fresh SQLite KB under a `TempDir` and points `KB_DB_PATH`
//!    at it via [`ENV_LOCK`] (the lock serializes tests that mutate
//!    process-wide env vars + the shared SQLite file).
//! 2. Builds a minimal [`AppState`] (mock provider/memory, no pairing) and
//!    binds an axum server on `127.0.0.1:0` so each test owns a unique
//!    port.
//! 3. Issues HTTP calls via `reqwest` and asserts on response shape.
//!
//! The handlers that go through the embedding provider (`POST /search`,
//! `POST /documents`, `POST /re-embed`) are not exercised over the live
//! network here — re-embed with `dry_run=true` exercises the SQL path
//! without hitting the embedder when the store has no chunks. Search/ingest
//! against a real embedder live in `cli_test.rs` behind `#[ignore]`.
//!
//! Tests serialize on `super::common::ENV_LOCK` and intentionally hold the
//! guard across `.await` to keep `KB_DB_PATH`/env mutation single-threaded
//! — see the rationale in `embed_test.rs` / `rerank_test.rs`.

#![allow(clippy::await_holding_lock)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::Value;
use tempfile::TempDir;

use super::common::ENV_LOCK;
use rantaiclaw::config::Config;
use rantaiclaw::gateway::{AppState, GatewayRateLimiter, IdempotencyStore};
use rantaiclaw::kb::axi::api;
use rantaiclaw::kb::store::sqlite::SqliteStore;
use rantaiclaw::kb::store::KbStore;
use rantaiclaw::kb::{Document, DocumentId};
use rantaiclaw::memory::{Memory, MemoryCategory, MemoryEntry};
use rantaiclaw::observability::NoopObserver;
use rantaiclaw::providers::Provider;
use rantaiclaw::security::pairing::PairingGuard;

// ────────────────────────────────────────────────────────────────────────────
// Mocks — minimal Provider + Memory impls so we can construct AppState.
// ────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct MockProvider;

#[async_trait]
impl Provider for MockProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("mock".into())
    }
}

#[derive(Default)]
struct MockMemory;

#[async_trait]
impl Memory for MockMemory {
    fn name(&self) -> &str {
        "mock"
    }
    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        Ok(None)
    }
    async fn list(
        &self,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        Ok(Vec::new())
    }
    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }
    async fn health_check(&self) -> bool {
        true
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Harness — boots the KB router on a random port, returns the base URL.
// ────────────────────────────────────────────────────────────────────────────

/// Hold-onto-me-or-I-leak handle. The `_lock_guard` holds the test
/// serializer for the harness's life so other tests wait until the current
/// one's I/O is complete. The server task is leaked intentionally; tokio
/// reaps it at runtime teardown.
struct Harness {
    base_url: String,
    _tmp: TempDir,
    // Hold the global env-mutation lock for the harness's life so other
    // tests wait until our HTTP I/O finishes. Std `MutexGuard` is `!Send`
    // but `#[tokio::test]` uses a `current_thread` runtime, so holding it
    // across awaits is safe (the future never migrates threads).
    _lock_guard: std::sync::MutexGuard<'static, ()>,
}

fn build_state(require_pairing: bool, tokens: &[String]) -> AppState {
    AppState {
        config: Arc::new(Mutex::new(Config::default())),
        provider: Arc::new(MockProvider),
        model: "test-model".into(),
        temperature: 0.0,
        mem: Arc::new(MockMemory),
        auto_save: false,
        tools_registry: Arc::new(Vec::new()),
        webhook_secret_hash: None,
        pairing: Arc::new(PairingGuard::new(require_pairing, tokens)),
        channel_approvals: Arc::new(
            rantaiclaw::gateway::channel_approval::ChannelApprovalStore::default(),
        ),
        web_approvals: Arc::new(rantaiclaw::security::PendingApprovals::default()),
        trust_forwarded_headers: false,
        rate_limiter: Arc::new(GatewayRateLimiter::new(100, 100, 100)),
        idempotency_store: Arc::new(IdempotencyStore::new(Duration::from_secs(300), 1000)),
        whatsapp: None,
        whatsapp_app_secret: None,
        linq: None,
        linq_signing_secret: None,
        nextcloud_talk: None,
        nextcloud_talk_webhook_secret: None,
        observer: Arc::new(NoopObserver),
        webhook_routes: Arc::new(Vec::new()),
    }
}

/// Stand up the KB router under a fresh per-test sqlite file. `seed_docs`
/// runs after the store is opened so callers can pre-populate documents.
///
/// Each test owns its own `TempDir` + `KB_DB_PATH`. `api.rs::ensure_kb_ctx`
/// keys its cached `KbContext` on the resolved DB path, so a path change
/// triggers a rebuild — the runtime store handle never carries state from
/// the previous test. We still serialize on [`ENV_LOCK`] because the
/// `KB_DB_PATH` env var is process-wide and parallel mutation would race.
async fn start_harness<F>(seed_docs: F) -> Harness
where
    F: FnOnce(SqliteStore) -> futures::future::BoxFuture<'static, ()>,
{
    start_harness_with_auth(false, &[], seed_docs).await
}

async fn start_harness_with_auth<F>(
    require_pairing: bool,
    tokens: &[String],
    seed_docs: F,
) -> Harness
where
    F: FnOnce(SqliteStore) -> futures::future::BoxFuture<'static, ()>,
{
    // Serialize against every other test that mutates KB_* / OPENROUTER_*
    // env vars. We use the shared `super::common::ENV_LOCK` so config_test,
    // embed_test, retrieve_test, and api_test all queue against the same
    // lock — independent mutexes would race on `KB_DB_PATH` when tests
    // run in parallel threads.
    let lock_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("kb.db");

    std::env::set_var("KB_DB_PATH", &db);
    std::env::set_var("KB_EMBEDDING_DIM", "4");
    // A NON-EMPTY embedding key is required: `build_ctx` fails fast with
    // `kb_not_configured` (→ HTTP 503) when no key resolves, and that key
    // otherwise comes from the ambient `OPENROUTER_API_KEY` — which other kb
    // tests (rerank/retrieve) set to "test-key" and don't always restore, so
    // leaving it empty here made the harness's 200-vs-503 outcome depend on
    // test order. The value is never sent anywhere: these handlers exercise
    // list/get/delete paths that don't call the embedder.
    std::env::set_var("KB_EMBEDDING_API_KEY", "test-embedding-key");
    std::env::remove_var("OPENROUTER_API_KEY");

    // Drop any KB context a prior test cached (it is keyed on the db path, but
    // a cached `Err` from an earlier no-key run must not leak into this one).
    api::clear_kb_ctx().await;

    // Pre-create the store so the schema matches the env dim before any
    // handler opens its own connection.
    let store = SqliteStore::open(&db, 4).await.expect("open sqlite store");
    seed_docs(store).await;

    let state = build_state(require_pairing, tokens);
    let app: Router = Router::new().merge(api::router()).with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");
    let base_url = format!("http://{addr}");

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Harness {
        base_url,
        _tmp: tmp,
        _lock_guard: lock_guard,
    }
}

fn sample_doc(id: &str, title: &str) -> Document {
    Document {
        id: DocumentId(id.into()),
        title: title.into(),
        content: format!("body of {title}"),
        categories: vec!["FAQ".into()],
        subcategory: None,
        metadata: serde_json::json!({}),
        s3_key: None,
        file_type: None,
        mime_type: None,
        file_size: None,
        organization_id: Some("rantaiclaw_org_a".into()),
        created_by: None,
        session_id: None,
        artifact_type: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        deleted_at: None,
        retention_days: None,
        retrieval_count: 0,
        last_retrieved_at: None,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn kb_list_empty_returns_empty_array() {
    let h = start_harness(|_store| Box::pin(async move {})).await;

    let resp = reqwest::get(format!("{}/api/v1/kb/documents", h.base_url))
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json body");
    assert!(body.is_array(), "expected array, got: {body}");
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn kb_list_after_seed_returns_documents() {
    let h = start_harness(|store| {
        Box::pin(async move {
            store
                .create_document(&sample_doc("rantaiclaw_doc_a", "Seeded Doc"))
                .await
                .expect("seed");
        })
    })
    .await;

    let resp = reqwest::get(format!("{}/api/v1/kb/documents", h.base_url))
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json body");
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "Seeded Doc");
    assert_eq!(arr[0]["id"], "rantaiclaw_doc_a");
}

#[tokio::test]
async fn kb_get_nonexistent_returns_404() {
    let h = start_harness(|_store| Box::pin(async move {})).await;

    let resp = reqwest::get(format!(
        "{}/api/v1/kb/documents/rantaiclaw_missing",
        h.base_url
    ))
    .await
    .expect("request");
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["error"], "not_found");
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|s| s.contains("rantaiclaw_missing")),
        "detail should mention id: {body}"
    );
}

#[tokio::test]
async fn kb_get_existing_returns_document() {
    let h = start_harness(|store| {
        Box::pin(async move {
            store
                .create_document(&sample_doc("rantaiclaw_doc_get", "Gettable"))
                .await
                .expect("seed");
        })
    })
    .await;

    let resp = reqwest::get(format!(
        "{}/api/v1/kb/documents/rantaiclaw_doc_get",
        h.base_url
    ))
    .await
    .expect("request");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["title"], "Gettable");
}

#[tokio::test]
async fn kb_delete_soft_hides_doc_from_list_and_get() {
    let h = start_harness(|store| {
        Box::pin(async move {
            store
                .create_document(&sample_doc("rantaiclaw_doc_del", "Deletable"))
                .await
                .expect("seed");
        })
    })
    .await;

    let client = reqwest::Client::new();

    // Delete (defaults to soft).
    let del = client
        .delete(format!(
            "{}/api/v1/kb/documents/rantaiclaw_doc_del",
            h.base_url
        ))
        .send()
        .await
        .expect("delete");
    assert_eq!(del.status(), 200);
    let del_body: Value = del.json().await.expect("delete body");
    assert_eq!(del_body["mode"], "soft");
    assert_eq!(del_body["id"], "rantaiclaw_doc_del");

    // GET on the same id must now 404 (soft-delete hides from getters).
    let after = reqwest::get(format!(
        "{}/api/v1/kb/documents/rantaiclaw_doc_del",
        h.base_url
    ))
    .await
    .expect("get after delete");
    assert_eq!(after.status(), 404);

    // List must also not include the doc.
    let listing: Value = reqwest::get(format!("{}/api/v1/kb/documents", h.base_url))
        .await
        .expect("list")
        .json()
        .await
        .expect("list json");
    assert_eq!(listing.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn kb_drift_returns_report_with_current_model() {
    let h = start_harness(|_store| Box::pin(async move {})).await;

    let resp = reqwest::get(format!("{}/api/v1/kb/drift", h.base_url))
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json body");
    assert!(body["current_model"].is_string());
    // Empty store → no rows → in_sync = true.
    assert_eq!(body["in_sync"], true);
    assert_eq!(body["total_chunks"], 0);
    assert_eq!(body["stale_chunks"], 0);
}

#[tokio::test]
async fn kb_re_embed_dry_run_on_empty_store_writes_nothing() {
    let h = start_harness(|_store| Box::pin(async move {})).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/v1/kb/re-embed", h.base_url))
        .json(&serde_json::json!({
            "include_current": false,
            "dry_run": true,
            "batch_size": 50,
        }))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["chunks_re_embedded"], 0);
    assert_eq!(body["total_chunks_examined"], 0);
    assert!(body["errors"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn kb_search_with_empty_query_returns_400() {
    let h = start_harness(|_store| Box::pin(async move {})).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/v1/kb/search", h.base_url))
        .json(&serde_json::json!({ "query": "   " }))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["error"], "bad_request");
}

#[tokio::test]
async fn kb_routes_require_auth_when_pairing_enabled() {
    let h = start_harness_with_auth(true, &["rantaiclaw_test_token".into()], |_store| {
        Box::pin(async move {})
    })
    .await;

    // No Authorization header → 401.
    let unauth = reqwest::get(format!("{}/api/v1/kb/documents", h.base_url))
        .await
        .expect("request");
    assert_eq!(unauth.status(), 401);
    let body: Value = unauth.json().await.expect("json body");
    assert_eq!(body["error"], "unauthorized");

    // With a valid bearer token → 200.
    let client = reqwest::Client::new();
    let ok = client
        .get(format!("{}/api/v1/kb/documents", h.base_url))
        .header("Authorization", "Bearer rantaiclaw_test_token")
        .send()
        .await
        .expect("request");
    assert_eq!(ok.status(), 200);
}

#[tokio::test]
async fn kb_intelligence_returns_entities_relations_and_stats() {
    use rantaiclaw::kb::intelligence::types::{Entity, EntityMention, EntityType, ExtractSource};
    use rantaiclaw::kb::store::IntelligenceStore;

    let h = start_harness(|store| {
        Box::pin(async move {
            // Seed a document plus one intelligence entity + a mention of it,
            // exactly as the orchestrator would (Task-4 seeding pattern).
            store
                .create_document(&sample_doc("rantaiclaw_doc_intel", "Intel Doc"))
                .await
                .expect("seed doc");
            let entity_id = store
                .upsert_entity(&Entity {
                    id: "e_nqrust".into(),
                    canonical_key: "nqrust:Product".into(),
                    name: "NQRust".into(),
                    entity_type: EntityType::Product,
                    confidence: 0.9,
                    metadata: serde_json::json!({}),
                })
                .await
                .expect("seed entity");
            store
                .add_mention(&EntityMention {
                    id: "m_nqrust".into(),
                    entity_id,
                    document_id: "rantaiclaw_doc_intel".into(),
                    chunk_index: Some(0),
                    context: None,
                    source: ExtractSource::Llm,
                })
                .await
                .expect("seed mention");
        })
    })
    .await;

    let resp = reqwest::get(format!(
        "{}/api/v1/kb/documents/rantaiclaw_doc_intel/intelligence",
        h.base_url
    ))
    .await
    .expect("request");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json body");

    assert!(body["entities"].is_array(), "entities missing: {body}");
    assert!(body["relations"].is_array(), "relations missing: {body}");
    assert!(body["stats"].is_object(), "stats missing: {body}");

    let entities = body["entities"].as_array().unwrap();
    assert_eq!(entities.len(), 1, "one seeded entity: {body}");
    assert_eq!(entities[0]["name"], "NQRust");
    assert_eq!(entities[0]["entity_type"], "Product");

    assert_eq!(body["stats"]["total_entities"], 1);
    assert_eq!(body["stats"]["total_relations"], 0);
    assert_eq!(body["stats"]["entity_types"]["Product"], 1);
}

#[tokio::test]
async fn kb_graph_returns_nodes_edges_and_stats() {
    use rantaiclaw::kb::intelligence::types::{Entity, EntityMention, EntityType, ExtractSource};
    use rantaiclaw::kb::store::IntelligenceStore;

    let h = start_harness(|store| {
        Box::pin(async move {
            let entity_id = store
                .upsert_entity(&Entity {
                    id: "e_g".into(),
                    canonical_key: "nqrust:Product".into(),
                    name: "NQRust".into(),
                    entity_type: EntityType::Product,
                    confidence: 0.9,
                    metadata: serde_json::json!({}),
                })
                .await
                .expect("seed entity");
            store
                .add_mention(&EntityMention {
                    id: "m_g".into(),
                    entity_id,
                    document_id: "rantaiclaw_doc_g".into(),
                    chunk_index: Some(0),
                    context: None,
                    source: ExtractSource::Llm,
                })
                .await
                .expect("seed mention");
        })
    })
    .await;

    let resp = reqwest::get(format!("{}/api/v1/kb/graph?limit=10", h.base_url))
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json body");
    assert!(body["nodes"].is_array(), "nodes missing: {body}");
    assert!(body["edges"].is_array(), "edges missing: {body}");
    assert!(body["stats"].is_object(), "stats missing: {body}");
    assert_eq!(body["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(body["nodes"][0]["name"], "NQRust");
}

#[tokio::test]
async fn graph_limit_is_clamped_to_hard_cap() {
    use rantaiclaw::kb::intelligence::types::{Entity, EntityMention, EntityType, ExtractSource};
    use rantaiclaw::kb::store::IntelligenceStore;

    let h = start_harness(|store| {
        Box::pin(async move {
            let entity_id = store
                .upsert_entity(&Entity {
                    id: "e_lim".into(),
                    canonical_key: "lim:Product".into(),
                    name: "LimitProbe".into(),
                    entity_type: EntityType::Product,
                    confidence: 0.9,
                    metadata: serde_json::json!({}),
                })
                .await
                .expect("seed entity");
            store
                .add_mention(&EntityMention {
                    id: "m_lim".into(),
                    entity_id,
                    document_id: "rantaiclaw_doc_lim".into(),
                    chunk_index: Some(0),
                    context: None,
                    source: ExtractSource::Llm,
                })
                .await
                .expect("seed mention");
        })
    })
    .await;

    // A caller-supplied limit far above the hard cap must not error; the
    // response stays bounded by the server ceiling.
    let resp = reqwest::get(format!("{}/api/v1/kb/graph?limit=100000", h.base_url))
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json body");
    assert!(body["nodes"].as_array().unwrap().len() <= 5000);
}

#[tokio::test]
async fn graph_stats_truncated_flag_reflects_cap() {
    use rantaiclaw::kb::intelligence::types::{Entity, EntityMention, EntityType, ExtractSource};
    use rantaiclaw::kb::store::IntelligenceStore;

    let h = start_harness(|store| {
        Box::pin(async move {
            for (id, key, name) in [("t1", "trunc:A", "A"), ("t2", "trunc:B", "B")] {
                store
                    .upsert_entity(&Entity {
                        id: id.into(),
                        canonical_key: key.into(),
                        name: name.into(),
                        entity_type: EntityType::Product,
                        confidence: 0.9,
                        metadata: serde_json::json!({}),
                    })
                    .await
                    .expect("seed entity");
                store
                    .add_mention(&EntityMention {
                        id: format!("m_{id}"),
                        entity_id: id.into(),
                        document_id: "rantaiclaw_doc_trunc".into(),
                        chunk_index: Some(0),
                        context: None,
                        source: ExtractSource::Llm,
                    })
                    .await
                    .expect("seed mention");
            }
        })
    })
    .await;

    // limit below the corpus (2 entities) → truncated flag set, nodes capped at 1.
    let body: Value = reqwest::get(format!("{}/api/v1/kb/graph?limit=1", h.base_url))
        .await
        .expect("request")
        .json()
        .await
        .expect("json body");
    assert_eq!(
        body["stats"]["truncated"], true,
        "expected truncated: {body}"
    );
    assert_eq!(body["stats"]["corpus_entities"], 2);
    assert_eq!(body["nodes"].as_array().unwrap().len(), 1);

    // limit above the corpus → not truncated.
    let body2: Value = reqwest::get(format!("{}/api/v1/kb/graph?limit=10", h.base_url))
        .await
        .expect("request")
        .json()
        .await
        .expect("json body");
    assert_eq!(
        body2["stats"]["truncated"], false,
        "expected not truncated: {body2}"
    );
}

#[tokio::test]
async fn graph_exposes_capability() {
    // Default config: intelligence extraction is disabled and the model name
    // (e.g. "openai/gpt-4.1-nano") always contains a "/".
    let h = start_harness(|_store| Box::pin(async move {})).await;

    let body: Value = reqwest::get(format!("{}/api/v1/kb/graph", h.base_url))
        .await
        .expect("request")
        .json()
        .await
        .expect("json body");
    assert_eq!(
        body["capability"]["intelligence_enabled"], false,
        "capability: {body}"
    );
    assert!(
        body["capability"]["extraction_model"]
            .as_str()
            .unwrap()
            .contains('/'),
        "extraction_model: {body}"
    );
}

#[tokio::test]
async fn graph_dedupes_edges_weights_and_recomputes_degree() {
    use rantaiclaw::kb::intelligence::types::{
        Entity, EntityMention, EntityType, ExtractSource, Relation, RelationType,
    };
    use rantaiclaw::kb::store::IntelligenceStore;

    let h = start_harness(|store| {
        Box::pin(async move {
            for (id, key, name) in [("a", "k:A", "A"), ("b", "k:B", "B"), ("c", "k:C", "C")] {
                store
                    .upsert_entity(&Entity {
                        id: id.into(),
                        canonical_key: key.into(),
                        name: name.into(),
                        entity_type: EntityType::Product,
                        confidence: 0.9,
                        metadata: serde_json::json!({}),
                    })
                    .await
                    .unwrap();
                store
                    .add_mention(&EntityMention {
                        id: format!("m_{id}"),
                        entity_id: id.into(),
                        document_id: "doc1".into(),
                        chunk_index: Some(0),
                        context: None,
                        source: ExtractSource::Llm,
                    })
                    .await
                    .unwrap();
            }
            // A->B extracted from TWO docs (duplicate pair+type), B->C once.
            for (rid, doc) in [("r1", "doc1"), ("r2", "doc2")] {
                store
                    .add_relation(&Relation {
                        id: rid.into(),
                        source_entity_id: "a".into(),
                        target_entity_id: "b".into(),
                        relation_type: RelationType::RelatedTo,
                        confidence: 0.9,
                        document_id: doc.into(),
                        metadata: serde_json::json!({}),
                    })
                    .await
                    .unwrap();
            }
            store
                .add_relation(&Relation {
                    id: "r3".into(),
                    source_entity_id: "b".into(),
                    target_entity_id: "c".into(),
                    relation_type: RelationType::RelatedTo,
                    confidence: 0.9,
                    document_id: "doc2".into(),
                    metadata: serde_json::json!({}),
                })
                .await
                .unwrap();
        })
    })
    .await;

    let body: Value = reqwest::get(format!("{}/api/v1/kb/graph?limit=100", h.base_url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let edges = body["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2, "A->B deduped + B->C");
    let ab = edges
        .iter()
        .find(|e| e["source"] == "a" && e["target"] == "b")
        .unwrap();
    assert_eq!(ab["weight"], 2);
    let deg = |n: &str| {
        body["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == n)
            .unwrap()["degree"]
            .as_u64()
            .unwrap()
    };
    assert_eq!((deg("a"), deg("b"), deg("c")), (1, 2, 1)); // NOT inflated by the duplicate
    assert_eq!(body["stats"]["corpus_entities"], 3);
    assert_eq!(body["stats"]["corpus_relations"], 2); // distinct (src,tgt,type)
}

#[tokio::test]
async fn graph_corpus_entities_respects_group() {
    use rantaiclaw::kb::intelligence::types::{
        Entity, EntityMention, EntityType, ExtractSource, Relation, RelationType,
    };
    use rantaiclaw::kb::store::IntelligenceStore;

    // `document_group.group_id` has an FK against `knowledge_base_group.id`,
    // and `create_group` generates that id server-side — so the seed closure
    // (which only returns `()`) stashes it here for the assertions below.
    // `start_harness` awaits the closure to completion before returning, so
    // this is populated by the time we read it.
    let group_id_cell: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let group_id_cell2 = group_id_cell.clone();

    let h = start_harness(|store| {
        Box::pin(async move {
            // A document that belongs to a freshly created group.
            let group = store
                .create_group("Group G", None, None)
                .await
                .expect("create group");
            *group_id_cell2.lock() = Some(group.id.clone());
            store
                .create_document(&sample_doc("rantaiclaw_doc_group", "Group Doc"))
                .await
                .expect("seed doc");
            store
                .add_document_to_group("rantaiclaw_doc_group", &group.id)
                .await
                .expect("link doc to group");

            // Three entities; only "a" and "b" are mentioned from the
            // in-group document. "c" is mentioned from an out-of-group doc.
            for (id, key, name) in [("a", "k:A", "A"), ("b", "k:B", "B"), ("c", "k:C", "C")] {
                store
                    .upsert_entity(&Entity {
                        id: id.into(),
                        canonical_key: key.into(),
                        name: name.into(),
                        entity_type: EntityType::Product,
                        confidence: 0.9,
                        metadata: serde_json::json!({}),
                    })
                    .await
                    .expect("seed entity");
            }
            for id in ["a", "b"] {
                store
                    .add_mention(&EntityMention {
                        id: format!("m_{id}"),
                        entity_id: id.into(),
                        document_id: "rantaiclaw_doc_group".into(),
                        chunk_index: Some(0),
                        context: None,
                        source: ExtractSource::Llm,
                    })
                    .await
                    .expect("seed mention");
            }
            store
                .add_mention(&EntityMention {
                    id: "m_c".into(),
                    entity_id: "c".into(),
                    document_id: "rantaiclaw_doc_other".into(),
                    chunk_index: Some(0),
                    context: None,
                    source: ExtractSource::Llm,
                })
                .await
                .expect("seed mention");

            // One relation between two in-group entities (a->b), and one
            // relation reaching an out-of-group entity (b->c). The grouped
            // `corpus_relations` count restricts BOTH endpoints to in-group
            // entities, so only a->b should be counted there.
            store
                .add_relation(&Relation {
                    id: "r_ab".into(),
                    source_entity_id: "a".into(),
                    target_entity_id: "b".into(),
                    relation_type: RelationType::RelatedTo,
                    confidence: 0.9,
                    document_id: "rantaiclaw_doc_group".into(),
                    metadata: serde_json::json!({}),
                })
                .await
                .expect("seed relation a->b");
            store
                .add_relation(&Relation {
                    id: "r_bc".into(),
                    source_entity_id: "b".into(),
                    target_entity_id: "c".into(),
                    relation_type: RelationType::RelatedTo,
                    confidence: 0.9,
                    document_id: "rantaiclaw_doc_other".into(),
                    metadata: serde_json::json!({}),
                })
                .await
                .expect("seed relation b->c");
        })
    })
    .await;

    let group_id = group_id_cell.lock().clone().expect("group id captured");
    let grouped: Value = reqwest::get(format!(
        "{}/api/v1/kb/graph?group={group_id}&limit=100",
        h.base_url
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(grouped["stats"]["corpus_entities"], 2, "grouped: {grouped}");
    assert_eq!(
        grouped["stats"]["corpus_relations"], 1,
        "grouped: {grouped}"
    );

    let ungrouped: Value = reqwest::get(format!("{}/api/v1/kb/graph?limit=100", h.base_url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        ungrouped["stats"]["corpus_entities"], 3,
        "ungrouped: {ungrouped}"
    );
    assert_eq!(
        ungrouped["stats"]["corpus_relations"], 2,
        "ungrouped: {ungrouped}"
    );
    assert!(
        grouped["stats"]["corpus_relations"].as_u64().unwrap()
            < ungrouped["stats"]["corpus_relations"].as_u64().unwrap(),
        "group scope must restrict corpus_relations to in-group entities only: grouped={grouped} ungrouped={ungrouped}"
    );
}

#[tokio::test]
async fn kb_ingest_missing_file_field_returns_400() {
    let h = start_harness(|_store| Box::pin(async move {})).await;

    let client = reqwest::Client::new();
    // Multipart with only a `title` field — no `file` part.
    let form = reqwest::multipart::Form::new().text("title", "no file attached");
    let resp = client
        .post(format!("{}/api/v1/kb/documents", h.base_url))
        .multipart(form)
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["error"], "bad_request");
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|s| s.contains("'file' field is required")),
        "detail should mention missing file: {body}"
    );
}
