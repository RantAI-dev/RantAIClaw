//! Integration tests for the sqlite-vec + FTS5 KbStore backend.
//!
//! Each test uses a fresh `TempDir` so the on-disk database lifetime is
//! scoped to the test. `embedding_dim = 4` keeps fixtures cheap.

use chrono::Utc;
use tempfile::TempDir;

use rantaiclaw::kb::store::sqlite::SqliteStore;
use rantaiclaw::kb::store::KbStore;
use rantaiclaw::kb::{Document, DocumentId};

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
