//! Integration tests for `src/kb/maintenance/` — Phase 9.
//!
//! Uses a real [`SqliteStore`] (with `TempDir`) instead of mocks because the
//! drift report flows through the store's SQL aggregation, and the bulk
//! re-embed path exercises a multi-statement UPDATE on `chunk` + `chunk_vec`
//! that's the whole point of the test.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tempfile::TempDir;

use rantaiclaw::kb::embed::EmbeddingProvider;
use rantaiclaw::kb::maintenance::{check_drift, run_bulk_re_embed, BulkReEmbedOptions};
use rantaiclaw::kb::store::sqlite::SqliteStore;
use rantaiclaw::kb::store::KbStore;
use rantaiclaw::kb::{Chunk, ChunkMetadata, Document, DocumentId, KbConfig, KbError, KbResult};

const DIM: usize = 4;

fn ones() -> Vec<f32> {
    vec![1.0; DIM]
}

fn sample_doc(id: &str) -> Document {
    Document {
        id: DocumentId(id.into()),
        title: format!("doc {id}"),
        content: format!("body of {id}"),
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

fn sample_chunk(doc_title: &str, idx: usize) -> Chunk {
    Chunk {
        content: format!("chunk content {idx}"),
        metadata: ChunkMetadata {
            document_title: doc_title.into(),
            category: "FAQ".into(),
            subcategory: None,
            section: None,
            chunk_index: idx,
            contextual_prefix: None,
        },
    }
}

/// Build a `KbConfig` with `embedding_model` set to the supplied label.
/// All other fields are placeholder values — maintenance code only reads
/// `embedding_model` and `embedding_dim`.
fn cfg_with_model(model: &str) -> KbConfig {
    KbConfig {
        extract_primary: "smart".into(),
        extract_fallback: "unpdf".into(),
        extract_smart_fallback: "rantaiclaw_test_model_a".into(),
        embedding_model: model.into(),
        embedding_dim: DIM,
        default_max_chunks: 4,
        rerank_enabled: false,
        rerank_provider: String::new(),
        rerank_model: "rantaiclaw_test_model_a".into(),
        rerank_initial_k: 20,
        rerank_final_k: 5,
        hybrid_bm25_enabled: true,
        contextual_retrieval_enabled: false,
        contextual_retrieval_model: "rantaiclaw_test_model_a".into(),
        query_expansion_enabled: false,
        query_expansion_model: "rantaiclaw_test_model_a".into(),
        query_expansion_paraphrases: 3,
        standalone_query_enabled: false,
        extract_vision_base_url: String::new(),
        extract_vision_api_key: String::new(),
        extract_mineru_base_url: String::new(),
        embedding_base_url: "http://localhost".into(),
        embedding_api_key: String::new(),
        embed_batch_size: 100,
        embed_concurrency: 2,
        query_embed_cache_size: 8,
        query_embed_cache_ttl_ms: 60_000,
        openrouter_chat_url: "http://localhost".into(),
        intelligence_enabled: false,
        intelligence_model: "openai/gpt-4.1-nano".into(),
        intelligence_resolution: "exact".into(),
        graph_max_nodes: 200,
        graphrag_enabled: false,
        graphrag_max_neighbors: 20,
    }
}

async fn fresh_store() -> (TempDir, Arc<dyn KbStore>) {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("kb.db");
    let store = SqliteStore::open(&path, DIM).await.unwrap();
    let store: Arc<dyn KbStore> = Arc::new(store);
    (tmp, store)
}

// ---- Task 9.1: drift report ------------------------------------------

#[tokio::test]
async fn drift_report_in_sync_when_all_match() {
    let (_tmp, store) = fresh_store().await;
    let cfg = cfg_with_model("rantaiclaw_model_current");

    let doc = sample_doc("rantaiclaw_doc_a");
    store.create_document(&doc).await.unwrap();
    let chunks: Vec<_> = (0..3).map(|i| sample_chunk(&doc.title, i)).collect();
    let embeds = vec![ones(); 3];
    store
        .store_chunks(&doc.id, &chunks, &embeds, &cfg.embedding_model)
        .await
        .unwrap();

    let report = check_drift(&cfg, &store).await.unwrap();
    assert!(report.in_sync);
    assert_eq!(report.stale_chunk_count, 0);
    assert_eq!(report.current_model, "rantaiclaw_model_current");
}

#[tokio::test]
async fn drift_report_counts_stale() {
    let (_tmp, store) = fresh_store().await;
    let cfg = cfg_with_model("rantaiclaw_model_current");

    let doc = sample_doc("rantaiclaw_doc_mixed");
    store.create_document(&doc).await.unwrap();
    let chunks: Vec<_> = (0..4).map(|i| sample_chunk(&doc.title, i)).collect();
    let embeds = vec![ones(); 4];

    // 2 chunks on the old model, 2 on the current model.
    store
        .store_chunks(&doc.id, &chunks[..2], &embeds[..2], "rantaiclaw_model_old")
        .await
        .unwrap();
    store
        .store_chunks(&doc.id, &chunks[2..], &embeds[..2], &cfg.embedding_model)
        .await
        .unwrap();

    let report = check_drift(&cfg, &store).await.unwrap();
    assert!(!report.in_sync);
    assert_eq!(report.stale_chunk_count, 2);
    // Both models surface in by_model.
    let labels: Vec<Option<String>> = report.by_model.iter().map(|(m, _)| m.clone()).collect();
    assert!(labels.contains(&Some("rantaiclaw_model_old".into())));
    assert!(labels.contains(&Some("rantaiclaw_model_current".into())));
}

#[tokio::test]
async fn drift_report_treats_null_model_as_stale() {
    // Pinpoint test for the `None` arm of the filter — uses a raw
    // rusqlite UPDATE to NULL out the embedding_model column on a chunk
    // (simulating a pre-tracking row from before the column existed).
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("kb.db");
    let store = SqliteStore::open(&path, DIM).await.unwrap();
    let cfg = cfg_with_model("rantaiclaw_model_current");

    let doc = sample_doc("rantaiclaw_doc_pretrack");
    store.create_document(&doc).await.unwrap();
    let chunks: Vec<_> = (0..2).map(|i| sample_chunk(&doc.title, i)).collect();
    let embeds = vec![ones(); 2];
    store
        .store_chunks(&doc.id, &chunks, &embeds, &cfg.embedding_model)
        .await
        .unwrap();

    // Drop the SqliteStore connection lock by opening a sibling connection
    // to the same file. The store's mutex is held for the duration of any
    // method call; once the awaits above return, the mutex is released.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE chunk SET embedding_model = NULL
             WHERE id = (SELECT id FROM chunk LIMIT 1)",
            [],
        )
        .unwrap();
    }

    let store_arc: Arc<dyn KbStore> = Arc::new(store);
    let report = check_drift(&cfg, &store_arc).await.unwrap();
    assert_eq!(
        report.stale_chunk_count, 1,
        "the NULL-model row must count as stale"
    );
    assert!(!report.in_sync);
    assert!(
        report.by_model.iter().any(|(m, _)| m.is_none()),
        "by_model must surface the NULL bucket"
    );
}

