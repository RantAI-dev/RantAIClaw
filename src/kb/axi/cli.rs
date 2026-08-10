//! `rantaiclaw kb` axi-cli subcommand and dispatcher.
//!
//! Per `axi.md`, the axi-cli is the agent-shellable surface: idempotent, no
//! interactive prompts, TOON output by default, JSON on `--json`. Every arm
//! returns one of two exit codes:
//!
//! - `Ok(0)` — operation succeeded; output already printed.
//! - `Ok(1)` — operational failure (e.g. document not found). Caller printed
//!   a TOON-formatted `error[1]{code,message}:` block.
//! - `Err(KbError)` — internal failure (DB unreachable, bad config). `main.rs`
//!   prints a TOON error block to stdout and exits 1.
//!
//! Storage path resolution: `KB_DB_PATH` env var → the active profile's
//! `profiles/<name>/kb.db` → `./kb.db` cwd fallback.
//!
//! Heavy components (config, store, embedder, optional reranker) are built
//! lazily inside the dispatcher rather than at construct-time, so `--help`
//! and clap parse errors stay fast and offline.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;

use crate::kb::chunk::{smart_chunk_document, SmartChunkOptions};
use crate::kb::embed;
use crate::kb::file::{process_file, ProcessingOptions};
use crate::kb::intelligence::extract::llm::CombinedLlmExtractor;
use crate::kb::intelligence::extract_document_intelligence;
use crate::kb::intelligence::types::{Entity, Relation};
use crate::kb::maintenance::{
    check_drift, run_bulk_re_embed, BulkReEmbedOptions, BulkReEmbedReport, DriftReport,
};
use crate::kb::rerank;
use crate::kb::retrieve::format::format_context_for_prompt;
use crate::kb::retrieve::{RetrieveOptions, Retriever, SourceRef};
use crate::kb::store::sqlite::SqliteStore;
use crate::kb::store::{Graph, IntelligenceStore, KbStore};
use crate::kb::{Document, DocumentId, KbConfig, KbResult, SearchResult};

/// Per-chunk preview width in the TOON `chunks` table. The table is a
/// machine-readable index; the full chunk text reaches the agent through the
/// retrieval `context` block printed above it (see `cmd_search`), so this cap
/// only bounds the table row — it must stay wide enough to identify a chunk,
/// not carry the whole answer. Chunks are built at ~800 chars
/// (`SmartChunkOptions::default`).
const CONTENT_PREVIEW_CHARS: usize = 600;

#[derive(Subcommand, Debug)]
pub enum KbCommand {
    /// Search the knowledge base. Outputs TOON by default.
    Search {
        /// Search query
        query: String,
        /// Max chunks to return
        #[arg(long, default_value_t = 5)]
        top: usize,
        /// Filter by knowledge base group ID (repeat for multiple)
        #[arg(long = "group")]
        groups: Vec<String>,
        /// Filter by category
        #[arg(long)]
        category: Option<String>,
        /// Output JSON instead of TOON
        #[arg(long)]
        json: bool,
    },
    /// Ingest a file (PDF, markdown, image, office, text).
    Ingest {
        /// Path to the file to ingest
        path: PathBuf,
        /// Override document title (default: file stem)
        #[arg(long)]
        title: Option<String>,
        /// Add to categories (repeat for multiple)
        #[arg(long = "category")]
        categories: Vec<String>,
        /// Add to knowledge base groups (repeat for multiple)
        #[arg(long = "group")]
        groups: Vec<String>,
        /// Output JSON instead of TOON
        #[arg(long)]
        json: bool,
    },
    /// List documents.
    List {
        /// Filter by organization ID
        #[arg(long)]
        organization: Option<String>,
        /// Output JSON instead of TOON
        #[arg(long)]
        json: bool,
    },
    /// Show a document by id.
    Get {
        /// Document id
        id: String,
        /// Output JSON instead of TOON
        #[arg(long)]
        json: bool,
    },
    /// Delete a document. Defaults to soft-delete; `--hard` for permanent.
    Delete {
        /// Document id
        id: String,
        /// Hard-delete (permanently remove rows). Default is soft-delete.
        #[arg(long)]
        hard: bool,
    },
    /// Report which chunks were embedded with a stale model.
    Drift {
        /// Output JSON instead of TOON
        #[arg(long)]
        json: bool,
    },
    /// Re-embed every chunk using the currently-configured model.
    ReEmbed {
        /// Re-embed even chunks already on current model
        #[arg(long)]
        include_current: bool,
        /// Report without writing
        #[arg(long)]
        dry_run: bool,
        /// Batch size
        #[arg(long, default_value_t = 100)]
        batch_size: usize,
        /// Output JSON instead of TOON
        #[arg(long)]
        json: bool,
    },
    /// Show the entities and relations extracted from a document.
    Intelligence {
        /// Document id
        document_id: String,
        /// Output JSON instead of TOON
        #[arg(long)]
        json: bool,
    },
    /// Show whether the Knowledge Base is active and whether a key resolves.
    Status {
        /// Output JSON instead of TOON
        #[arg(long)]
        json: bool,
    },
    /// Activate the Knowledge Base. Refuses when no embedding key resolves.
    Enable,
    /// Deactivate the Knowledge Base. Credentials are kept.
    Disable,
    /// Show the cross-document knowledge graph (top entities by degree).
    Graph {
        /// Filter to a knowledge base group's documents
        #[arg(long)]
        group: Option<String>,
        /// Max nodes (defaults to KB_GRAPH_MAX_NODES)
        #[arg(long)]
        limit: Option<usize>,
        /// Output JSON instead of TOON
        #[arg(long)]
        json: bool,
    },
}

