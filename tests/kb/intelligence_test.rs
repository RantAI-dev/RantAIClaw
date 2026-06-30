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
    assert_eq!(out.entities[0].0, "NQRust");
    assert_eq!(out.relations.len(), 1);
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
                entities: vec![("NQRust".into(), EntityType::Product, 0.9)],
                relations: vec![],
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
