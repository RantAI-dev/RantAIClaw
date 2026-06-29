use rantaiclaw::kb::intelligence::extract::pattern::extract_pattern_entities;
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
