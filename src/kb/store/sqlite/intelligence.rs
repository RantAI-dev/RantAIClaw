//! Cross-document knowledge-graph portion of [`super::SqliteStore`] (SP-2 KB
//! Document Intelligence). Implements [`IntelligenceStore`].
//!
//! Mirrors `documents.rs`: every method offloads rusqlite work to a blocking
//! thread so the async interface stays responsive, serializes JSON metadata as
//! TEXT, and stamps timestamps in SQL via `strftime` (matching the existing
//! backend convention). Entities are deduplicated by `canonical_key`; mentions
//! and relations link entities to their source documents.

use async_trait::async_trait;
use rusqlite::{params, Row};

use super::SqliteStore;
use crate::kb::intelligence::types::{Entity, EntityMention, EntityType, Relation, RelationType};
use crate::kb::store::{Graph, GraphEdge, GraphNode, IntelligenceStore};
use crate::kb::{KbError, KbResult};

fn map_entity(row: &Row<'_>) -> KbResult<Entity> {
    let metadata_json: String = row.get("metadata")?;
    let type_str: String = row.get("type")?;
    Ok(Entity {
        id: row.get("id")?,
        canonical_key: row.get("canonical_key")?,
        name: row.get("name")?,
        entity_type: EntityType::from_str_lenient(&type_str),
        confidence: row.get("confidence")?,
        metadata: serde_json::from_str(&metadata_json).unwrap_or(serde_json::Value::Null),
    })
}

fn map_relation(row: &Row<'_>) -> KbResult<Relation> {
    let metadata_json: String = row.get("metadata")?;
    let type_str: String = row.get("relation_type")?;
    Ok(Relation {
        id: row.get("id")?,
        source_entity_id: row.get("source_entity_id")?,
        target_entity_id: row.get("target_entity_id")?,
        relation_type: RelationType::from_str_lenient(&type_str),
        confidence: row.get("confidence")?,
        document_id: row.get("document_id")?,
        metadata: serde_json::from_str(&metadata_json).unwrap_or(serde_json::Value::Null),
    })
}

