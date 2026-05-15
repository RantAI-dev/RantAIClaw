//! Integration tests for the sqlite-vec + FTS5 KbStore backend.
//!
//! Each test uses a fresh `TempDir` so the on-disk database lifetime is
//! scoped to the test. `embedding_dim = 4` keeps fixtures cheap.

use chrono::Utc;
use tempfile::TempDir;

use rantaiclaw::kb::store::sqlite::SqliteStore;
use rantaiclaw::kb::store::{KbStore, SearchFilter};
use rantaiclaw::kb::{Chunk, ChunkMetadata, Document, DocumentId};

fn ones(dim: usize) -> Vec<f32> {
    vec![1.0; dim]
}
fn alt(dim: usize, mag: f32) -> Vec<f32> {
    let mut v = vec![0.0; dim];
    v[0] = mag;
    v
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
        organization_id: Some("org-1".into()),
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

#[tokio::test]
async fn create_get_list_delete_document() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("kb.db");
    let store = SqliteStore::open(&path, 4).await.unwrap();

    let doc = sample_doc("doc-1", "First Doc");
    store.create_document(&doc).await.unwrap();

    let got = store
        .get_document(&doc.id)
        .await
        .unwrap()
        .expect("doc must exist");
    assert_eq!(got.title, "First Doc");
    assert_eq!(got.categories, vec!["FAQ"]);

    let list = store.list_documents(Some("org-1")).await.unwrap();
    assert_eq!(list.len(), 1);

    store
        .delete_document(&doc.id, /* soft */ true)
        .await
        .unwrap();
    let soft = store
        .get_document(&doc.id)
        .await
        .unwrap()
        .expect("still present after soft delete");
    assert!(soft.deleted_at.is_some());

    store
        .delete_document(&doc.id, /* soft */ false)
        .await
        .unwrap();
    assert!(store.get_document(&doc.id).await.unwrap().is_none());
}

#[tokio::test]
async fn store_chunks_and_vector_search() {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let doc = sample_doc("doc-vec", "Vec Doc");
    store.create_document(&doc).await.unwrap();

    let chunks = vec![
        Chunk {
            content: "alpha content".into(),
            metadata: ChunkMetadata {
                document_title: doc.title.clone(),
                category: "FAQ".into(),
                subcategory: None,
                section: None,
                chunk_index: 0,
                contextual_prefix: None,
            },
        },
        Chunk {
            content: "beta content".into(),
            metadata: ChunkMetadata {
                document_title: doc.title.clone(),
                category: "FAQ".into(),
                subcategory: None,
                section: None,
                chunk_index: 1,
                contextual_prefix: None,
            },
        },
    ];
    let embeds = vec![ones(4), alt(4, 2.0)];
    store
        .store_chunks(&doc.id, &chunks, &embeds, "test-model")
        .await
        .unwrap();

    assert_eq!(store.chunk_count(&doc.id).await.unwrap(), 2);

    let results = store
        .search_by_vector(&ones(4), 5, &SearchFilter::default())
        .await
        .unwrap();
    assert!(!results.is_empty());
    // The "alpha" chunk's embedding matches the query, so it should rank first.
    assert_eq!(results[0].content, "alpha content");
}

#[tokio::test]
async fn dimension_mismatch_errors_loudly() {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let doc = sample_doc("doc-mis", "Mismatch");
    store.create_document(&doc).await.unwrap();

    let chunks = vec![Chunk {
        content: "x".into(),
        metadata: ChunkMetadata {
            document_title: doc.title.clone(),
            category: "FAQ".into(),
            subcategory: None,
            section: None,
            chunk_index: 0,
            contextual_prefix: None,
        },
    }];
    let bad_embed = vec![vec![1.0, 2.0, 3.0]]; // dim=3 vs configured 4
    let err = store
        .store_chunks(&doc.id, &chunks, &bad_embed, "test-model")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        rantaiclaw::kb::KbError::DimensionMismatch { .. }
    ));
}