impl KbCommand {
    /// Run the subcommand. Returns:
    /// - `Ok(0)` — success.
    /// - `Ok(1)` — operational failure already reported on stdout.
    /// - `Err(KbError)` — internal failure; caller decides how to render.
    ///
    /// Plain-data parameters rather than `&Config`: `main.rs` compiles the
    /// config module as its own crate, so the bin's `Config` and the lib's
    /// `Config` are distinct types even though they share a source file.
    pub async fn run(
        self,
        knowledge_enabled: bool,
        embedding_api_key: Option<&str>,
        vision_api_key: Option<&str>,
    ) -> KbResult<i32> {
        // Status/Enable/Disable never open the store (opening rewrites
        // kb_meta — plan 098) and must work while the KB is off.
        match &self {
            Self::Status { json } => {
                return cmd_status_sync(knowledge_enabled, embedding_api_key, vision_api_key, *json)
            }
            Self::Enable => return cmd_set_enabled(true).await,
            Self::Disable => return cmd_set_enabled(false).await,
            _ => {}
        }
        // The operator's off switch (plans 102/104/107): data subcommands
        // answer a parseable TOON error + exit 1 (the AXI contract) instead
        // of a confusing empty result. Mirrors the HTTP kb_disabled gate.
        if !knowledge_enabled {
            print_error_toon(
                "kb_disabled",
                "Knowledge Base is off. Run `rantaiclaw kb enable`.",
            );
            return Ok(1);
        }
        let cfg = KbConfig::from_env_with_keys(embedding_api_key, vision_api_key)?;
        // Build the concrete store once, then alias it through both trait
        // views. `SqliteStore` implements `KbStore` and `IntelligenceStore`;
        // `Arc<dyn KbStore>` can't be upcast to `Arc<dyn IntelligenceStore>`,
        // so we keep a second handle over the same `SqliteStore` (mirrors the
        // HTTP `KbContext`). They share one DB handle.
        let concrete = open_store(&cfg).await?;
        let store: Arc<dyn KbStore> = concrete.clone();

        match self {
            Self::Search {
                query,
                top,
                groups,
                category,
                json,
            } => {
                // Attach the intelligence handle so GraphRAG can augment search
                // when `KB_GRAPHRAG_ENABLED`. This is the path the agent uses
                // (it shells out to `rantaiclaw kb search`).
                let intel: Arc<dyn IntelligenceStore> = concrete;
                cmd_search(&cfg, store, intel, query, top, groups, category, json).await
            }
            Self::Ingest {
                path,
                title,
                categories,
                groups,
                json,
            } => {
                let intel: Arc<dyn IntelligenceStore> = concrete;
                cmd_ingest(&cfg, store, intel, path, title, categories, groups, json).await
            }
            Self::List { organization, json } => {
                cmd_list(store, organization.as_deref(), json).await
            }
            Self::Get { id, json } => cmd_get(store, id, json).await,
            Self::Delete { id, hard } => cmd_delete(store, id, hard).await,
            Self::Drift { json } => cmd_drift(&cfg, store, json).await,
            Self::ReEmbed {
                include_current,
                dry_run,
                batch_size,
                json,
            } => cmd_re_embed(&cfg, store, include_current, dry_run, batch_size, json).await,
            // Handled above before the store was opened.
            Self::Status { .. } | Self::Enable | Self::Disable => unreachable!(),
            Self::Intelligence { document_id, json } => {
                let intel: Arc<dyn IntelligenceStore> = concrete;
                cmd_intelligence(intel, document_id, json).await
            }
            Self::Graph { group, limit, json } => {
                let intel: Arc<dyn IntelligenceStore> = concrete;
                cmd_graph(&cfg, intel, group, limit, json).await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Subcommand handlers — each renders its own output and returns an exit code.
// ---------------------------------------------------------------------------

async fn cmd_search(
    cfg: &KbConfig,
    store: Arc<dyn KbStore>,
    intel: Arc<dyn IntelligenceStore>,
    query: String,
    top: usize,
    groups: Vec<String>,
    category: Option<String>,
    json: bool,
) -> KbResult<i32> {
    let embedder = embed::make_provider(cfg)?;
    let reranker = rerank::make_reranker(cfg).map(Arc::from);

    let mut retriever = Retriever::new(cfg.clone(), store, embedder).with_intelligence(intel);
    if let Some(r) = reranker {
        retriever = retriever.with_reranker(r);
    }

    let result = retriever
        .retrieve(
            &query,
            RetrieveOptions {
                max_chunks: Some(top),
                category_filter: category,
                group_ids: groups,
                ..Default::default()
            },
        )
        .await?;

    if json {
        // `RetrievalResult` doesn't derive `Serialize` today (the `chunks`
        // field's `SearchResult` does, but the parent struct doesn't). Build
        // a flat ad-hoc JSON value so the surface is stable without forcing
        // a downstream derive change.
        let payload = serde_json::json!({
            "context": result.context,
            "sources": result.sources.iter().map(source_to_json).collect::<Vec<_>>(),
            "chunks": &result.chunks,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        // The TOON table below is the machine-readable index; `context` is
        // what the model actually reads from. It carries the full chunk text
        // under `[Title - Section]` headers plus the document inventory, and
        // it is the ONLY place a zero-chunk result still reports what the KB
        // contains — without it the agent reads an empty table and concludes
        // the KB is empty while `context` holds the document list.
        if !result.context.is_empty() {
            // `format_context_for_prompt` wraps the excerpts in the RAG
            // instruction block (cite inline, refuse cleanly, no invented
            // facts) and appends the source list. This is the block whose
            // module doc calls the wording load-bearing — before plan 089 it
            // had zero production callers and no model had ever received it.
            println!("{}", format_context_for_prompt(&result));
        }
        if result.chunks.is_empty() {
            if result.context.is_empty() {
                // No inventory either: the scope genuinely holds nothing.
                println!("No documents found in this knowledge-base scope.");
            } else {
                println!(
                    "No chunk crossed the relevance threshold. The documents \
                     listed above are present in scope — try a more specific \
                     query."
                );
            }
        }
        print!("{}", format_search_toon(&result.chunks));
    }
    Ok(0)
}

async fn cmd_ingest(
    cfg: &KbConfig,
    store: Arc<dyn KbStore>,
    intel: Arc<dyn IntelligenceStore>,
    path: PathBuf,
    title: Option<String>,
    categories: Vec<String>,
    groups: Vec<String>,
    json: bool,
) -> KbResult<i32> {
    let started = std::time::Instant::now();
    // 1. Extract content from disk.
    let processed = process_file(cfg, &path, ProcessingOptions::default()).await?;

    // 2. Pick a title — explicit override, else the file stem, else the
    //    full path string as last resort.
    let title = title.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| path.display().to_string())
    });

    // 3. Build chunks via the smart chunker. Category passed to the chunker
    //    is purely metadata — the first user-supplied category, or a neutral
    //    default that mirrors the KB's "no category" sentinel.
    let primary_category = categories
        .first()
        .cloned()
        .unwrap_or_else(|| "RANTAICLAW".to_string());
    let mut chunks = smart_chunk_document(
        &processed.content,
        &title,
        &primary_category,
        None,
        SmartChunkOptions::default(),
    );

    if chunks.is_empty() {
        // Fail-soft but explicit: the file was processable but produced no
        // chunks. Surface as operational error so the agent can decide.
        print_error_toon(
            "empty_chunks",
            &format!("no chunks produced from {}", path.display()),
        );
        return Ok(1);
    }

    // Opt-in contextual prefixes (plan 091): one chat call per document,
    // producing a one-line situating sentence per chunk. Fail-soft — every
    // error path returns empty strings and the chunk indexes without a
    // prefix. Must run BEFORE the embed map below so the prefix reaches the
    // vector through prepare_chunk_for_embedding (plan 090); writing it to
    // the DB after embedding would store text the vector never saw.
    // Credential: cfg.chat_api_key, resolved once at config construction
    // (plan 108) — console-configured keys reach this path.

    let bodies: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let prefixes = crate::kb::retrieve::contextual::generate_contextual_prefixes(
        cfg,
        &processed.content,
        &bodies,
    )
    .await;
    for (chunk, prefix) in chunks.iter_mut().zip(prefixes) {
        if !prefix.trim().is_empty() {
            chunk.metadata.contextual_prefix = Some(prefix);
        }
    }

    // 4. Embed each chunk's metadata-prefixed text (plan 090) — same recipe
    //    as the HTTP ingest and bulk re-embed paths.
    let embedder = embed::make_provider(cfg)?;
    let texts: Vec<String> = chunks
        .iter()
        .map(crate::kb::chunk::prepare::prepare_chunk_for_embedding)
        .collect();
    let embeddings = embedder.embed_many(&texts).await?;

    // 5. Persist document + chunks. The DocumentId is a fresh UUID so
    //    re-ingest of the same file produces a new row (idempotency is the
    //    caller's concern, per the plan).
    let doc_id = DocumentId(uuid::Uuid::new_v4().to_string());
    let now = chrono::Utc::now();
    let metadata = serde_json::json!({
        "source_path": path.display().to_string(),
        "groups": groups,
    });
    let document = Document {
        id: doc_id.clone(),
        title: title.clone(),
        content: processed.content.clone(),
        categories: categories.clone(),
        subcategory: None,
        metadata,
        s3_key: None,
        file_type: Some(format!("{:?}", processed.file_type).to_lowercase()),
        mime_type: None,
        file_size: tokio::fs::metadata(&path).await.ok().map(|m| m.len()),
        organization_id: None,
        created_by: None,
        session_id: None,
        artifact_type: None,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        retention_days: None,
        retrieval_count: 0,
        last_retrieved_at: None,
    };
    crate::kb::store::store_document_with_chunks(
        store.as_ref(),
        &document,
        &chunks,
        &embeddings,
        &crate::kb::chunk::prepare::tagged_model(embedder.model()),
    )
    .await?;

    // Document intelligence: gated on the `intelligence_enabled` flag (off by
    // default). Unlike the HTTP ingest — which detaches via `tokio::spawn` so
    // the response returns immediately — the CLI is a short-lived invocation
    // that exits as soon as `run` returns, so a detached task would be dropped
    // before it ran. We therefore await it inline. Extraction failure never
    // fails the ingest: errors are warned (doc id only), not propagated. The
    // API key is passed by value into the extractor and never logged.
    if cfg.intelligence_enabled {
        let extractor = build_intelligence_extractor(cfg);
        let chunk_refs: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();
        if let Err(e) = extract_document_intelligence(
            intel.as_ref(),
            &extractor,
            &doc_id.0,
            &chunk_refs,
            &cfg.intelligence_resolution,
        )
        .await
        {
            tracing::warn!(
                target: "kb::ingest",
                document_id = %doc_id.0,
                error = %e,
                "document intelligence extraction failed (non-fatal)"
            );
        }
    }

    // u128 → u64 cast: ingestion that takes more than ~584 million years
    // worth of milliseconds is not a realistic case.
    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = started.elapsed().as_millis() as u64;

    if json {
        let payload = serde_json::json!({
            "document": &document,
            "chunks_stored": chunks.len(),
            "elapsed_ms": elapsed_ms,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        let row = serde_json::json!({
            "document_id": doc_id.0,
            "chunks_stored": chunks.len(),
            "elapsed_ms": elapsed_ms,
        });
        print!(
            "{}",
            super::format_toon(
                "result",
                &[row],
                &["document_id", "chunks_stored", "elapsed_ms"],
            )
        );
    }
    Ok(0)
}

async fn cmd_list(
    store: Arc<dyn KbStore>,
    organization: Option<&str>,
    json: bool,
) -> KbResult<i32> {
    let docs = store.list_documents(organization).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&docs)?);
    } else {
        // Resolve chunk counts in one batched query so we don't N+1 the
        // store. Empty lists short-circuit.
        let ids: Vec<DocumentId> = docs.iter().map(|d| d.id.clone()).collect();
        let counts = if ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            store.chunk_counts(&ids).await?
        };
        let rows: Vec<serde_json::Value> = docs
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id.0,
                    "title": d.title,
                    "categories": d.categories.join("|"),
                    "created_at": d.created_at.to_rfc3339(),
                    "chunk_count": counts.get(&d.id).copied().unwrap_or(0),
                })
            })
            .collect();
        print!(
            "{}",
            super::format_toon(
                "documents",
                &rows,
                &["id", "title", "categories", "created_at", "chunk_count"],
            )
        );
    }
    Ok(0)
}

