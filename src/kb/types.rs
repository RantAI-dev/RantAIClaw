use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: DocumentId,
    pub title: String,
    pub content: String, // denormalized full text
    pub categories: Vec<String>,
    pub subcategory: Option<String>,
    pub metadata: serde_json::Value,
    pub s3_key: Option<String>,
    pub file_type: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<u64>,
    pub organization_id: Option<String>,
    pub created_by: Option<String>,
    pub session_id: Option<String>,
    pub artifact_type: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub retention_days: Option<i32>,
    pub retrieval_count: i64,
    pub last_retrieved_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMetadata {
    pub document_title: String,
    pub category: String,
    pub subcategory: Option<String>,
    pub section: Option<String>,
    pub chunk_index: usize,
    pub contextual_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub content: String,
    pub metadata: ChunkMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: ChunkId,
    pub document_id: DocumentId,
    pub document_title: String,
    pub content: String,
    pub categories: Vec<String>,
    pub subcategory: Option<String>,
    pub section: Option<String>,
    pub similarity: f32,
    pub contextual_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentGroup {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub color: Option<String>,
    pub organization_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub id: String,
    pub name: String,
    pub label: String,
    pub color: String,
    pub is_system: bool,
    pub organization_id: Option<String>,
}
