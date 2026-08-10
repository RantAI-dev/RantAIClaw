use rantaiclaw::kb::intelligence::extract::pattern::extract_pattern_entities;
use rantaiclaw::kb::intelligence::types::{EntityType, ExtractSource, RelationType};

#[tokio::test]
async fn llm_extractor_parses_entities_and_relations_from_chat() {
    use rantaiclaw::kb::intelligence::extract::llm::CombinedLlmExtractor;
    use rantaiclaw::kb::intelligence::extract::EntityRelationExtractor;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let content = r#"{"entities":[{"name":"NQRust","type":"Product","confidence":0.9}],
        "relations":[{"source":"NQRust","target":"NexusQuantum","type":"PartOf","confidence":0.8}]}"#;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content": content}}]})))
        .expect(1)
        .mount(&server)
        .await;

    let ext = CombinedLlmExtractor::new(
        "test-model".into(),
        format!("{}/chat", server.uri()),
        "test-key".into(),
    );
    let out = ext
        .extract(&["NQRust is part of NexusQuantum."])
        .await
        .unwrap();
    assert_eq!(out.entities.len(), 1);
    // Tuple is (chunk_index, name, type, confidence) since plan 093.
    assert_eq!(out.entities[0].0, 0, "single-chunk input -> chunk_index 0");
    assert_eq!(out.entities[0].1, "NQRust");
    assert_eq!(out.relations.len(), 1);
}

#[tokio::test]
async fn llm_extractor_sanitizes_zero_confidence_to_nonzero() {
    use rantaiclaw::kb::intelligence::extract::llm::CombinedLlmExtractor;
    use rantaiclaw::kb::intelligence::extract::EntityRelationExtractor;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    // Regression: a model that echoes a 0.0 confidence must NOT surface as 0% in the graph.
    let content = r#"{"entities":[{"name":"NQRust","type":"Product","confidence":0.0}],
        "relations":[{"source":"NQRust","target":"NexusQuantum","type":"PartOf","confidence":0.0}]}"#;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content": content}}]})))
        .expect(1)
        .mount(&server)
        .await;

    let ext = CombinedLlmExtractor::new(
        "test-model".into(),
        format!("{}/chat", server.uri()),
        "test-key".into(),
    );
    let out = ext
        .extract(&["NQRust is part of NexusQuantum."])
        .await
        .unwrap();
    assert_eq!(out.entities.len(), 1);
    assert!(
        out.entities[0].3 > 0.0,
        "entity confidence must be sanitised above 0"
    );
    assert_eq!(out.relations.len(), 1);
    assert!(
        out.relations[0].3 > 0.0,
        "relation confidence must be sanitised above 0"
    );
}

#[test]
fn entity_type_serde_roundtrips_and_falls_back() {
    assert_eq!(
        serde_json::to_string(&EntityType::Person).unwrap(),
        "\"Person\""
    );
    assert_eq!(
        serde_json::from_str::<EntityType>("\"Person\"").unwrap(),
        EntityType::Person
    );
    assert_eq!(
        serde_json::from_str::<RelationType>("\"WorksFor\"").unwrap(),
        RelationType::WorksFor
    );
    let parsed: EntityType = serde_json::from_str("\"Spaceship\"").unwrap();
    assert_eq!(parsed, EntityType::Other("Spaceship".into()));
    let r: RelationType = serde_json::from_str("\"FOUNDED_BY\"").unwrap();
    assert_eq!(r, RelationType::Other("FOUNDED_BY".into()));
    assert_eq!(ExtractSource::Pattern.as_str(), "pattern");
}

#[test]
fn pattern_extractor_finds_high_precision_entities() {
    let text = "Contact ops@rantaiclaw.dev or see https://nexusquantum.id for the NQRust API.";
    let ents = extract_pattern_entities(text);
    let by_type = |t: EntityType| ents.iter().any(|(n, ty)| *ty == t && !n.is_empty());
    assert!(by_type(EntityType::Email), "email not found: {ents:?}");
    assert!(by_type(EntityType::Url), "url not found: {ents:?}");
    // No email/url in this one.
    assert!(extract_pattern_entities("plain prose with no markers").is_empty());
}

#[test]
fn canonical_key_merges_same_entity_across_casing_and_whitespace() {
    use rantaiclaw::kb::intelligence::resolve::canonical_key;
    let a = canonical_key("NQRust", &EntityType::Product);
    let b = canonical_key("  nqrust ", &EntityType::Product);
    assert_eq!(a, b, "same name+type must share one canonical key");
    // Different type → different node.
    assert_ne!(a, canonical_key("NQRust", &EntityType::Organization));
}