async fn cmd_get(store: Arc<dyn KbStore>, id: String, json: bool) -> KbResult<i32> {
    let document = store.get_document(&DocumentId(id.clone())).await?;
    let Some(doc) = document else {
        print_error_toon("not_found", &format!("document {id} not found"));
        return Ok(1);
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&doc)?);
    } else {
        let row = serde_json::json!({
            "id": doc.id.0,
            "title": doc.title,
            "categories": doc.categories.join("|"),
            "subcategory": doc.subcategory,
            "created_at": doc.created_at.to_rfc3339(),
            "updated_at": doc.updated_at.to_rfc3339(),
            "deleted_at": doc.deleted_at.map(|t| t.to_rfc3339()),
        });
        print!(
            "{}",
            super::format_toon(
                "document",
                &[row],
                &[
                    "id",
                    "title",
                    "categories",
                    "subcategory",
                    "created_at",
                    "updated_at",
                    "deleted_at",
                ],
            )
        );
    }
    Ok(0)
}

async fn cmd_delete(store: Arc<dyn KbStore>, id: String, hard: bool) -> KbResult<i32> {
    let soft = !hard;
    store.delete_document(&DocumentId(id.clone()), soft).await?;
    let row = serde_json::json!({
        "id": id,
        "mode": if hard { "hard" } else { "soft" },
    });
    print!("{}", super::format_toon("result", &[row], &["id", "mode"]));
    Ok(0)
}

