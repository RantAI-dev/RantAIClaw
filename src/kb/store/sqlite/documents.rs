//! Document CRUD + retrieval-stats portion of [`super::SqliteStore`].
//!
//! Each method offloads rusqlite calls to a blocking thread so the async
//! interface stays responsive. Categories + metadata are serialized as JSON
//! strings to preserve the TS schema shape without introducing relational
//! join tables for tag-like fields.

use chrono::{DateTime, Utc};
use rusqlite::{params, Row};

use super::SqliteStore;
use crate::kb::{Document, DocumentId, KbError, KbResult};

#[allow(clippy::cast_sign_loss)]
pub(crate) fn map_row(row: &Row<'_>) -> KbResult<Document> {
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
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
        deleted_at: deleted_at.as_deref().map(parse_ts).transpose()?,
        retention_days: row.get("retention_days")?,
        retrieval_count: row.get("retrieval_count")?,
        last_retrieved_at: last_retrieved_at.as_deref().map(parse_ts).transpose()?,
    })
}

/// Parse an RFC3339 timestamp out of a TEXT column. Fail-fast (CLAUDE.md 3.5):
/// silently falling back to `Utc::now()` masked storage corruption and made
/// retention/age computations meaningless.
fn parse_ts(s: &str) -> KbResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| KbError::Other(format!("invalid timestamp in DB: {s:?} ({e})")))
}

impl SqliteStore {
    pub(crate) async fn create_document_impl(&self, doc: &Document) -> KbResult<()> {
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

    pub(crate) async fn get_document_impl(&self, id: &DocumentId) -> KbResult<Option<Document>> {
        let conn = self.conn.clone();
        let id = id.0.clone();
        tokio::task::spawn_blocking(move || -> KbResult<Option<Document>> {
            let conn = conn.blocking_lock();
            let mut stmt =
                conn.prepare("SELECT * FROM document WHERE id = ?1 AND deleted_at IS NULL")?;
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

    pub(crate) async fn update_document_impl(&self, doc: &Document) -> KbResult<()> {
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

    pub(crate) async fn delete_document_impl(&self, id: &DocumentId, soft: bool) -> KbResult<()> {
        let conn = self.conn.clone();
        let id = id.0.clone();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let mut conn = conn.blocking_lock();
            if soft {
                conn.execute(
                    "UPDATE document
                     SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                     WHERE id = ?1",
                    params![id],
                )?;
            } else {
                // Hard delete: drop the document AND its chunks + vectors
                // atomically. `chunk_vec` (vec0) has no foreign key, so neither a
                // cascade nor `DELETE FROM document` reaches it — delete it
                // explicitly by the chunk rowids, or it orphans and a later
                // ingest collides on rowid reuse
                // (`UNIQUE constraint failed on chunk_vec primary key`).
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare("SELECT rowid FROM chunk WHERE document_id = ?1")?;
                    let rowids: Vec<i64> = stmt
                        .query_map(params![id], |row| row.get(0))?
                        .collect::<rusqlite::Result<_>>()?;
                    drop(stmt);
                    let mut del_vec = tx.prepare("DELETE FROM chunk_vec WHERE rowid = ?1")?;
                    for rid in &rowids {
                        del_vec.execute(params![rid])?;
                    }
                    drop(del_vec);
                    tx.execute("DELETE FROM chunk WHERE document_id = ?1", params![id])?;
                    tx.execute("DELETE FROM document WHERE id = ?1", params![id])?;
                }
                tx.commit()?;
            }
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    pub(crate) async fn list_documents_impl(
        &self,
        organization_id: Option<&str>,
    ) -> KbResult<Vec<Document>> {
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

    pub(crate) async fn record_retrieval_hits_impl(&self, ids: &[DocumentId]) -> KbResult<()> {
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
}