#[tokio::test]
async fn upsert_entity_merges_by_canonical_key_across_documents() {
    use rantaiclaw::kb::intelligence::types::{Entity, EntityMention};
    use rantaiclaw::kb::store::sqlite::SqliteStore;
    use rantaiclaw::kb::store::IntelligenceStore;
    use tempfile::TempDir;

    fn ent(key: &str, name: &str) -> Entity {
        Entity {
            id: format!("e_{key}"),
            canonical_key: key.into(),
            name: name.into(),
            entity_type: EntityType::Product,
            confidence: 0.9,
            metadata: serde_json::json!({}),
        }
    }

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();

    let id1 = store
        .upsert_entity(&ent("nqrust:Product", "NQRust"))
        .await
        .unwrap();
    let id2 = store
        .upsert_entity(&ent("nqrust:Product", "NQRust"))
        .await
        .unwrap();
    assert_eq!(
        id1, id2,
        "same canonical_key must resolve to one entity row"
    );

    store
        .add_mention(&EntityMention {
            id: "m1".into(),
            entity_id: id1.clone(),
            document_id: "d1".into(),
            chunk_index: Some(0),
            context: Some("x".into()),
            source: ExtractSource::Llm,
        })
        .await
        .unwrap();
    store
        .add_mention(&EntityMention {
            id: "m2".into(),
            entity_id: id2.clone(),
            document_id: "d2".into(),
            chunk_index: Some(1),
            context: None,
            source: ExtractSource::Pattern,
        })
        .await
        .unwrap();

    let graph = store.graph(None, 100).await.unwrap();
    assert_eq!(graph.nodes.len(), 1, "one global node");
    assert_eq!(graph.nodes[0].doc_count, 2, "merged across two documents");

    store.delete_document_intelligence("d1").await.unwrap();
    assert_eq!(store.graph(None, 100).await.unwrap().nodes[0].doc_count, 1);
    store.delete_document_intelligence("d2").await.unwrap();
    assert!(
        store.graph(None, 100).await.unwrap().nodes.is_empty(),
        "orphan entity GC'd"
    );
}

#[tokio::test]
async fn orchestration_merges_same_entity_across_two_documents() {
    use async_trait::async_trait;
    use rantaiclaw::kb::intelligence::extract::{EntityRelationExtractor, Extracted};
    use rantaiclaw::kb::intelligence::extract_document_intelligence;
    use rantaiclaw::kb::intelligence::types::EntityType;
    use rantaiclaw::kb::store::{sqlite::SqliteStore, IntelligenceStore};
    use tempfile::TempDir;

    struct CannedExtractor;
    #[async_trait]
    impl EntityRelationExtractor for CannedExtractor {
        async fn extract(&self, _c: &[&str]) -> rantaiclaw::kb::KbResult<Extracted> {
            Ok(Extracted {
                entities: vec![(0, "NQRust".into(), EntityType::Product, 0.9)],
                relations: vec![],
                ..Default::default()
            })
        }
    }

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let ext = CannedExtractor;
    extract_document_intelligence(&store, &ext, "d1", &["NQRust doc one"], "exact")
        .await
        .unwrap();
    extract_document_intelligence(&store, &ext, "d2", &["NQRust doc two"], "exact")
        .await
        .unwrap();
    let g = store.graph(None, 100).await.unwrap();
    assert_eq!(g.nodes.len(), 1, "one global node across two docs");
    assert_eq!(g.nodes[0].doc_count, 2);
}