async fn cmd_drift(cfg: &KbConfig, store: Arc<dyn KbStore>, json: bool) -> KbResult<i32> {
    let report = check_drift(cfg, &store).await?;
    if json {
        println!("{}", drift_to_json(&report));
    } else {
        let total: usize = report.by_model.iter().map(|(_, n)| n).sum();
        let row = serde_json::json!({
            "current_model": report.current_model,
            "total_chunks": total,
            "stale_chunks": report.stale_chunk_count,
            "in_sync": report.in_sync,
        });
        print!(
            "{}",
            super::format_toon(
                "drift",
                &[row],
                &["current_model", "total_chunks", "stale_chunks", "in_sync"],
            )
        );
    }
    Ok(0)
}

async fn cmd_re_embed(
    cfg: &KbConfig,
    store: Arc<dyn KbStore>,
    include_current: bool,
    dry_run: bool,
    batch_size: usize,
    json: bool,
) -> KbResult<i32> {
    let embedder = embed::make_provider(cfg)?;
    let started = std::time::Instant::now();
    let report = run_bulk_re_embed(
        cfg,
        &store,
        &embedder,
        BulkReEmbedOptions {
            batch_size,
            include_already_current: include_current,
            dry_run,
        },
    )
    .await?;
    // The bulk runner already tracks `elapsed_ms` on the report; keep the
    // CLI's wall-clock measurement only as a backstop in case a future
    // refactor zeroes it.
    let elapsed_ms = if report.elapsed_ms == 0 {
        #[allow(clippy::cast_possible_truncation)]
        {
            started.elapsed().as_millis() as u64
        }
    } else {
        report.elapsed_ms
    };

    if json {
        println!("{}", re_embed_to_json(&report, elapsed_ms));
    } else {
        let row = serde_json::json!({
            "examined": report.total_chunks_examined,
            "re_embedded": report.chunks_re_embedded,
            "skipped": report.chunks_skipped,
            "errors": report.errors.len(),
            "elapsed_ms": elapsed_ms,
        });
        print!(
            "{}",
            super::format_toon(
                "result",
                &[row],
                &["examined", "re_embedded", "skipped", "errors", "elapsed_ms"],
            )
        );
    }
    Ok(0)
}

