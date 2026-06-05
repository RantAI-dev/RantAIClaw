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

#[tokio::test]
async fn create_get_list_delete_document() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("kb.db");
    let store = SqliteStore::open(&path, 4).await.unwrap();

    let doc = sample_doc("rantaiclaw_doc_alpha", "First Doc");
    store.create_document(&doc).await.unwrap();

    let got = store
        .get_document(&doc.id)
        .await
        .unwrap()
        .expect("doc must exist");
    assert_eq!(got.title, "First Doc");
    assert_eq!(got.categories, vec!["FAQ"]);

    let list = store
        .list_documents(Some("rantaiclaw_org_a"))
        .await
        .unwrap();
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
    let list_after_soft = store
        .list_documents(Some("rantaiclaw_org_a"))
        .await
        .unwrap();
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
async fn hard_delete_clears_vectors_so_reingest_does_not_collide() {
    // Regression: hard-deleting a document must remove its chunk + chunk_vec
    // rows. chunk_vec (vec0) has no foreign key, so a stale vector orphans and
    // the next ingest's reused rowid hits
    // `UNIQUE constraint failed on chunk_vec primary key`.
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();

    let one_chunk = |title: &str, body: &str| {
        vec![Chunk {
            content: body.into(),
            metadata: ChunkMetadata {
                document_title: title.into(),
                category: "FAQ".into(),
                subcategory: None,
                section: None,
                chunk_index: 0,
                contextual_prefix: None,
            },
        }]
    };

    let doc_a = sample_doc("rantaiclaw_doc_del_a", "Doc A");
    store.create_document(&doc_a).await.unwrap();
    store
        .store_chunks(
            &doc_a.id,
            &one_chunk("Doc A", "alpha"),
            &[ones(4)],
            "rantaiclaw_test_model_a",
        )
        .await
        .unwrap();
    assert_eq!(store.chunk_count(&doc_a.id).await.unwrap(), 1);

    // Hard delete must clear the chunk AND its vector.
    store
        .delete_document(&doc_a.id, /* soft */ false)
        .await
        .unwrap();
    assert_eq!(store.chunk_count(&doc_a.id).await.unwrap(), 0);

    // A fresh ingest reuses rowid 1; before the fix this collided on chunk_vec.
    let doc_b = sample_doc("rantaiclaw_doc_del_b", "Doc B");
    store.create_document(&doc_b).await.unwrap();
    store
        .store_chunks(
            &doc_b.id,
            &one_chunk("Doc B", "beta"),
            &[ones(4)],
            "rantaiclaw_test_model_a",
        )
        .await
        .expect("re-ingest after hard delete must not collide on chunk_vec");
    assert_eq!(store.chunk_count(&doc_b.id).await.unwrap(), 1);

    // Only Doc B's vector remains — proves no orphaned chunk_vec row survived.
    let results = store
        .search_by_vector(&ones(4), 5, &SearchFilter::default())
        .await
        .unwrap();
    assert_eq!(
        results.len(),
        1,
        "stale Doc A vector must not survive hard delete"
    );
}

#[tokio::test]
async fn store_chunks_and_vector_search() {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let doc = sample_doc("rantaiclaw_doc_vec", "Vec Doc");
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
        .store_chunks(&doc.id, &chunks, &embeds, "rantaiclaw_test_model_a")
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
    let doc = sample_doc("rantaiclaw_doc_cat", "Category Doc");
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
        .store_chunks(&doc.id, &chunks, &[ones(4)], "rantaiclaw_test_model_a")
        .await
        .unwrap();

    // Query for the wrong category — must return no hits with json_each.
    // With the old LIKE substring filter, "A" would have matched "FAQ".
    let filter = SearchFilter {
        category: Some("A".into()),
        ..Default::default()
    };
    let results = store.search_by_vector(&ones(4), 5, &filter).await.unwrap();
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
    assert_eq!(
        hits.len(),
        1,
        "exact category match must still return the doc"
    );
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
    let doc = sample_doc("rantaiclaw_doc_soft", "Soft Doc");
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
        .store_chunks(&doc.id, &chunks, &[ones(4)], "rantaiclaw_test_model_a")
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
    let counts = store
        .chunk_counts(std::slice::from_ref(&doc.id))
        .await
        .unwrap();
    assert_eq!(counts.get(&doc.id).copied(), Some(0));
}

#[tokio::test]
async fn dimension_mismatch_errors_loudly() {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let doc = sample_doc("rantaiclaw_doc_mismatch", "Mismatch");
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
        .store_chunks(&doc.id, &chunks, &bad_embed, "rantaiclaw_test_model_a")
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
    let doc = sample_doc("rantaiclaw_doc_bm25", "BM Doc");
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
        .store_chunks(&doc.id, &chunks, &embeds, "rantaiclaw_test_model_a")
        .await
        .unwrap();

    let hits = store.bm25_search("fox", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].content.contains("fox"));
    // Score is negated (higher = better) so a matching hit must be > 0.
    assert!(hits[0].score > 0.0);
}

