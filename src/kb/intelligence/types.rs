//! Value types for KB document intelligence — entity/relation extraction and graph.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;

// ---------------------------------------------------------------------------
// EntityType
// ---------------------------------------------------------------------------

/// Typed category of an extracted entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityType {
    Person,
    Organization,
    Location,
    Product,
    Technology,
    Concept,
    Event,
    Date,
    Email,
    Url,
    Phone,
    Money,
    Function,
    Api,
    Error,
    File,
    /// Unrecognised value from LLM output. Note: an inner string that matches a known variant name (e.g. "Person") deserializes back to that known variant, not Other — intentional for lenient fallback.
    Other(String),
}

impl EntityType {
    pub fn as_str(&self) -> Cow<'static, str> {
        match self {
            EntityType::Person => Cow::Borrowed("Person"),
            EntityType::Organization => Cow::Borrowed("Organization"),
            EntityType::Location => Cow::Borrowed("Location"),
            EntityType::Product => Cow::Borrowed("Product"),
            EntityType::Technology => Cow::Borrowed("Technology"),
            EntityType::Concept => Cow::Borrowed("Concept"),
            EntityType::Event => Cow::Borrowed("Event"),
            EntityType::Date => Cow::Borrowed("Date"),
            EntityType::Email => Cow::Borrowed("Email"),
            EntityType::Url => Cow::Borrowed("Url"),
            EntityType::Phone => Cow::Borrowed("Phone"),
            EntityType::Money => Cow::Borrowed("Money"),
            EntityType::Function => Cow::Borrowed("Function"),
            EntityType::Api => Cow::Borrowed("Api"),
            EntityType::Error => Cow::Borrowed("Error"),
            EntityType::File => Cow::Borrowed("File"),
            EntityType::Other(s) => Cow::Owned(s.clone()),
        }
    }

    pub fn from_str_lenient(s: &str) -> Self {
        match s {
            "Person" => EntityType::Person,
            "Organization" => EntityType::Organization,
            "Location" => EntityType::Location,
            "Product" => EntityType::Product,
            "Technology" => EntityType::Technology,
            "Concept" => EntityType::Concept,
            "Event" => EntityType::Event,
            "Date" => EntityType::Date,
            "Email" => EntityType::Email,
            "Url" => EntityType::Url,
            "Phone" => EntityType::Phone,
            "Money" => EntityType::Money,
            "Function" => EntityType::Function,
            "Api" => EntityType::Api,
            "Error" => EntityType::Error,
            "File" => EntityType::File,
            other => EntityType::Other(other.to_string()),
        }
    }
}

impl Serialize for EntityType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for EntityType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(EntityType::from_str_lenient(&s))
    }
}

// ---------------------------------------------------------------------------
// RelationType
// ---------------------------------------------------------------------------

/// Typed category of an extracted relation between entities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelationType {
    WorksFor,
    PartOf,
    LocatedIn,
    Implements,
    Calls,
    DependsOn,
    Uses,
    Produces,
    RelatedTo,
    /// Unrecognised value from LLM output. Note: an inner string that matches a known variant name (e.g. "Person") deserializes back to that known variant, not Other — intentional for lenient fallback.
    Other(String),
}

impl RelationType {
    pub fn as_str(&self) -> Cow<'static, str> {
        match self {
            RelationType::WorksFor => Cow::Borrowed("WorksFor"),
            RelationType::PartOf => Cow::Borrowed("PartOf"),
            RelationType::LocatedIn => Cow::Borrowed("LocatedIn"),
            RelationType::Implements => Cow::Borrowed("Implements"),
            RelationType::Calls => Cow::Borrowed("Calls"),
            RelationType::DependsOn => Cow::Borrowed("DependsOn"),
            RelationType::Uses => Cow::Borrowed("Uses"),
            RelationType::Produces => Cow::Borrowed("Produces"),
            RelationType::RelatedTo => Cow::Borrowed("RelatedTo"),
            RelationType::Other(s) => Cow::Owned(s.clone()),
        }
    }

    pub fn from_str_lenient(s: &str) -> Self {
        match s {
            "WorksFor" => RelationType::WorksFor,
            "PartOf" => RelationType::PartOf,
            "LocatedIn" => RelationType::LocatedIn,
            "Implements" => RelationType::Implements,
            "Calls" => RelationType::Calls,
            "DependsOn" => RelationType::DependsOn,
            "Uses" => RelationType::Uses,
            "Produces" => RelationType::Produces,
            "RelatedTo" => RelationType::RelatedTo,
            other => RelationType::Other(other.to_string()),
        }
    }
}

impl Serialize for RelationType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for RelationType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(RelationType::from_str_lenient(&s))
    }
}

// ---------------------------------------------------------------------------
// ExtractSource
// ---------------------------------------------------------------------------

/// How an entity or relation was extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractSource {
    Llm,
    Pattern,
}

impl ExtractSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ExtractSource::Llm => "llm",
            ExtractSource::Pattern => "pattern",
        }
    }
}

// ---------------------------------------------------------------------------
// Graph value types
// ---------------------------------------------------------------------------

/// A canonical entity in the knowledge graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub id: String,
    pub canonical_key: String,
    pub name: String,
    pub entity_type: EntityType,
    pub confidence: f32,
    pub metadata: serde_json::Value,
}

/// A mention of an entity in a specific document/chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityMention {
    pub id: String,
    pub entity_id: String,
    pub document_id: String,
    pub chunk_index: Option<i64>,
    pub context: Option<String>,
    pub source: ExtractSource,
}

/// A directed relation between two entities.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub id: String,
    pub source_entity_id: String,
    pub target_entity_id: String,
    pub relation_type: RelationType,
    pub confidence: f32,
    pub document_id: String,
    pub metadata: serde_json::Value,
}