async fn cmd_intelligence(
    intel: Arc<dyn IntelligenceStore>,
    document_id: String,
    json: bool,
) -> KbResult<i32> {
    let (entities, relations) = intel.intelligence_for_document(&document_id).await?;
    if json {
        let payload = serde_json::json!({
            "entities": entities.iter().map(entity_to_json).collect::<Vec<_>>(),
            "relations": relations.iter().map(relation_to_json).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        let entity_rows: Vec<serde_json::Value> = entities.iter().map(entity_to_json).collect();
        print!(
            "{}",
            super::format_toon(
                "entities",
                &entity_rows,
                &["id", "name", "entity_type", "confidence"],
            )
        );
        let relation_rows: Vec<serde_json::Value> =
            relations.iter().map(relation_to_json).collect();
        print!(
            "{}",
            super::format_toon(
                "relations",
                &relation_rows,
                &["source", "target", "relation_type", "confidence"],
            )
        );
    }
    Ok(0)
}

async fn cmd_graph(
    cfg: &KbConfig,
    intel: Arc<dyn IntelligenceStore>,
    group: Option<String>,
    limit: Option<usize>,
    json: bool,
) -> KbResult<i32> {
    let limit = limit.unwrap_or(cfg.graph_max_nodes);
    let graph = intel.graph(group.as_deref(), limit).await?;
    if json {
        println!("{}", graph_to_json(&graph));
    } else {
        let node_rows: Vec<serde_json::Value> = graph
            .nodes
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": n.id,
                    "name": n.name,
                    "entity_type": n.entity_type,
                    "degree": n.degree,
                    "doc_count": n.doc_count,
                })
            })
            .collect();
        print!(
            "{}",
            super::format_toon(
                "nodes",
                &node_rows,
                &["id", "name", "entity_type", "degree", "doc_count"],
            )
        );
        let edge_rows: Vec<serde_json::Value> = graph
            .edges
            .iter()
            .map(|e| {
                serde_json::json!({
                    "source": e.source,
                    "target": e.target,
                    "relation_type": e.relation_type,
                    "weight": e.weight,
                })
            })
            .collect();
        print!(
            "{}",
            super::format_toon(
                "edges",
                &edge_rows,
                &["source", "target", "relation_type", "weight"],
            ),
        );
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the document-intelligence LLM extractor from KB config — the same
/// fields the HTTP ingest path uses: `intelligence_model`, the OpenRouter chat
/// endpoint, and the resolved embedding/`OPENROUTER_API_KEY` credential. The
/// key is passed by value into the extractor and never logged.
fn build_intelligence_extractor(cfg: &KbConfig) -> CombinedLlmExtractor {
    CombinedLlmExtractor::new(
        cfg.intelligence_model.clone(),
        cfg.openrouter_chat_url.clone(),
        KbConfig::resolve_key(&cfg.embedding_api_key),
    )
}

