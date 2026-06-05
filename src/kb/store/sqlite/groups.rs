//! Knowledge-base group CRUD + membership portion of [`super::SqliteStore`].
//!
//! Groups live in `knowledge_base_group`; membership lives in the
//! `document_group` junction table. Each method offloads rusqlite calls to a
//! blocking thread, mirroring `documents.rs`. Timestamps are RFC3339 TEXT,
//! parsed fail-fast on read (corruption must surface, not be masked).
//!
//! Foreign-key enforcement is **not** assumed on the connection (the schema
//! declares `ON DELETE CASCADE` but `PRAGMA foreign_keys` is off by default in
//! SQLite). `delete_group` therefore removes the `document_group` rows
//! explicitly inside a transaction rather than relying on cascade.

use chrono::{DateTime, Utc};
use rusqlite::params;

use super::documents::map_row;
use super::SqliteStore;
use crate::kb::{Document, KbError, KbGroup, KbGroupSummary, KbResult};

/// Parse an RFC3339 timestamp out of a TEXT column. Fail-fast (CLAUDE.md 3.5):
/// a corrupt timestamp must surface, not silently coerce to `now`.
fn parse_ts(s: &str) -> KbResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| KbError::Other(format!("invalid timestamp in DB: {s:?} ({e})")))
}

impl SqliteStore {
    pub(crate) async fn create_group_impl(
        &self,
        name: &str,
        description: Option<&str>,
        color: Option<&str>,
    ) -> KbResult<KbGroup> {
        let conn = self.conn.clone();
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let group = KbGroup {
            id: id.clone(),
            name: name.to_string(),
            description: description.map(str::to_string),
            color: color.map(str::to_string),
            created_at: now,
            updated_at: now,
        };
        let row = group.clone();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO knowledge_base_group (
                    id, name, description, color, organization_id, created_by,
                    created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?6)",
                params![
                    row.id,
                    row.name,
                    row.description,
                    row.color,
                    row.created_at.to_rfc3339(),
                    row.updated_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))??;
        Ok(group)
    }

    pub(crate) async fn list_groups_impl(&self) -> KbResult<Vec<KbGroupSummary>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> KbResult<Vec<KbGroupSummary>> {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT g.id, g.name, g.description, g.color,
                        (SELECT COUNT(*) FROM document_group dg WHERE dg.group_id = g.id)
                            AS document_count
                 FROM knowledge_base_group g
                 ORDER BY g.created_at DESC",
            )?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(KbGroupSummary {
                    id: row.get("id")?,
                    name: row.get("name")?,
                    description: row.get("description")?,
                    color: row.get("color")?,
                    document_count: row.get("document_count")?,
                });
            }
            Ok(out)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    pub(crate) async fn get_group_impl(&self, id: &str) -> KbResult<Option<KbGroup>> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || -> KbResult<Option<KbGroup>> {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, name, description, color, created_at, updated_at
                 FROM knowledge_base_group WHERE id = ?1",
            )?;
            let mut rows = stmt.query(params![id])?;
            if let Some(row) = rows.next()? {
                let created_at: String = row.get("created_at")?;
                let updated_at: String = row.get("updated_at")?;
                Ok(Some(KbGroup {
                    id: row.get("id")?,
                    name: row.get("name")?,
                    description: row.get("description")?,
                    color: row.get("color")?,
                    created_at: parse_ts(&created_at)?,
                    updated_at: parse_ts(&updated_at)?,
                }))
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    pub(crate) async fn update_group_impl(
        &self,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
        color: Option<&str>,
    ) -> KbResult<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let name = name.map(str::to_string);
        let description = description.map(str::to_string);
        let color = color.map(str::to_string);
        let now = Utc::now().to_rfc3339();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let conn = conn.blocking_lock();
            // Build a partial UPDATE: only touch columns the caller supplied.
            // `updated_at` is always bumped. COALESCE-style branching is
            // avoided in favor of an explicit set-list so the SQL stays
            // auditable (KISS).
            let mut sets: Vec<&str> = Vec::new();
            let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::new();
            if let Some(n) = name.as_ref() {
                sets.push("name = ?");
                binds.push(n);
            }
            if let Some(d) = description.as_ref() {
                sets.push("description = ?");
                binds.push(d);
            }
            if let Some(c) = color.as_ref() {
                sets.push("color = ?");
                binds.push(c);
            }
            sets.push("updated_at = ?");
            binds.push(&now);
            binds.push(&id);
            let sql = format!(
                "UPDATE knowledge_base_group SET {} WHERE id = ?",
                sets.join(", ")
            );
            let updated = conn.execute(&sql, rusqlite::params_from_iter(binds))?;
            if updated == 0 {
                return Err(KbError::NotFound(format!("group {id}")));
            }
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    pub(crate) async fn delete_group_impl(&self, id: &str) -> KbResult<bool> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || -> KbResult<bool> {
            let mut conn = conn.blocking_lock();
            let tx = conn.transaction()?;
            // Clear membership explicitly — foreign_keys enforcement is off by
            // default, so the schema's ON DELETE CASCADE would not fire.
            tx.execute(
                "DELETE FROM document_group WHERE group_id = ?1",
                params![id],
            )?;
            let removed = tx.execute(
                "DELETE FROM knowledge_base_group WHERE id = ?1",
                params![id],
            )?;
            tx.commit()?;
            Ok(removed > 0)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    pub(crate) async fn add_document_to_group_impl(
        &self,
        document_id: &str,
        group_id: &str,
    ) -> KbResult<()> {
        let conn = self.conn.clone();
        let document_id = document_id.to_string();
        let group_id = group_id.to_string();
        let now = Utc::now().to_rfc3339();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO document_group (document_id, group_id, created_at)
                 VALUES (?1, ?2, ?3)",
                params![document_id, group_id, now],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    pub(crate) async fn remove_document_from_group_impl(
        &self,
        document_id: &str,
        group_id: &str,
    ) -> KbResult<bool> {
        let conn = self.conn.clone();
        let document_id = document_id.to_string();
        let group_id = group_id.to_string();
        tokio::task::spawn_blocking(move || -> KbResult<bool> {
            let conn = conn.blocking_lock();
            let removed = conn.execute(
                "DELETE FROM document_group WHERE document_id = ?1 AND group_id = ?2",
                params![document_id, group_id],
            )?;
            Ok(removed > 0)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    pub(crate) async fn list_group_documents_impl(
        &self,
        group_id: &str,
    ) -> KbResult<Vec<Document>> {
        let conn = self.conn.clone();
        let group_id = group_id.to_string();
        tokio::task::spawn_blocking(move || -> KbResult<Vec<Document>> {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT d.* FROM document d
                 JOIN document_group dg ON dg.document_id = d.id
                 WHERE dg.group_id = ?1 AND d.deleted_at IS NULL
                 ORDER BY d.created_at DESC",
            )?;
            let mut rows = stmt.query(params![group_id])?;
            let mut docs = Vec::new();
            while let Some(row) = rows.next()? {
                docs.push(map_row(row)?);
            }
            Ok(docs)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }
}