#[tokio::test]
async fn group_lifecycle_create_list_add_doc_list_docs_delete() {
    // Full group lifecycle: create -> list (count 0) -> add a doc ->
    // list_group_documents -> delete (cascades membership).
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();

    // Create.
    let group = store
        .create_group("Onboarding", Some("new-hire docs"), Some("#3366ff"))
        .await
        .unwrap();
    assert_eq!(group.name, "Onboarding");
    assert_eq!(group.description.as_deref(), Some("new-hire docs"));
    assert_eq!(group.created_at, group.updated_at);

    // get_group round-trips.
    let fetched = store
        .get_group(&group.id)
        .await
        .unwrap()
        .expect("group must exist");
    assert_eq!(fetched.id, group.id);

    // list_groups shows it with a zero document_count.
    let summaries = store.list_groups().await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, group.id);
    assert_eq!(summaries[0].document_count, 0);

    // Add a document, then attach it to the group.
    let doc = sample_doc("rantaiclaw_doc_grp", "Grouped Doc");
    store.create_document(&doc).await.unwrap();
    store
        .add_document_to_group(&doc.id.0, &group.id)
        .await
        .unwrap();
    // Idempotent: a second add must not error or double-count.
    store
        .add_document_to_group(&doc.id.0, &group.id)
        .await
        .unwrap();

    // list_group_documents returns the doc.
    let docs = store.list_group_documents(&group.id).await.unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].id, doc.id);

    // document_count reflects the single membership row.
    let summaries = store.list_groups().await.unwrap();
    assert_eq!(summaries[0].document_count, 1);

    // update_group bumps updated_at and only touches provided fields.
    store
        .update_group(&group.id, Some("Onboarding v2"), None, None)
        .await
        .unwrap();
    let updated = store.get_group(&group.id).await.unwrap().unwrap();
    assert_eq!(updated.name, "Onboarding v2");
    assert_eq!(
        updated.description.as_deref(),
        Some("new-hire docs"),
        "unspecified fields must be preserved"
    );
    assert!(updated.updated_at >= updated.created_at);

    // remove_document_from_group detaches; second remove returns false.
    assert!(store
        .remove_document_from_group(&doc.id.0, &group.id)
        .await
        .unwrap());
    assert!(!store
        .remove_document_from_group(&doc.id.0, &group.id)
        .await
        .unwrap());
    assert!(store
        .list_group_documents(&group.id)
        .await
        .unwrap()
        .is_empty());

    // Delete the group; second delete returns false.
    assert!(store.delete_group(&group.id).await.unwrap());
    assert!(!store.delete_group(&group.id).await.unwrap());
    assert!(store.get_group(&group.id).await.unwrap().is_none());
    assert!(store.list_groups().await.unwrap().is_empty());
}

#[tokio::test]
async fn delete_group_clears_membership_rows() {
    // Deleting a group must remove its document_group rows even though
    // PRAGMA foreign_keys is off (the schema cascade would not fire).
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();

    let group = store.create_group("Temp", None, None).await.unwrap();
    let doc = sample_doc("rantaiclaw_doc_grp_del", "Doc");
    store.create_document(&doc).await.unwrap();
    store
        .add_document_to_group(&doc.id.0, &group.id)
        .await
        .unwrap();

    store.delete_group(&group.id).await.unwrap();

    // Re-create a group with the same id is impossible (uuid), but the doc must
    // no longer appear in any group listing for the old id.
    let docs = store.list_group_documents(&group.id).await.unwrap();
    assert!(
        docs.is_empty(),
        "membership must be cleared on group delete"
    );
    // The document itself survives — only the membership was removed.
    assert!(store.get_document(&doc.id).await.unwrap().is_some());
}

#[tokio::test]
async fn count_by_embedding_model_aggregates() {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let doc = sample_doc("rantaiclaw_doc_drift", "Drift");
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
        .store_chunks(&doc.id, &chunks[..2], &embeds[..2], "rantaiclaw_model_a")
        .await
        .unwrap();
    store
        .store_chunks(&doc.id, &chunks[2..], &embeds[..1], "rantaiclaw_model_b")
        .await
        .unwrap();

    let mut counts = store.count_by_embedding_model().await.unwrap();
    counts.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        counts,
        vec![
            (Some("rantaiclaw_model_a".into()), 2),
            (Some("rantaiclaw_model_b".into()), 1),
        ]
    );
}