/// JSON/TOON row for an entity. `entity_type` is the string form of the typed
/// enum (via `EntityType::as_str`).
fn entity_to_json(e: &Entity) -> serde_json::Value {
    serde_json::json!({
        "id": e.id,
        "name": e.name,
        "entity_type": e.entity_type.as_str(),
        "confidence": e.confidence,
    })
}

/// JSON/TOON row for a relation, keyed by source/target entity ids.
fn relation_to_json(r: &Relation) -> serde_json::Value {
    serde_json::json!({
        "id": r.id,
        "source": r.source_entity_id,
        "target": r.target_entity_id,
        "relation_type": r.relation_type.as_str(),
        "confidence": r.confidence,
    })
}

fn graph_to_json(g: &Graph) -> String {
    let payload = serde_json::json!({
        "nodes": g.nodes.iter().map(|n| serde_json::json!({
            "id": n.id,
            "name": n.name,
            "entity_type": n.entity_type,
            "degree": n.degree,
            "doc_count": n.doc_count,
        })).collect::<Vec<_>>(),
        "edges": g.edges.iter().map(|e| serde_json::json!({
            "source": e.source,
            "target": e.target,
            "relation_type": e.relation_type,
            "weight": e.weight,
        })).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
}

/// Resolve the on-disk path for the KB SQLite database.
///
/// Precedence:
/// 1. `KB_DB_PATH` env var (explicit override, used by tests + power users).
/// 2. The active profile's `profiles/<name>/kb.db` — so `--profile work` and
///    `--profile personal` keep separate knowledge bases. `resolve_active_name`
///    is pure (no dir creation), keeping this cheap on the per-turn ambient
///    path; `SqliteStore::open` creates the parent dir on first write.
/// 3. `./kb.db` in the current working directory — final fallback when even
///    the user's HOME is unavailable (CI containers, embedded systems).
pub(crate) fn resolve_kb_db_path() -> PathBuf {
    if let Ok(env_path) = std::env::var("KB_DB_PATH") {
        if !env_path.is_empty() {
            return PathBuf::from(env_path);
        }
    }
    // Guard on HOME so we never panic in `paths::home_dir` on hosts without
    // one — fall through to the cwd default instead.
    if directories::UserDirs::new().is_some() {
        let name = crate::profile::ProfileManager::resolve_active_name();
        return crate::profile::paths::kb_db(&name);
    }
    PathBuf::from("./kb.db")
}

/// Open the SQLite store as a concrete `Arc<SqliteStore>` so the caller can
/// alias it through both the [`KbStore`] and [`IntelligenceStore`] seams (an
/// `Arc<dyn KbStore>` can't be upcast to `Arc<dyn IntelligenceStore>`).
async fn open_store(cfg: &KbConfig) -> KbResult<Arc<SqliteStore>> {
    let path = resolve_kb_db_path();
    let store = SqliteStore::open(&path, cfg.embedding_dim).await?;
    Ok(Arc::new(store))
}

/// Print a TOON-formatted operational-error block to stdout.
///
/// Per AXI principle 6, everything goes to stdout — operators grep one
/// stream, agents parse one stream.
/// `kb status` — no store open (that would rewrite kb_meta, plan 098):
/// db_path existence + a read-only row count instead.
fn cmd_status_sync(
    enabled: bool,
    embedding_api_key: Option<&str>,
    vision_api_key: Option<&str>,
    json: bool,
) -> KbResult<i32> {
    let emb_cfg = embedding_api_key.unwrap_or_default();
    let source = if std::env::var("KB_EMBEDDING_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        "env"
    } else if !emb_cfg.is_empty() {
        "config"
    } else if std::env::var("OPENROUTER_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        "openrouter_env"
    } else {
        "none"
    };
    let vision_configured = vision_api_key.map(|v| !v.is_empty()).unwrap_or(false)
        || std::env::var("KB_EXTRACT_VISION_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
    let db_path = resolve_kb_db_path();
    // Read-only count: never SqliteStore::open here.
    let document_count: Option<i64> = std::path::Path::new(&db_path)
        .exists()
        .then(|| {
            rusqlite::Connection::open_with_flags(
                &db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM document WHERE deleted_at IS NULL",
                    [],
                    |r| r.get(0),
                )
                .ok()
            })
        })
        .flatten();

    let row = serde_json::json!({
        "enabled": enabled,
        "embedding_configured": source != "none",
        "vision_configured": vision_configured,
        "source": source,
        "db_path": db_path.display().to_string(),
        "document_count": document_count,
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&row)?);
    } else {
        print!(
            "{}",
            super::format_toon(
                "status",
                &[row],
                &[
                    "enabled",
                    "embedding_configured",
                    "vision_configured",
                    "source",
                    "db_path",
                    "document_count",
                ],
            )
        );
    }
    Ok(0)
}