#[async_trait]
impl IntelligenceStore for SqliteStore {
    async fn upsert_entity(&self, e: &Entity) -> KbResult<String> {
        let conn = self.conn.clone();
        let e = e.clone();
        tokio::task::spawn_blocking(move || -> KbResult<String> {
            let conn = conn.blocking_lock();
            // ON CONFLICT(canonical_key) DO NOTHING keeps the first-seen node;
            // rusqlite RETURNING support is version-dependent, so resolve the id
            // with a follow-up SELECT.
            conn.execute(
                "INSERT INTO entity (id, canonical_key, name, type, confidence, metadata, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                 ON CONFLICT(canonical_key) DO NOTHING",
                params![
                    e.id,
                    e.canonical_key,
                    e.name,
                    e.entity_type.as_str(),
                    e.confidence,
                    serde_json::to_string(&e.metadata)?,
                ],
            )?;
            let id: String = conn.query_row(
                "SELECT id FROM entity WHERE canonical_key = ?1",
                params![e.canonical_key],
                |row| row.get(0),
            )?;
            Ok(id)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    async fn add_mention(&self, m: &EntityMention) -> KbResult<()> {
        let conn = self.conn.clone();
        let m = m.clone();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO entity_mention (id, entity_id, document_id, chunk_index, context, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    m.id,
                    m.entity_id,
                    m.document_id,
                    m.chunk_index,
                    m.context,
                    m.source.as_str(),
                ],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    async fn add_relation(&self, r: &Relation) -> KbResult<()> {
        let conn = self.conn.clone();
        let r = r.clone();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO entity_relation
                    (id, source_entity_id, target_entity_id, relation_type, confidence, document_id, metadata, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                params![
                    r.id,
                    r.source_entity_id,
                    r.target_entity_id,
                    r.relation_type.as_str(),
                    r.confidence,
                    r.document_id,
                    serde_json::to_string(&r.metadata)?,
                ],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    async fn intelligence_for_document(
        &self,
        document_id: &str,
    ) -> KbResult<(Vec<Entity>, Vec<Relation>)> {
        let conn = self.conn.clone();
        let document_id = document_id.to_string();
        tokio::task::spawn_blocking(move || -> KbResult<(Vec<Entity>, Vec<Relation>)> {
            let conn = conn.blocking_lock();
            let mut entities = Vec::new();
            {
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT e.* FROM entity e
                     JOIN entity_mention m ON m.entity_id = e.id
                     WHERE m.document_id = ?1",
                )?;
                let mut rows = stmt.query(params![document_id])?;
                while let Some(row) = rows.next()? {
                    entities.push(map_entity(row)?);
                }
            }
            let mut relations = Vec::new();
            {
                let mut stmt =
                    conn.prepare("SELECT * FROM entity_relation WHERE document_id = ?1")?;
                let mut rows = stmt.query(params![document_id])?;
                while let Some(row) = rows.next()? {
                    relations.push(map_relation(row)?);
                }
            }
            Ok((entities, relations))
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    async fn graph(&self, group_id: Option<&str>, limit: usize) -> KbResult<Graph> {
        let conn = self.conn.clone();
        let group_id = group_id.map(|s| s.to_string());
        let limit = limit as i64;
        tokio::task::spawn_blocking(move || -> KbResult<Graph> {
            let conn = conn.blocking_lock();

            // Nodes ordered by degree (relation endpoints) desc, capped to top-N.
            // When group-scoped, restrict to entities mentioned in that group's
            // documents via the `document_group` membership table.
            let node_sql = if group_id.is_some() {
                "SELECT e.id, e.name, e.type,
                        COUNT(DISTINCT r.id) AS degree,
                        COUNT(DISTINCT m.document_id) AS doc_count
                 FROM entity e
                 LEFT JOIN entity_mention m ON m.entity_id = e.id
                 LEFT JOIN entity_relation r
                        ON (r.source_entity_id = e.id OR r.target_entity_id = e.id)
                 WHERE e.id IN (
                     SELECT m2.entity_id FROM entity_mention m2
                     WHERE m2.document_id IN (
                         SELECT document_id FROM document_group WHERE group_id = ?2
                     )
                 )
                 GROUP BY e.id
                 ORDER BY degree DESC
                 LIMIT ?1"
            } else {
                "SELECT e.id, e.name, e.type,
                        COUNT(DISTINCT r.id) AS degree,
                        COUNT(DISTINCT m.document_id) AS doc_count
                 FROM entity e
                 LEFT JOIN entity_mention m ON m.entity_id = e.id
                 LEFT JOIN entity_relation r
                        ON (r.source_entity_id = e.id OR r.target_entity_id = e.id)
                 GROUP BY e.id
                 ORDER BY degree DESC
                 LIMIT ?1"
            };

            let mut nodes = Vec::new();
            {
                let mut stmt = conn.prepare(node_sql)?;
                let mut rows = match &group_id {
                    Some(g) => stmt.query(params![limit, g])?,
                    None => stmt.query(params![limit])?,
                };
                while let Some(row) = rows.next()? {
                    let degree: i64 = row.get("degree")?;
                    let doc_count: i64 = row.get("doc_count")?;
                    nodes.push(GraphNode {
                        id: row.get("id")?,
                        name: row.get("name")?,
                        entity_type: row.get("type")?,
                        degree: usize::try_from(degree).unwrap_or(0),
                        doc_count: usize::try_from(doc_count).unwrap_or(0),
                    });
                }
            }

            // Edges = relations whose BOTH endpoints are in the selected node set.
            let node_ids: std::collections::HashSet<String> =
                nodes.iter().map(|n| n.id.clone()).collect();
            let mut edges = Vec::new();
            {
                let mut stmt = conn.prepare(
                    "SELECT source_entity_id, target_entity_id, relation_type FROM entity_relation",
                )?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let source: String = row.get("source_entity_id")?;
                    let target: String = row.get("target_entity_id")?;
                    if node_ids.contains(&source) && node_ids.contains(&target) {
                        edges.push(GraphEdge {
                            source,
                            target,
                            relation_type: row.get("relation_type")?,
                        });
                    }
                }
            }

            Ok(Graph { nodes, edges })
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }

    async fn delete_document_intelligence(&self, document_id: &str) -> KbResult<()> {
        let conn = self.conn.clone();
        let document_id = document_id.to_string();
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let mut conn = conn.blocking_lock();
            // Explicit child deletes + orphan GC in one transaction. Does NOT
            // rely on FK cascade (PRAGMA foreign_keys is off by default here,
            // matching the rest of the backend).
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM entity_mention WHERE document_id = ?1",
                params![document_id],
            )?;
            tx.execute(
                "DELETE FROM entity_relation WHERE document_id = ?1",
                params![document_id],
            )?;
            tx.execute(
                "DELETE FROM entity
                 WHERE id NOT IN (SELECT entity_id FROM entity_mention)",
                [],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }
}
