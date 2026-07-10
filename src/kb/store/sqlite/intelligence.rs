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
use crate::kb::{ChunkId, ChunkMetadata, DocumentId, KbError, KbResult, SearchResult};

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
            // ON CONFLICT(canonical_key) keeps the first-seen node identity
            // (id/name/type) but refreshes confidence to the MAX across mentions,
            // so a later, higher-confidence extraction (e.g. a re-extract after a
            // prompt fix) lifts a stale value instead of being silently dropped.
            // rusqlite RETURNING support is version-dependent, so resolve the id
            // with a follow-up SELECT.
            conn.execute(
                "INSERT INTO entity (id, canonical_key, name, type, confidence, metadata, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                 ON CONFLICT(canonical_key) DO UPDATE SET confidence = max(confidence, excluded.confidence)",
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

            // Edges = relations whose BOTH endpoints are in the selected node
            // set, deduplicated by (source, target, relation_type) with a
            // `weight` counting how many relation rows collapsed into each.
            let node_ids: std::collections::HashSet<String> =
                nodes.iter().map(|n| n.id.clone()).collect();
            let mut ew: std::collections::HashMap<(String, String, String), usize> =
                std::collections::HashMap::new();
            {
                let mut stmt = conn.prepare(
                    "SELECT source_entity_id, target_entity_id, relation_type FROM entity_relation",
                )?;
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let (s, t): (String, String) =
                        (row.get("source_entity_id")?, row.get("target_entity_id")?);
                    if node_ids.contains(&s) && node_ids.contains(&t) {
                        let r: String = row.get("relation_type")?;
                        *ew.entry((s, t, r)).or_insert(0) += 1;
                    }
                }
            }
            let edges: Vec<GraphEdge> = ew
                .into_iter()
                .map(|((source, target, relation_type), weight)| GraphEdge {
                    source,
                    target,
                    relation_type,
                    weight,
                })
                .collect();

            // degree = incident DEDUPED edges (overwrites the SQL
            // COUNT(DISTINCT r.id) ordering key used for node selection above).
            let mut degree: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for e in &edges {
                *degree.entry(e.source.clone()).or_insert(0) += 1;
                if e.target != e.source {
                    *degree.entry(e.target.clone()).or_insert(0) += 1;
                }
            }
            for n in &mut nodes {
                n.degree = degree.get(&n.id).copied().unwrap_or(0);
            }

            // Scope-aware corpus counts (both honor `group`, so a group view
            // is internally consistent). Computed scope-wide, i.e. before the
            // top-N node cap applied above.
            let (total_entities, total_relations) = match &group_id {
                Some(g) => {
                    let scoped = "SELECT entity_id FROM entity_mention WHERE document_id IN
                                  (SELECT document_id FROM document_group WHERE group_id = ?1)";
                    let te: i64 = conn.query_row(
                        &format!("SELECT COUNT(DISTINCT entity_id) FROM ({scoped})"),
                        params![g],
                        |r| r.get(0),
                    )?;
                    let tr: i64 = conn.query_row(
                        &format!(
                            "SELECT COUNT(*) FROM (SELECT DISTINCT source_entity_id, target_entity_id, relation_type
                             FROM entity_relation WHERE source_entity_id IN ({scoped}) AND target_entity_id IN ({scoped}))"
                        ),
                        params![g],
                        |r| r.get(0),
                    )?;
                    (usize::try_from(te).unwrap_or(0), usize::try_from(tr).unwrap_or(0))
                }
                None => {
                    let te: i64 =
                        conn.query_row("SELECT COUNT(*) FROM entity", [], |r| r.get(0))?;
                    let tr: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM (SELECT DISTINCT source_entity_id, target_entity_id, relation_type FROM entity_relation)",
                        [],
                        |r| r.get(0),
                    )?;
                    (usize::try_from(te).unwrap_or(0), usize::try_from(tr).unwrap_or(0))
                }
            };

            Ok(Graph {
                nodes,
                edges,
                total_entities,
                total_relations,
            })
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

    async fn graph_expand_chunks(
        &self,
        query: &str,
        max_neighbors: usize,
        limit: usize,
    ) -> KbResult<Vec<SearchResult>> {
        let conn = self.conn.clone();
        let query = query.to_string();
        let max_neighbors = max_neighbors as i64;
        tokio::task::spawn_blocking(move || -> KbResult<Vec<SearchResult>> {
            let conn = conn.blocking_lock();

            // 1) Seeds: entities whose name (>= 3 chars, to avoid noise) appears
            //    as a case-insensitive substring of the query. `instr` returns a
            //    1-based position, or 0 when not found.
            let mut seed_ids: Vec<String> = Vec::new();
            {
                let mut stmt = conn.prepare(
                    "SELECT id FROM entity
                     WHERE length(name) >= 3 AND instr(lower(?1), lower(name)) > 0",
                )?;
                let mut rows = stmt.query(params![query])?;
                while let Some(row) = rows.next()? {
                    seed_ids.push(row.get(0)?);
                }
            }
            if seed_ids.is_empty() {
                return Ok(Vec::new());
            }

            // 2) One-hop neighbours: the other endpoint of any relation touching
            //    a seed, capped at `max_neighbors`. Seeds are always included.
            let seed_set: std::collections::HashSet<String> = seed_ids.iter().cloned().collect();
            let mut entity_ids = seed_set.clone();
            {
                let ph = vec!["?"; seed_ids.len()].join(",");
                let sql = format!(
                    "SELECT source_entity_id, target_entity_id FROM entity_relation
                     WHERE source_entity_id IN ({ph}) OR target_entity_id IN ({ph})"
                );
                let mut stmt = conn.prepare(&sql)?;
                // Seeds bound twice — once per IN clause.
                let bind = seed_ids.iter().chain(seed_ids.iter());
                let mut rows = stmt.query(rusqlite::params_from_iter(bind))?;
                let mut added: i64 = 0;
                while let Some(row) = rows.next()? {
                    if added >= max_neighbors {
                        break;
                    }
                    let src: String = row.get(0)?;
                    let tgt: String = row.get(1)?;
                    for cand in [src, tgt] {
                        if !seed_set.contains(&cand) && entity_ids.insert(cand) {
                            added += 1;
                            if added >= max_neighbors {
                                break;
                            }
                        }
                    }
                }
            }

            // 3) Chunks mentioning any seed/neighbour entity → SearchResult,
            //    ordered by how many matched entities each chunk mentions.
            //    `limit` is an internal usize (not user input), so it is inlined.
            let entity_vec: Vec<String> = entity_ids.into_iter().collect();
            let ph = vec!["?"; entity_vec.len()].join(",");
            let sql = format!(
                "SELECT c.id, c.document_id, c.content, c.metadata_json,
                        c.contextual_prefix, d.title, d.categories_json, d.subcategory,
                        COUNT(*) AS hits
                 FROM chunk c
                 JOIN entity_mention m
                       ON m.document_id = c.document_id AND m.chunk_index = c.chunk_index
                 JOIN document d ON d.id = c.document_id
                 WHERE m.entity_id IN ({ph}) AND d.deleted_at IS NULL
                 GROUP BY c.id
                 ORDER BY hits DESC, c.id ASC
                 LIMIT {limit}"
            );
            let mut results: Vec<SearchResult> = Vec::new();
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(entity_vec.iter()))?;
            while let Some(r) = rows.next()? {
                let chunk_id: String = r.get("id")?;
                let document_id: String = r.get("document_id")?;
                let categories_json: String = r.get("categories_json")?;
                let categories: Vec<String> =
                    serde_json::from_str(&categories_json).unwrap_or_default();
                let metadata_json: String = r.get("metadata_json")?;
                let metadata: ChunkMetadata =
                    serde_json::from_str(&metadata_json).unwrap_or_else(|_| ChunkMetadata {
                        document_title: r.get("title").unwrap_or_default(),
                        category: categories.first().cloned().unwrap_or_default(),
                        subcategory: r.get("subcategory").ok(),
                        section: None,
                        chunk_index: 0,
                        contextual_prefix: r.get("contextual_prefix").ok().flatten(),
                    });
                results.push(SearchResult {
                    id: ChunkId(chunk_id),
                    document_id: DocumentId(document_id),
                    document_title: r.get("title")?,
                    content: r.get("content")?,
                    categories,
                    subcategory: r.get("subcategory")?,
                    section: metadata.section.clone(),
                    // Graph-sourced candidates rank via RRF position, not cosine
                    // similarity, so there is no meaningful score here.
                    similarity: 0.0,
                    contextual_prefix: metadata.contextual_prefix.clone(),
                });
            }
            Ok(results)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }
}