/// `kb enable` / `kb disable` — the ONLY KbCommand arms that write config.
/// Load fresh from disk, mutate, save (same discipline as the gateway's
/// PUT): persisting a stale in-memory snapshot would clobber concurrent
/// edits. Enable refuses when no embedding key resolves anywhere, so it
/// cannot persist a config the gateway then 503s on (plan 103 agreement).
async fn cmd_set_enabled(enabled: bool) -> KbResult<i32> {
    let mut fresh = crate::config::Config::load_or_init()
        .await
        .map_err(|e| crate::kb::KbError::Other(format!("config load: {e}")))?;
    if enabled {
        let key_resolves = fresh
            .knowledge
            .embedding_api_key
            .as_deref()
            .map(|v| !v.is_empty())
            .unwrap_or(false)
            || std::env::var("KB_EMBEDDING_API_KEY")
                .map(|v| !v.is_empty())
                .unwrap_or(false)
            || std::env::var("OPENROUTER_API_KEY")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
        if !key_resolves {
            print_error_toon(
                "no_credential",
                "cannot activate the knowledge base without an embedding key;                  run `rantaiclaw setup knowledge` or set KB_EMBEDDING_API_KEY",
            );
            return Ok(1);
        }
    }
    fresh.knowledge.enabled = enabled;
    fresh
        .save()
        .await
        .map_err(|e| crate::kb::KbError::Other(format!("config save: {e}")))?;
    let row = serde_json::json!({
        "enabled": enabled,
        "note": if enabled { "knowledge base activated" } else { "deactivated; credentials kept" },
    });
    print!(
        "{}",
        super::format_toon("result", &[row], &["enabled", "note"])
    );
    Ok(0)
}