#[tokio::test]
async fn drift_report_includes_current_model() {
    let (_tmp, store) = fresh_store().await;
    let cfg = cfg_with_model("rantaiclaw_model_xyz");
    let report = check_drift(&cfg, &store).await.unwrap();
    assert_eq!(report.current_model, "rantaiclaw_model_xyz");
    assert!(report.in_sync, "empty store has no stale chunks");
    assert!(report.by_model.is_empty());
}

// ---- Task 9.2: bulk re-embed -----------------------------------------

/// Fake embedder that returns a constant vector per call. The first
/// component is set to `marker` so the test can assert that the stored
/// vector was actually replaced (no cheap way to read vec0 bytes back via
/// the public API, but we use the `embedding_model` tag as a proxy and the
/// `fail_on_batch` knob to exercise the per-batch error path).
struct ConstantEmbedder {
    dim: usize,
    /// Embed-many call counter. Incremented BEFORE the failure check, so
    /// `fail_on_call == 1` fails the first call.
    calls: Arc<AtomicUsize>,
    /// When `Some(n)`, embed_many returns an error on call number `n`.
    fail_on_call: Option<usize>,
}

impl ConstantEmbedder {
    fn new(dim: usize) -> Self {
        Self {
            dim,
            calls: Arc::new(AtomicUsize::new(0)),
            fail_on_call: None,
        }
    }
    fn failing(dim: usize, fail_on_call: usize) -> Self {
        Self {
            dim,
            calls: Arc::new(AtomicUsize::new(0)),
            fail_on_call: Some(fail_on_call),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for ConstantEmbedder {
    fn model(&self) -> &str {
        "rantaiclaw_test_constant_embedder"
    }
    fn dim(&self) -> usize {
        self.dim
    }
    async fn embed_query(&self, _text: &str) -> KbResult<Vec<f32>> {
        Ok(vec![1.0; self.dim])
    }
    async fn embed_many(&self, texts: &[String]) -> KbResult<Vec<Vec<f32>>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if Some(n) == self.fail_on_call {
            return Err(KbError::Other(format!("synthetic failure on call {n}")));
        }
        Ok(texts.iter().map(|_| vec![1.0; self.dim]).collect())
    }
}

/// Seed `count` chunks tagged with `model_tag`. Each chunk has a unique
/// `chunk_index` so the resulting `chunk.id` ("<doc>_<index>") is unique.
async fn seed_chunks(
    store: &Arc<dyn KbStore>,
    doc: &Document,
    starting_index: usize,
    count: usize,
    model_tag: &str,
) {
    let chunks: Vec<_> = (0..count)
        .map(|i| sample_chunk(&doc.title, starting_index + i))
        .collect();
    let embeds = vec![ones(); count];
    store
        .store_chunks(&doc.id, &chunks, &embeds, model_tag)
        .await
        .unwrap();
}

#[tokio::test]
async fn bulk_re_embed_skips_already_current_chunks() {
    let (_tmp, store) = fresh_store().await;
    let cfg = cfg_with_model("rantaiclaw_model_current");

    let doc = sample_doc("rantaiclaw_doc_skip");
    store.create_document(&doc).await.unwrap();
    // 3 chunks already on current model, 3 on an old model.
    seed_chunks(&store, &doc, 0, 3, &cfg.embedding_model).await;
    seed_chunks(&store, &doc, 3, 3, "rantaiclaw_model_old").await;

    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(ConstantEmbedder::new(DIM));
    let report = run_bulk_re_embed(
        &cfg,
        &store,
        &embedder,
        BulkReEmbedOptions {
            batch_size: 10,
            include_already_current: false,
            dry_run: false,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        report.chunks_re_embedded, 3,
        "only stale chunks re-embedded"
    );
    assert_eq!(report.total_chunks_examined, 3);
    assert!(report.errors.is_empty());

    // After the run, drift must be zero — all stale rows now tagged current.
    let drift = check_drift(&cfg, &store).await.unwrap();
    assert!(
        drift.in_sync,
        "drift must be cleared after a successful run"
    );
}

#[tokio::test]
async fn bulk_re_embed_processes_all_when_include_current() {
    let (_tmp, store) = fresh_store().await;
    let cfg = cfg_with_model("rantaiclaw_model_current");

    let doc = sample_doc("rantaiclaw_doc_all");
    store.create_document(&doc).await.unwrap();
    seed_chunks(&store, &doc, 0, 3, &cfg.embedding_model).await;
    seed_chunks(&store, &doc, 3, 3, "rantaiclaw_model_old").await;

    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(ConstantEmbedder::new(DIM));
    let report = run_bulk_re_embed(
        &cfg,
        &store,
        &embedder,
        BulkReEmbedOptions {
            batch_size: 10,
            include_already_current: true,
            dry_run: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        report.chunks_re_embedded, 6,
        "all six chunks processed when include_already_current=true"
    );
}

#[tokio::test]
async fn bulk_re_embed_dry_run_writes_nothing() {
    let (_tmp, store) = fresh_store().await;
    let cfg = cfg_with_model("rantaiclaw_model_current");

    let doc = sample_doc("rantaiclaw_doc_dry");
    store.create_document(&doc).await.unwrap();
    seed_chunks(&store, &doc, 0, 4, "rantaiclaw_model_old").await;

    let drift_before = check_drift(&cfg, &store).await.unwrap();
    assert_eq!(drift_before.stale_chunk_count, 4);

    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(ConstantEmbedder::new(DIM));
    let report = run_bulk_re_embed(
        &cfg,
        &store,
        &embedder,
        BulkReEmbedOptions {
            batch_size: 10,
            include_already_current: false,
            dry_run: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.chunks_re_embedded, 4, "report shows would-be writes");

    // DB must be untouched — drift report identical to before.
    let drift_after = check_drift(&cfg, &store).await.unwrap();
    assert_eq!(
        drift_after.stale_chunk_count, 4,
        "dry_run must NOT modify the DB"
    );
    assert!(!drift_after.in_sync);
}

#[tokio::test]
async fn bulk_re_embed_continues_after_batch_error() {
    let (_tmp, store) = fresh_store().await;
    let cfg = cfg_with_model("rantaiclaw_model_current");

    let doc = sample_doc("rantaiclaw_doc_err");
    store.create_document(&doc).await.unwrap();
    // 5 stale chunks; batch_size=2 means at least 3 batches.
    seed_chunks(&store, &doc, 0, 5, "rantaiclaw_model_old").await;

    // Fail call #2 — first batch succeeds, second batch fails, third batch
    // succeeds. Net: at least one error recorded, plus partial progress.
    let embedder = Arc::new(ConstantEmbedder::failing(DIM, 2));
    let calls_handle = embedder.calls.clone();
    let embedder_dyn: Arc<dyn EmbeddingProvider> = embedder;
    let report = run_bulk_re_embed(
        &cfg,
        &store,
        &embedder_dyn,
        BulkReEmbedOptions {
            batch_size: 2,
            include_already_current: false,
            dry_run: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.errors.len(), 1, "exactly one batch failed");
    assert!(
        report.chunks_re_embedded >= 2,
        "at least one successful batch (2 chunks) advanced"
    );
    assert!(
        calls_handle.load(Ordering::SeqCst) >= 3,
        "embedder kept being called after the failing batch"
    );
}

#[tokio::test]
async fn bulk_re_embed_respects_batch_size() {
    let (_tmp, store) = fresh_store().await;
    let cfg = cfg_with_model("rantaiclaw_model_current");

    let doc = sample_doc("rantaiclaw_doc_batches");
    store.create_document(&doc).await.unwrap();
    // 25 stale chunks; with batch_size=10 the driver makes 3 pages.
    seed_chunks(&store, &doc, 0, 25, "rantaiclaw_model_old").await;

    let embedder = Arc::new(ConstantEmbedder::new(DIM));
    let calls_handle = embedder.calls.clone();
    let embedder_dyn: Arc<dyn EmbeddingProvider> = embedder;
    let report = run_bulk_re_embed(
        &cfg,
        &store,
        &embedder_dyn,
        BulkReEmbedOptions {
            batch_size: 10,
            include_already_current: false,
            dry_run: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.chunks_re_embedded, 25);
    assert_eq!(
        calls_handle.load(Ordering::SeqCst),
        3,
        "25 chunks at batch_size=10 -> 3 embed_many calls"
    );
}
