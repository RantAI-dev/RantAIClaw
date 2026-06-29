use rantaiclaw::kb::intelligence::types::{EntityType, ExtractSource, RelationType};

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