fn print_error_toon(code: &str, message: &str) {
    let row = serde_json::json!({ "code": code, "message": message });
    print!(
        "{}",
        super::format_toon("error", &[row], &["code", "message"]),
    );
}

/// Render the search-chunks TOON block. Extracted so end-to-end tests can
/// assert directly on the formatter output without spawning a binary.
fn format_search_toon(chunks: &[SearchResult]) -> String {
    let rows: Vec<serde_json::Value> = chunks
        .iter()
        .map(|c| {
            serde_json::json!({
                "document": c.document_title,
                "section": c.section.clone().unwrap_or_default(),
                "score": c.similarity,
                "content_preview": truncate_for_preview(&c.content),
            })
        })
        .collect();
    super::format_toon(
        "chunks",
        &rows,
        &["document", "section", "score", "content_preview"],
    )
}

/// Truncate `content` to [`CONTENT_PREVIEW_CHARS`] chars on a `char`
/// boundary (so multi-byte UTF-8 stays valid). Adds an ellipsis when
/// truncation happens.
fn truncate_for_preview(content: &str) -> String {
    let mut out = String::new();
    for (i, ch) in content.chars().enumerate() {
        if i >= CONTENT_PREVIEW_CHARS {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

fn source_to_json(s: &SourceRef) -> serde_json::Value {
    serde_json::json!({
        "document_title": s.document_title,
        "section": s.section,
        "categories": s.categories,
    })
}

fn drift_to_json(r: &DriftReport) -> String {
    let total: usize = r.by_model.iter().map(|(_, n)| n).sum();
    let payload = serde_json::json!({
        "current_model": r.current_model,
        "by_model": r.by_model.iter().map(|(m, n)| serde_json::json!({
            "model": m,
            "count": n,
        })).collect::<Vec<_>>(),
        "total_chunks": total,
        "stale_chunks": r.stale_chunk_count,
        "in_sync": r.in_sync,
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
}

fn re_embed_to_json(r: &BulkReEmbedReport, elapsed_ms: u64) -> String {
    let payload = serde_json::json!({
        "examined": r.total_chunks_examined,
        "re_embedded": r.chunks_re_embedded,
        "skipped": r.chunks_skipped,
        "errors": r.errors,
        "elapsed_ms": elapsed_ms,
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use crate::kb::store::{Graph, GraphEdge};

    #[test]
    fn graph_to_json_includes_edge_weight() {
        let g = Graph {
            edges: vec![GraphEdge {
                source: "a".into(),
                target: "b".into(),
                relation_type: "RelatedTo".into(),
                weight: 3,
            }],
            ..Default::default()
        };
        let v: serde_json::Value = serde_json::from_str(&super::graph_to_json(&g)).unwrap();
        assert_eq!(v["edges"][0]["weight"], 3);
    }
}
