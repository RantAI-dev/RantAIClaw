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
//! Storage path resolution: `KB_DB_PATH` env var → XDG data dir
//! (`~/.local/share/rantaiclaw/kb.db` on Linux) → `./kb.db` cwd fallback.
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
use crate::kb::maintenance::{
    check_drift, run_bulk_re_embed, BulkReEmbedOptions, BulkReEmbedReport, DriftReport,
};
use crate::kb::rerank;
use crate::kb::retrieve::{RetrieveOptions, Retriever, SourceRef};
use crate::kb::store::sqlite::SqliteStore;
use crate::kb::store::KbStore;
use crate::kb::{Document, DocumentId, KbConfig, KbResult, SearchResult};

/// Preview length used when rendering chunk content into TOON rows. Keeps
/// the row narrow enough to be useful to a downstream LLM without blowing
/// the per-cell budget. 120 chars matches the value documented in the plan.
const CONTENT_PREVIEW_CHARS: usize = 120;

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
}

impl KbCommand {
    /// Run the subcommand. Returns:
    /// - `Ok(0)` — success.
    /// - `Ok(1)` — operational failure already reported on stdout.
    /// - `Err(KbError)` — internal failure; caller decides how to render.
    pub async fn run(self) -> KbResult<i32> {
        let cfg = KbConfig::from_env()?;
        let store = open_store(&cfg).await?;

        match self {
            Self::Search {
                query,
                top,
                groups,
                category,
                json,
            } => cmd_search(&cfg, store, query, top, groups, category, json).await,
            Self::Ingest {
                path,
                title,
                categories,
                groups,
                json,
            } => cmd_ingest(&cfg, store, path, title, categories, groups, json).await,
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
        }
    }
}

// ---------------------------------------------------------------------------
// Subcommand handlers — each renders its own output and returns an exit code.
// ---------------------------------------------------------------------------

async fn cmd_search(
    cfg: &KbConfig,
    store: Arc<dyn KbStore>,
    query: String,
    top: usize,
    groups: Vec<String>,
    category: Option<String>,
    json: bool,
) -> KbResult<i32> {
    let embedder = embed::make_provider(cfg)?;
    let reranker = rerank::make_reranker(cfg).map(Arc::from);

    let mut retriever = Retriever::new(cfg.clone(), store, embedder);
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
        print!("{}", format_search_toon(&result.chunks));
    }
    Ok(0)
}

async fn cmd_ingest(
    cfg: &KbConfig,
    store: Arc<dyn KbStore>,
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
    let chunks = smart_chunk_document(
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

    // 4. Embed each chunk's content.
    let embedder = embed::make_provider(cfg)?;
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
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
    store.create_document(&document).await?;
    store
        .store_chunks(&doc_id, &chunks, &embeddings, embedder.model())
        .await?;

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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the on-disk path for the KB SQLite database.
///
/// Precedence:
/// 1. `KB_DB_PATH` env var (explicit override, used by tests + power users).
/// 2. `directories::ProjectDirs` data dir
///    (`~/.local/share/rantaiclaw/kb.db` on Linux, `~/Library/Application
///    Support/rantaiclaw/kb.db` on macOS, etc.).
/// 3. `./kb.db` in the current working directory — final fallback when
///    even the user's HOME is unavailable (CI containers, embedded
///    systems).
pub(crate) fn resolve_kb_db_path() -> PathBuf {
    if let Ok(env_path) = std::env::var("KB_DB_PATH") {
        if !env_path.is_empty() {
            return PathBuf::from(env_path);
        }
    }
    if let Some(dirs) = directories::ProjectDirs::from("", "", "rantaiclaw") {
        return dirs.data_dir().join("kb.db");
    }
    PathBuf::from("./kb.db")
}

async fn open_store(cfg: &KbConfig) -> KbResult<Arc<dyn KbStore>> {
    let path = resolve_kb_db_path();
    let store = SqliteStore::open(&path, cfg.embedding_dim).await?;
    Ok(Arc::new(store))
}

/// Print a TOON-formatted operational-error block to stdout.
///
/// Per AXI principle 6, everything goes to stdout — operators grep one
/// stream, agents parse one stream.
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
