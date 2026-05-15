//! Document CRUD + retrieval-stats portion of [`super::SqliteStore`]'s
//! `KbStore` impl.
//!
//! Each method offloads rusqlite calls to a blocking thread so the async
//! interface stays responsive. Categories + metadata are serialized as JSON
//! strings to preserve the TS schema shape without introducing relational
//! join tables for tag-like fields.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Row};

use super::SqliteStore;
use crate::kb::store::{Bm25Hit, KbStore, SearchFilter};
use crate::kb::{Chunk, Document, DocumentId, KbError, KbResult, SearchResult};

fn map_row(row: &Row<'_>) -> rusqlite::Result<Document> {
    let categories_json: String = row.get("categories_json")?;
    let metadata_json: String = row.get("metadata_json")?;
    let created_at: String = row.get("created_at")?;
    let updated_at: String = row.get("updated_at")?;
    let deleted_at: Option<String> = row.get("deleted_at")?;
    let last_retrieved_at: Option<String> = row.get("last_retrieved_at")?;

    Ok(Document {
        id: DocumentId(row.get("id")?),
        title: row.get("title")?,
        content: row.get("content")?,
        categories: serde_json::from_str(&categories_json).unwrap_or_default(),
        subcategory: row.get("subcategory")?,
        metadata: serde_json::from_str(&metadata_json).unwrap_or(serde_json::Value::Null),
        s3_key: row.get("s3_key")?,
        file_type: row.get("file_type")?,
        mime_type: row.get("mime_type")?,
        file_size: row.get::<_, Option<i64>>("file_size")?.map(|v| v as u64),
        organization_id: row.get("organization_id")?,
        created_by: row.get("created_by")?,
        session_id: row.get("session_id")?,
        artifact_type: row.get("artifact_type")?,
        created_at: parse_ts(&created_at),
        updated_at: parse_ts(&updated_at),
        deleted_at: deleted_at.as_deref().map(parse_ts),
        retention_days: row.get("retention_days")?,
        retrieval_count: row.get("retrieval_count")?,
        last_retrieved_at: last_retrieved_at.as_deref().map(parse_ts),
    })
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[async_trait]
impl KbStore for SqliteStore {
    async fn create_document(&self, doc: &Document) -> KbResult<()> {
        let conn = self.conn.clone();
        let doc = doc.clone();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO document (
                    id, title, content, categories_json, subcategory, metadata_json,
                    s3_key, file_type, mime_type, file_size,
                    organization_id, created_by, session_id, artifact_type,
                    created_at, updated_at, deleted_at, retention_days,
                    retrieval_count, last_retrieved_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                params![
                    doc.id.0,
                    doc.title,
                    doc.content,
                    serde_json::to_string(&doc.categories)?,
                    doc.subcategory,
                    serde_json::to_string(&doc.metadata)?,
                    doc.s3_key,
                    doc.file_type,
                    doc.mime_type,
                    doc.file_size.map(|v| v as i64),
                    doc.organization_id,
                    doc.created_by,
                    doc.session_id,
                    doc.artifact_type,
                    doc.created_at.to_rfc3339(),
                    doc.updated_at.to_rfc3339(),
                    doc.deleted_at.map(|d| d.to_rfc3339()),
                    doc.retention_days,
                    doc.retrieval_count,
                    doc.last_retrieved_at.map(|d| d.to_rfc3339()),
                ],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    async fn get_document(&self, id: &DocumentId) -> KbResult<Option<Document>> {
        let conn = self.conn.clone();
        let id = id.0.clone();
        tokio::task::spawn_blocking(move || -> KbResult<Option<Document>> {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare("SELECT * FROM document WHERE id = ?1")?;
            let mut rows = stmt.query(params![id])?;
            if let Some(row) = rows.next()? {
                Ok(Some(map_row(row)?))
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    async fn update_document(&self, doc: &Document) -> KbResult<()> {
        let conn = self.conn.clone();
        let doc = doc.clone();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let conn = conn.blocking_lock();
            let updated = conn.execute(
                "UPDATE document SET
                    title = ?1, content = ?2, categories_json = ?3, subcategory = ?4,
                    metadata_json = ?5, s3_key = ?6, file_type = ?7, mime_type = ?8,
                    file_size = ?9, organization_id = ?10, created_by = ?11,
                    session_id = ?12, artifact_type = ?13, updated_at = ?14,
                    deleted_at = ?15, retention_days = ?16,
                    retrieval_count = ?17, last_retrieved_at = ?18
                 WHERE id = ?19",
                params![
                    doc.title,
                    doc.content,
                    serde_json::to_string(&doc.categories)?,
                    doc.subcategory,
                    serde_json::to_string(&doc.metadata)?,
                    doc.s3_key,
                    doc.file_type,
                    doc.mime_type,
                    doc.file_size.map(|v| v as i64),
                    doc.organization_id,
                    doc.created_by,
                    doc.session_id,
                    doc.artifact_type,
                    doc.updated_at.to_rfc3339(),
                    doc.deleted_at.map(|d| d.to_rfc3339()),
                    doc.retention_days,
                    doc.retrieval_count,
                    doc.last_retrieved_at.map(|d| d.to_rfc3339()),
                    doc.id.0,
                ],
            )?;
            if updated == 0 {
                return Err(KbError::NotFound(format!("document {}", doc.id.0)));
            }
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    async fn delete_document(&self, id: &DocumentId, soft: bool) -> KbResult<()> {
        let conn = self.conn.clone();
        let id = id.0.clone();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let conn = conn.blocking_lock();
            if soft {
                conn.execute(
                    "UPDATE document
                     SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE id = ?1",
                    params![id],
                )?;
            } else {
                conn.execute("DELETE FROM document WHERE id = ?1", params![id])?;
            }
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    async fn list_documents(&self, organization_id: Option<&str>) -> KbResult<Vec<Document>> {
        let conn = self.conn.clone();
        let org = organization_id.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || -> KbResult<Vec<Document>> {
            let conn = conn.blocking_lock();
            let mut docs = Vec::new();
            if let Some(org) = org {
                let mut stmt = conn.prepare(
                    "SELECT * FROM document
                     WHERE organization_id = ?1 AND deleted_at IS NULL
                     ORDER BY created_at DESC",
                )?;
                let mut rows = stmt.query(params![org])?;
                while let Some(row) = rows.next()? {
                    docs.push(map_row(row)?);
                }
            } else {
                let mut stmt = conn.prepare(
                    "SELECT * FROM document
                     WHERE deleted_at IS NULL
                     ORDER BY created_at DESC",
                )?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    docs.push(map_row(row)?);
                }
            }
            Ok(docs)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    async fn record_retrieval_hits(&self, ids: &[DocumentId]) -> KbResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.conn.clone();
        let ids: Vec<String> = ids.iter().map(|d| d.0.clone()).collect();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let mut conn = conn.blocking_lock();
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "UPDATE document
                     SET retrieval_count = retrieval_count + 1,
                         last_retrieved_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE id = ?1",
                )?;
                for id in &ids {
                    stmt.execute(params![id])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    // --- chunks: stubbed until Task 2.5 lands. Returning explicit "not yet
    // implemented" surfaces the contract to callers without panicking. ---

    async fn store_chunks(
        &self,
        _document_id: &DocumentId,
        _chunks: &[Chunk],
        _embeddings: &[Vec<f32>],
        _embedding_model: &str,
    ) -> KbResult<()> {
        Err(KbError::Other("store_chunks: pending Task 2.5".to_string()))
    }

    async fn delete_chunks_by_document(&self, _document_id: &DocumentId) -> KbResult<()> {
        Err(KbError::Other(
            "delete_chunks_by_document: pending Task 2.5".to_string(),
        ))
    }

    async fn chunk_count(&self, _document_id: &DocumentId) -> KbResult<usize> {
        Err(KbError::Other("chunk_count: pending Task 2.5".to_string()))
    }

    async fn chunk_counts(
        &self,
        _ids: &[DocumentId],
    ) -> KbResult<std::collections::HashMap<DocumentId, usize>> {
        Err(KbError::Other("chunk_counts: pending Task 2.5".to_string()))
    }

    async fn search_by_vector(
        &self,
        _query: &[f32],
        _limit: usize,
        _filter: &SearchFilter,
    ) -> KbResult<Vec<SearchResult>> {
        Err(KbError::Other(
            "search_by_vector: pending Task 2.5".to_string(),
        ))
    }

    async fn bm25_search(&self, _query: &str, _limit: usize) -> KbResult<Vec<Bm25Hit>> {
        Err(KbError::Other("bm25_search: pending Task 2.6".to_string()))
    }

    async fn count_by_embedding_model(&self) -> KbResult<Vec<(Option<String>, usize)>> {
        Err(KbError::Other(
            "count_by_embedding_model: pending Task 2.7".to_string(),
        ))
    }
}