#[tokio::test]
async fn graph_expand_chunks_surfaces_neighbor_only_chunks() {
    // Plan 093: drives the PRODUCTION orchestrator (extract_document_intelligence)
    // instead of hand-seeding mention rows. The previous version seeded
    // `ExtractSource::Llm` mentions WITH `chunk_index: Some(_)` — a row shape
    // the real extractor never produced (it wrote NULL), which is exactly how
    // the NULL-join defect stayed green in CI.
    use async_trait::async_trait;
    use chrono::Utc;
    use rantaiclaw::kb::intelligence::extract::{EntityRelationExtractor, Extracted};
    use rantaiclaw::kb::intelligence::extract_document_intelligence;
    use rantaiclaw::kb::store::sqlite::SqliteStore;
    use rantaiclaw::kb::store::{IntelligenceStore, KbStore};
    use rantaiclaw::kb::{Chunk, ChunkMetadata, Document, DocumentId};
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();

    // One document, two chunks. Chunk 0 mentions Alice + TechCorp; chunk 1
    // mentions only TechCorp.
    let doc = Document {
        id: DocumentId("d_graphrag".into()),
        title: "GraphRAG Doc".into(),
        content: "body".into(),
        categories: vec!["FAQ".into()],
        subcategory: None,
        metadata: serde_json::json!({}),
        s3_key: None,
        file_type: None,
        mime_type: None,
        file_size: None,
        organization_id: None,
        created_by: None,
        session_id: None,
        artifact_type: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        deleted_at: None,
        retention_days: None,
        retrieval_count: 0,
        last_retrieved_at: None,
    };
    store.create_document(&doc).await.unwrap();
    let chunks = vec![
        Chunk {
            content: "Alice works at TechCorp.".into(),
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
            content: "TechCorp builds embedded systems.".into(),
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
    store
        .store_chunks(
            &doc.id,
            &chunks,
            &[vec![1.0; 4], vec![1.0; 4]],
            "test_model",
        )
        .await
        .unwrap();

    // Stub LLM extractor emitting what a real model would for those chunks:
    // Alice in chunk 0, TechCorp in chunks 0 and 1, one WorksFor relation.
    struct CannedExtractor;
    #[async_trait]
    impl EntityRelationExtractor for CannedExtractor {
        async fn extract(&self, _c: &[&str]) -> rantaiclaw::kb::KbResult<Extracted> {
            Ok(Extracted {
                entities: vec![
                    (0, "Alice".into(), EntityType::Person, 0.9),
                    (0, "TechCorp".into(), EntityType::Organization, 0.95),
                    (1, "TechCorp".into(), EntityType::Organization, 0.95),
                ],
                relations: vec![(
                    "Alice".into(),
                    "TechCorp".into(),
                    RelationType::WorksFor,
                    0.85,
                )],
                ..Default::default()
            })
        }
    }
    extract_document_intelligence(
        &store,
        &CannedExtractor,
        "d_graphrag",
        &[
            "Alice works at TechCorp.",
            "TechCorp builds embedded systems.",
        ],
        "exact",
    )
    .await
    .unwrap();

    // Query names only "Alice" → seed Alice → 1-hop neighbour TechCorp.
    let got = store
        .graph_expand_chunks("What is Alice's role?", 10, 10)
        .await
        .unwrap();
    let contents: Vec<String> = got.iter().map(|c| c.content.clone()).collect();
    assert!(
        contents
            .iter()
            .any(|c| c.contains("Alice works at TechCorp")),
        "seed-entity chunk missing: {contents:?}"
    );
    assert!(
        contents
            .iter()
            .any(|c| c.contains("TechCorp builds embedded systems")),
        "neighbour-only chunk (reachable only via the Alice->TechCorp edge) missing: {contents:?}"
    );

    // A query naming no entity expands to nothing.
    let none = store
        .graph_expand_chunks("totally unrelated zzz", 10, 10)
        .await
        .unwrap();
    assert!(
        none.is_empty(),
        "no entity match must yield no graph chunks: {none:?}"
    );
}

#[tokio::test]
async fn llm_mentions_always_carry_a_chunk_index() {
    // Plan 093 invariant: the SQL join in graph_expand_chunks depends on
    // every LLM mention carrying its chunk index. A NULL here silently
    // removes the entity from retrieval — pin it where a refactor of the
    // orchestrator will trip over it.
    use async_trait::async_trait;
    use rantaiclaw::kb::intelligence::extract::{EntityRelationExtractor, Extracted};
    use rantaiclaw::kb::intelligence::extract_document_intelligence;
    use rantaiclaw::kb::store::sqlite::SqliteStore;
    use tempfile::TempDir;

    struct CannedExtractor;
    #[async_trait]
    impl EntityRelationExtractor for CannedExtractor {
        async fn extract(&self, _c: &[&str]) -> rantaiclaw::kb::KbResult<Extracted> {
            Ok(Extracted {
                entities: vec![
                    (0, "Alpha".into(), EntityType::Concept, 0.9),
                    (1, "Beta".into(), EntityType::Concept, 0.9),
                ],
                relations: vec![],
                ..Default::default()
            })
        }
    }

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    extract_document_intelligence(&store, &CannedExtractor, "d1", &["c0", "c1"], "exact")
        .await
        .unwrap();

    // Inspect the raw mention rows: none from the LLM source may be NULL.
    let conn = rusqlite::Connection::open(tmp.path().join("kb.db")).unwrap();
    let null_llm: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entity_mention WHERE source = 'llm' AND chunk_index IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let total_llm: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entity_mention WHERE source = 'llm'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(total_llm >= 2, "expected llm mentions to be stored");
    assert_eq!(
        null_llm, 0,
        "an LLM mention with a NULL chunk_index is invisible to GraphRAG's chunk join"
    );
}

#[tokio::test]
async fn upsert_entity_refreshes_confidence_to_max_on_conflict() {
    use rantaiclaw::kb::intelligence::types::{Entity, EntityMention};
    use rantaiclaw::kb::store::sqlite::SqliteStore;
    use rantaiclaw::kb::store::IntelligenceStore;
    use tempfile::TempDir;

    fn ent(conf: f32) -> Entity {
        Entity {
            id: "e1".into(),
            canonical_key: "nqrust:Product".into(),
            name: "NQRust".into(),
            entity_type: EntityType::Product,
            confidence: conf,
            metadata: serde_json::json!({}),
        }
    }

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();

    // A first extraction stored a stale 0.0 (mimics the pre-fix binary).
    let id = store.upsert_entity(&ent(0.0)).await.unwrap();
    // A re-extract with real confidence must LIFT it, not be silently dropped.
    store.upsert_entity(&ent(0.95)).await.unwrap();
    // A later lower value must NOT lower it (max wins).
    store.upsert_entity(&ent(0.5)).await.unwrap();

    store
        .add_mention(&EntityMention {
            id: "m1".into(),
            entity_id: id,
            document_id: "d1".into(),
            chunk_index: Some(0),
            context: None,
            source: ExtractSource::Llm,
        })
        .await
        .unwrap();

    let (entities, _) = store.intelligence_for_document("d1").await.unwrap();
    assert_eq!(entities.len(), 1);
    assert!(
        (entities[0].confidence - 0.95).abs() < 1e-6,
        "confidence must refresh to the max (0.95), got {}",
        entities[0].confidence
    );
}

#[tokio::test]
async fn hard_delete_clears_intelligence_soft_delete_keeps_it() {
    use chrono::Utc;
    use rantaiclaw::kb::intelligence::types::{Entity, EntityMention};
    use rantaiclaw::kb::store::sqlite::SqliteStore;
    use rantaiclaw::kb::store::{IntelligenceStore, KbStore};
    use rantaiclaw::kb::{Document, DocumentId};
    use tempfile::TempDir;

    fn doc(id: &str) -> Document {
        Document {
            id: DocumentId(id.into()),
            title: "T".into(),
            content: "c".into(),
            categories: vec![],
            subcategory: None,
            metadata: serde_json::json!({}),
            s3_key: None,
            file_type: None,
            mime_type: None,
            file_size: None,
            organization_id: None,
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
    async fn seed_entity(store: &SqliteStore, doc_id: &str) {
        let id = store
            .upsert_entity(&Entity {
                id: "e".into(),
                canonical_key: "x:Product".into(),
                name: "X".into(),
                entity_type: EntityType::Product,
                confidence: 0.9,
                metadata: serde_json::json!({}),
            })
            .await
            .unwrap();
        store
            .add_mention(&EntityMention {
                id: "m".into(),
                entity_id: id,
                document_id: doc_id.into(),
                chunk_index: Some(0),
                context: None,
                source: ExtractSource::Llm,
            })
            .await
            .unwrap();
    }

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();

    // Hard delete must clear the doc's graph rows + GC the orphaned entity.
    store.create_document(&doc("d_hard")).await.unwrap();
    seed_entity(&store, "d_hard").await;
    assert_eq!(store.graph(None, 100).await.unwrap().nodes.len(), 1);
    store
        .delete_document(&DocumentId("d_hard".into()), false)
        .await
        .unwrap();
    assert!(
        store.graph(None, 100).await.unwrap().nodes.is_empty(),
        "hard delete must clear the document's intelligence"
    );

    // Soft delete must KEEP intelligence (the doc is recoverable).
    store.create_document(&doc("d_soft")).await.unwrap();
    seed_entity(&store, "d_soft").await;
    store
        .delete_document(&DocumentId("d_soft".into()), true)
        .await
        .unwrap();
    assert_eq!(
        store.graph(None, 100).await.unwrap().nodes.len(),
        1,
        "soft delete must preserve the document's intelligence"
    );
}

#[tokio::test]
async fn store_intelligence_persists_all() {
    use rantaiclaw::kb::intelligence::types::{Entity, EntityMention, Relation};
    use rantaiclaw::kb::store::sqlite::SqliteStore;
    use rantaiclaw::kb::store::IntelligenceStore;
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();

    let alice = Entity {
        id: "e_alice".into(),
        canonical_key: "alice:Person".into(),
        name: "Alice".into(),
        entity_type: EntityType::Person,
        confidence: 0.9,
        metadata: serde_json::json!({}),
    };
    let corp = Entity {
        id: "e_corp".into(),
        canonical_key: "techcorp:Organization".into(),
        name: "TechCorp".into(),
        entity_type: EntityType::Organization,
        confidence: 0.95,
        metadata: serde_json::json!({}),
    };
    let mentions = vec![
        EntityMention {
            id: "m1".into(),
            entity_id: alice.id.clone(),
            document_id: "d1".into(),
            chunk_index: Some(0),
            context: None,
            source: ExtractSource::Llm,
        },
        EntityMention {
            id: "m2".into(),
            entity_id: corp.id.clone(),
            document_id: "d1".into(),
            chunk_index: Some(0),
            context: None,
            source: ExtractSource::Llm,
        },
    ];
    let relations = vec![Relation {
        id: "r1".into(),
        source_entity_id: alice.id.clone(),
        target_entity_id: corp.id.clone(),
        relation_type: RelationType::WorksFor,
        confidence: 0.85,
        document_id: "d1".into(),
        metadata: serde_json::json!({}),
    }];

    store
        .store_intelligence("d1", &[alice.clone(), corp.clone()], &mentions, &relations)
        .await
        .unwrap();

    let (entities, got_relations) = store.intelligence_for_document("d1").await.unwrap();
    assert_eq!(entities.len(), 2, "both entities landed");
    assert_eq!(got_relations.len(), 1, "the relation landed");
    assert_eq!(got_relations[0].source_entity_id, alice.id);
    assert_eq!(got_relations[0].target_entity_id, corp.id);

    let graph = store.graph(None, 100).await.unwrap();
    assert_eq!(graph.nodes.len(), 2, "both entities are graph nodes");
    assert!(
        graph.nodes.iter().all(|n| n.doc_count == 1),
        "each entity mentioned in exactly one document"
    );
}

#[tokio::test]
async fn store_intelligence_cross_document_merge() {
    use rantaiclaw::kb::intelligence::types::{Entity, EntityMention, Relation};
    use rantaiclaw::kb::store::sqlite::SqliteStore;
    use rantaiclaw::kb::store::IntelligenceStore;
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();

    // Doc A: "Acme" entity, mentioned once, with its own provisional id.
    let acme_a = Entity {
        id: "provA_acme".into(),
        canonical_key: "acme:Organization".into(),
        name: "Acme".into(),
        entity_type: EntityType::Organization,
        confidence: 0.9,
        metadata: serde_json::json!({}),
    };
    store
        .store_intelligence(
            "docA",
            &[acme_a.clone()],
            &[EntityMention {
                id: "mA".into(),
                entity_id: acme_a.id.clone(),
                document_id: "docA".into(),
                chunk_index: Some(0),
                context: None,
                source: ExtractSource::Llm,
            }],
            &[],
        )
        .await
        .unwrap();

    let (entities_a, _) = store.intelligence_for_document("docA").await.unwrap();
    assert_eq!(entities_a.len(), 1);
    // Nothing collided yet, so the surviving id equals the provisional one.
    let surviving_id = entities_a[0].id.clone();
    assert_eq!(surviving_id, acme_a.id);

    // Doc B: SAME canonical_key as Acme but a FRESH, DIFFERENT provisional id
    // (mimics a second document's independent extraction), plus a distinct
    // "Beta" entity and a relation wired against Acme's PROVISIONAL id.
    let acme_b = Entity {
        id: "provB_acme".into(),
        canonical_key: "acme:Organization".into(),
        ..acme_a.clone()
    };
    assert_ne!(
        acme_b.id, acme_a.id,
        "doc B must use a fresh provisional id"
    );
    let beta = Entity {
        id: "provB_beta".into(),
        canonical_key: "beta:Organization".into(),
        name: "Beta".into(),
        entity_type: EntityType::Organization,
        confidence: 0.8,
        metadata: serde_json::json!({}),
    };
    store
        .store_intelligence(
            "docB",
            &[acme_b.clone(), beta.clone()],
            &[
                EntityMention {
                    id: "mB1".into(),
                    entity_id: acme_b.id.clone(),
                    document_id: "docB".into(),
                    chunk_index: Some(0),
                    context: None,
                    source: ExtractSource::Llm,
                },
                EntityMention {
                    id: "mB2".into(),
                    entity_id: beta.id.clone(),
                    document_id: "docB".into(),
                    chunk_index: Some(0),
                    context: None,
                    source: ExtractSource::Llm,
                },
            ],
            &[Relation {
                id: "rB".into(),
                source_entity_id: acme_b.id.clone(),
                target_entity_id: beta.id.clone(),
                relation_type: RelationType::WorksFor,
                confidence: 0.7,
                document_id: "docB".into(),
                metadata: serde_json::json!({}),
            }],
        )
        .await
        .unwrap();

    // REGRESSION GUARD: without the in-transaction remap, docB's mention/
    // relation would still carry the provisional id "provB_acme", which was
    // never inserted as its own entity row (ON CONFLICT kept docA's row) —
    // the mention would orphan (drop out of the JOIN) and the relation would
    // carry a dangling source_entity_id.
    let (entities_b, relations_b) = store.intelligence_for_document("docB").await.unwrap();
    assert_eq!(entities_b.len(), 2, "docB must see Acme (merged) + Beta");
    assert!(
        entities_b.iter().any(|e| e.id == surviving_id),
        "docB's Acme mention must resolve to docA's surviving id {surviving_id}, not the \
         provisional id {}; entities_b={entities_b:?}",
        acme_b.id
    );
    assert_eq!(relations_b.len(), 1);
    assert_eq!(
        relations_b[0].source_entity_id, surviving_id,
        "relation source must remap through to the surviving id, not the provisional id"
    );
    let beta_surviving_id = entities_b
        .iter()
        .find(|e| e.name == "Beta")
        .expect("Beta entity present")
        .id
        .clone();
    assert_eq!(relations_b[0].target_entity_id, beta_surviving_id);

    // Global graph view: exactly 2 merged nodes; Acme's doc_count reflects
    // BOTH documents, proving the merge (not a duplicate node per document).
    let graph = store.graph(None, 100).await.unwrap();
    assert_eq!(
        graph.nodes.len(),
        2,
        "one merged Acme node + one Beta node, not 3"
    );
    let acme_node = graph
        .nodes
        .iter()
        .find(|n| n.id == surviving_id)
        .expect("surviving Acme node present");
    assert_eq!(acme_node.doc_count, 2, "Acme merged across docA + docB");
}

#[tokio::test]
async fn store_intelligence_reingest_idempotent() {
    use async_trait::async_trait;
    use rantaiclaw::kb::intelligence::extract::{EntityRelationExtractor, Extracted};
    use rantaiclaw::kb::intelligence::extract_document_intelligence;
    use rantaiclaw::kb::store::{sqlite::SqliteStore, IntelligenceStore};
    use tempfile::TempDir;

    struct CannedExtractor;
    #[async_trait]
    impl EntityRelationExtractor for CannedExtractor {
        async fn extract(&self, _c: &[&str]) -> rantaiclaw::kb::KbResult<Extracted> {
            Ok(Extracted {
                entities: vec![
                    (0, "NQRust".into(), EntityType::Product, 0.9),
                    (0, "NexusQuantum".into(), EntityType::Organization, 0.85),
                ],
                relations: vec![(
                    "NQRust".into(),
                    "NexusQuantum".into(),
                    RelationType::PartOf,
                    0.8,
                )],
                ..Default::default()
            })
        }
    }

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let ext = CannedExtractor;

    let summary1 = extract_document_intelligence(&store, &ext, "d1", &["chunk one"], "exact")
        .await
        .unwrap();
    let summary2 = extract_document_intelligence(&store, &ext, "d1", &["chunk one"], "exact")
        .await
        .unwrap();

    assert_eq!(
        summary1.entities, summary2.entities,
        "IntelligenceSummary per-iteration entity count must stay stable across a re-ingest"
    );
    assert_eq!(
        summary1.relations, summary2.relations,
        "IntelligenceSummary relation count must stay stable across a re-ingest"
    );

    let (entities, relations) = store.intelligence_for_document("d1").await.unwrap();
    assert_eq!(entities.len(), 2, "no duplicate entities after re-ingest");
    assert_eq!(relations.len(), 1, "no duplicate relations after re-ingest");

    let graph = store.graph(None, 100).await.unwrap();
    assert_eq!(graph.nodes.len(), 2, "no duplicate graph nodes");
    assert!(
        graph.nodes.iter().all(|n| n.doc_count == 1),
        "re-ingesting the SAME document must not double its doc_count: {:?}",
        graph.nodes
    );
}

#[tokio::test]
async fn relations_survive_entity_name_case_mismatch() {
    // Plan 094: entity dedup lowercases via canonical_key, but relation
    // wiring matched raw names — "techcorp" in a relation vs "TechCorp" in
    // the entity list silently deleted the edge, with no counter and no log.
    use async_trait::async_trait;
    use rantaiclaw::kb::intelligence::extract::{EntityRelationExtractor, Extracted};
    use rantaiclaw::kb::intelligence::extract_document_intelligence;
    use rantaiclaw::kb::store::sqlite::SqliteStore;
    use rantaiclaw::kb::store::IntelligenceStore;
    use tempfile::TempDir;

    struct CannedExtractor;
    #[async_trait]
    impl EntityRelationExtractor for CannedExtractor {
        async fn extract(&self, _c: &[&str]) -> rantaiclaw::kb::KbResult<Extracted> {
            Ok(Extracted {
                entities: vec![
                    (0, "Alice".into(), EntityType::Person, 0.9),
                    (0, "TechCorp".into(), EntityType::Organization, 0.95),
                ],
                // The model refers to the same entities with different casing
                // and stray punctuation in its relations array — the common
                // real-world shape this plan fixes.
                relations: vec![(
                    "alice".into(),
                    "techcorp.".into(),
                    RelationType::WorksFor,
                    0.85,
                )],
                ..Default::default()
            })
        }
    }

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let summary =
        extract_document_intelligence(&store, &CannedExtractor, "d_case", &["c0"], "exact")
            .await
            .unwrap();
    assert_eq!(
        summary.relations, 1,
        "a casing/punctuation mismatch between entity and relation names must not drop the edge"
    );
    let (_entities, relations) = store.intelligence_for_document("d_case").await.unwrap();
    assert_eq!(relations.len(), 1, "the relation row must be stored");
}

#[tokio::test]
async fn summary_counts_deduped_entities_not_raw_extractions() {
    // Plan 095: the same entity in three chunks is three extractions and ONE
    // stored row — the summary must report 1, matching the Entities tab.
    use async_trait::async_trait;
    use rantaiclaw::kb::intelligence::extract::{EntityRelationExtractor, Extracted};
    use rantaiclaw::kb::intelligence::extract_document_intelligence;
    use rantaiclaw::kb::store::sqlite::SqliteStore;
    use rantaiclaw::kb::store::IntelligenceStore;
    use tempfile::TempDir;

    struct CannedExtractor;
    #[async_trait]
    impl EntityRelationExtractor for CannedExtractor {
        async fn extract(&self, _c: &[&str]) -> rantaiclaw::kb::KbResult<Extracted> {
            Ok(Extracted {
                entities: vec![
                    (0, "TechCorp".into(), EntityType::Organization, 0.9),
                    (1, "TechCorp".into(), EntityType::Organization, 0.9),
                    (2, "techcorp".into(), EntityType::Organization, 0.9),
                ],
                relations: vec![],
                ..Default::default()
            })
        }
    }

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();
    let summary = extract_document_intelligence(
        &store,
        &CannedExtractor,
        "d_dedup",
        &["c0", "c1", "c2"],
        "exact",
    )
    .await
    .unwrap();
    assert_eq!(
        summary.entities, 1,
        "summary must count distinct canonical keys, not raw extractions"
    );
    let (entities, _relations) = store.intelligence_for_document("d_dedup").await.unwrap();
    assert_eq!(
        entities.len(),
        summary.entities,
        "summary must equal what the store actually holds"
    );
}

#[tokio::test]
async fn graph_node_selection_uses_deduped_degree() {
    // Plan 096: an entity with 5 duplicate relation rows for the SAME
    // (target, type) is ONE deduplicated edge; an entity with 2 genuinely
    // distinct edges is better connected by the metric the UI shows. Under
    // limit=1 the distinct-edge entity must win the slot — before the fix
    // selection ordered by raw relation-row count and the duplicate-heavy
    // entity won with degree 5, then rendered as degree 1.
    use rantaiclaw::kb::intelligence::types::{Entity, EntityMention, ExtractSource, Relation};
    use rantaiclaw::kb::store::sqlite::SqliteStore;
    use rantaiclaw::kb::store::IntelligenceStore;
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(tmp.path().join("kb.db"), 4)
        .await
        .unwrap();

    let ent = |id: &str, name: &str| Entity {
        id: id.into(),
        canonical_key: format!("{}:Concept", name.to_lowercase()),
        name: name.into(),
        entity_type: EntityType::Concept,
        confidence: 0.9,
        metadata: serde_json::json!({}),
    };
    let mention = |eid: &str| EntityMention {
        id: uuid::Uuid::new_v4().to_string(),
        entity_id: eid.into(),
        document_id: "d1".into(),
        chunk_index: Some(0),
        context: None,
        source: ExtractSource::Llm,
    };
    let rel = |src: &str, tgt: &str| Relation {
        id: uuid::Uuid::new_v4().to_string(),
        source_entity_id: src.into(),
        target_entity_id: tgt.into(),
        relation_type: RelationType::RelatedTo,
        confidence: 0.8,
        document_id: "d1".into(),
        metadata: serde_json::json!({}),
    };

    // dup_hub: 5 identical (dup_hub -> sink, RelatedTo) rows = 1 deduped edge.
    // true_hub: 2 distinct edges (-> t1, -> t2) = deduped degree 2.
    let entities = vec![
        ent("e_dup", "DupHub"),
        ent("e_sink", "Sink"),
        ent("e_true", "TrueHub"),
        ent("e_t1", "TargetOne"),
        ent("e_t2", "TargetTwo"),
    ];
    let mentions: Vec<EntityMention> = ["e_dup", "e_sink", "e_true", "e_t1", "e_t2"]
        .iter()
        .map(|e| mention(e))
        .collect();
    let relations = vec![
        rel("e_dup", "e_sink"),
        rel("e_dup", "e_sink"),
        rel("e_dup", "e_sink"),
        rel("e_dup", "e_sink"),
        rel("e_dup", "e_sink"),
        rel("e_true", "e_t1"),
        rel("e_true", "e_t2"),
    ];
    store
        .store_intelligence("d1", &entities, &mentions, &relations)
        .await
        .unwrap();

    let g = store.graph(None, 1).await.unwrap();
    assert_eq!(g.nodes.len(), 1);
    assert_eq!(
        g.nodes[0].name, "TrueHub",
        "the top-1 slot must go to the entity with the most DEDUPED edges, got: {:?}",
        g.nodes[0]
    );
    // Rendered degree is within-the-returned-subgraph (edges need BOTH
    // endpoints in the node set — pinned by
    // graph_dedupes_edges_weights_and_recomputes_degree). With limit=1 the
    // targets fall outside the view, so the displayed degree is 0; the
    // SELECTION still ran on deduped degree 2 vs 1, which is what this test
    // pins.
    assert_eq!(
        g.nodes[0].degree, 0,
        "within-view degree with no co-selected neighbours"
    );
}

#[tokio::test]
async fn extractor_counts_failed_chunks_on_upstream_401() {
    // Plan 109: every failure mode used to `continue` and return
    // Ok(Extracted::default()) — a total failure was indistinguishable from
    // "this document has no entities".
    use rantaiclaw::kb::intelligence::extract::llm::CombinedLlmExtractor;
    use rantaiclaw::kb::intelligence::extract::EntityRelationExtractor;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let ext = CombinedLlmExtractor::new(
        "test-model".into(),
        server.uri(),
        "rantaiclaw_test_key".into(),
    );
    let out = ext
        .extract(&["chunk a", "chunk b", "chunk c"])
        .await
        .unwrap();
    assert_eq!(out.entities.len(), 0);
    assert_eq!(
        out.failed_chunks, 3,
        "every chunk failed and must be counted"
    );
    let reason = out.first_error.expect("reason recorded");
    assert_eq!(reason, "http 401");
    // Never the upstream body or a credential.
    assert!(!reason.contains("rantaiclaw_test_key"));
}

#[tokio::test]
async fn extractor_reports_zero_failures_on_success() {
    // Control: an over-eager fix must not turn every extraction into an
    // error.
    use rantaiclaw::kb::intelligence::extract::llm::CombinedLlmExtractor;
    use rantaiclaw::kb::intelligence::extract::EntityRelationExtractor;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let content =
        r#"{"entities":[{"name":"NQRust","type":"Product","confidence":0.9}],"relations":[]}"#;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices":[{"message":{"content": content}}]})))
        .mount(&server)
        .await;

    let ext = CombinedLlmExtractor::new(
        "test-model".into(),
        server.uri(),
        "rantaiclaw_test_key".into(),
    );
    let out = ext.extract(&["chunk a", "chunk b"]).await.unwrap();
    assert_eq!(out.entities.len(), 2);
    assert_eq!(out.failed_chunks, 0, "successes must not count as failures");
    assert!(out.first_error.is_none());
}

#[tokio::test]
async fn extractor_counts_partial_failures() {
    // Mixed: first call 500s, the second succeeds (wiremock consumes the
    // 1-shot mount first).
    use rantaiclaw::kb::intelligence::extract::llm::CombinedLlmExtractor;
    use rantaiclaw::kb::intelligence::extract::EntityRelationExtractor;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let content =
        r#"{"entities":[{"name":"NQRust","type":"Product","confidence":0.9}],"relations":[]}"#;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices":[{"message":{"content": content}}]})))
        .mount(&server)
        .await;

    let ext = CombinedLlmExtractor::new(
        "test-model".into(),
        server.uri(),
        "rantaiclaw_test_key".into(),
    );
    let out = ext.extract(&["chunk a", "chunk b"]).await.unwrap();
    assert_eq!(out.failed_chunks, 1, "exactly the failed chunk counts");
    assert_eq!(out.entities.len(), 1, "the surviving chunk still extracts");
    assert_eq!(out.first_error.as_deref(), Some("http 500"));
}
