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
    // Soft-deleted docs must be hidden from `get_document` so callers can't
    // accidentally surface them (matches the TS reference, which filters
    // `deletedAt: null` in vector-store.ts).
    assert!(
        store.get_document(&doc.id).await.unwrap().is_none(),
        "soft-deleted docs must not be returned by get_document"
    );
    // …and from `list_documents` as well.
    let list_after_soft = store.list_documents(Some("org-1")).await.unwrap();
    assert_eq!(
        list_after_soft.len(),
        0,
        "soft-deleted docs must not appear in list_documents"
    );

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
async fn category_filter_uses_array_membership_not_substring() {
    // Regression: the previous `categories_json LIKE '%X%'` filter matched
    // any substring of the JSON blob, so a query for category "A" would
    // match a doc categorized ["FAQ"]. The fix uses `json_each` for true
    // array-element equality. This test would have failed with the LIKE
    // implementation.
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let doc = sample_doc("doc-cat", "Category Doc");
    // `categories = ["FAQ"]` from `sample_doc` — substring "A" appears in "FAQ".
    store.create_document(&doc).await.unwrap();

    let chunks = vec![Chunk {
        content: "alpha content".into(),
        metadata: ChunkMetadata {
            document_title: doc.title.clone(),
            category: "FAQ".into(),
            subcategory: None,
            section: None,
            chunk_index: 0,
            contextual_prefix: None,
        },
    }];
    store
        .store_chunks(&doc.id, &chunks, &[ones(4)], "test-model")
        .await
        .unwrap();

    // Query for the wrong category — must return no hits with json_each.
    // With the old LIKE substring filter, "A" would have matched "FAQ".
    let filter = SearchFilter {
        category: Some("A".into()),
        ..Default::default()
    };
    let results = store
        .search_by_vector(&ones(4), 5, &filter)
        .await
        .unwrap();
    assert!(
        results.is_empty(),
        "category 'A' must not match doc categorized ['FAQ']; got {results:?}"
    );

    // Sanity: exact match still works.
    let filter_exact = SearchFilter {
        category: Some("FAQ".into()),
        ..Default::default()
    };
    let hits = store
        .search_by_vector(&ones(4), 5, &filter_exact)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "exact category match must still return the doc");
}

#[tokio::test]
async fn chunk_count_hides_soft_deleted_doc() {
    // Soft-deleting a document must logically zero out its chunk count so
    // callers can rely on `chunk_count == 0 iff doc invisible`, even though
    // the chunk rows physically remain until hard-delete.
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let doc = sample_doc("doc-soft", "Soft Doc");
    store.create_document(&doc).await.unwrap();

    let chunks = vec![Chunk {
        content: "alpha".into(),
        metadata: ChunkMetadata {
            document_title: doc.title.clone(),
            category: "FAQ".into(),
            subcategory: None,
            section: None,
            chunk_index: 0,
            contextual_prefix: None,
        },
    }];
    store
        .store_chunks(&doc.id, &chunks, &[ones(4)], "test-model")
        .await
        .unwrap();
    assert_eq!(store.chunk_count(&doc.id).await.unwrap(), 1);

    // Soft-delete -> chunk_count must drop to 0 logically.
    store.delete_document(&doc.id, true).await.unwrap();
    assert_eq!(
        store.chunk_count(&doc.id).await.unwrap(),
        0,
        "chunk_count must hide chunks belonging to soft-deleted docs"
    );

    // Same guarantee for the batch variant.
    let counts = store.chunk_counts(&[doc.id.clone()]).await.unwrap();
    assert_eq!(counts.get(&doc.id).copied(), Some(0));
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

#[tokio::test]
async fn bm25_search_returns_lexical_matches() {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let doc = sample_doc("doc-bm", "BM Doc");
    store.create_document(&doc).await.unwrap();

    let chunks = vec![
        Chunk {
            content: "the quick brown fox jumps".into(),
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
            content: "lazy dog sleeps quietly".into(),
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
    let embeds = vec![ones(4), ones(4)];
    store
        .store_chunks(&doc.id, &chunks, &embeds, "test-model")
        .await
        .unwrap();

    let hits = store.bm25_search("fox", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].content.contains("fox"));
    // Score is negated (higher = better) so a matching hit must be > 0.
    assert!(hits[0].score > 0.0);
}

#[tokio::test]
async fn count_by_embedding_model_aggregates() {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let doc = sample_doc("doc-d", "Drift");
    store.create_document(&doc).await.unwrap();

    let chunks: Vec<_> = (0..3)
        .map(|i| Chunk {
            content: format!("c{i}"),
            metadata: ChunkMetadata {
                document_title: doc.title.clone(),
                category: "FAQ".into(),
                subcategory: None,
                section: None,
                chunk_index: i,
                contextual_prefix: None,
            },
        })
        .collect();
    let embeds = vec![ones(4); 3];
    store
        .store_chunks(&doc.id, &chunks[..2], &embeds[..2], "model-a")
        .await
        .unwrap();
    store
        .store_chunks(&doc.id, &chunks[2..], &embeds[..1], "model-b")
        .await
        .unwrap();

    let mut counts = store.count_by_embedding_model().await.unwrap();
    counts.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        counts,
        vec![(Some("model-a".into()), 2), (Some("model-b".into()), 1),]
    );
}
