//! Integration tests for `src/kb/maintenance/` — Phase 9.
//!
//! Uses a real [`SqliteStore`] (with `TempDir`) instead of mocks because the
//! drift report flows through the store's SQL aggregation, and the bulk
//! re-embed path exercises a multi-statement UPDATE on `chunk` + `chunk_vec`
//! that's the whole point of the test.

use std::sync::Arc;

use chrono::Utc;
use tempfile::TempDir;

use rantaiclaw::kb::maintenance::check_drift;
use rantaiclaw::kb::store::sqlite::SqliteStore;
use rantaiclaw::kb::store::KbStore;
use rantaiclaw::kb::{Chunk, ChunkMetadata, Document, DocumentId, KbConfig};

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
