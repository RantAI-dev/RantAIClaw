//! HTTP surface for the Knowledge Base — mounted under `/api/v1/kb/*` by the
//! gateway. Mirrors the CLI subcommand layout (`crate::kb::axi::cli`) but emits
//! JSON instead of TOON because HTTP callers (web clients, scripted agents)
//! consume JSON.
//!
//! **Auth**: every handler calls [`check_auth`](crate::gateway::api_v1)-shaped
//! pairing check via this module's `check_auth` helper, mirroring the rules
//! in `api_v1.rs`. When the gateway is configured `require_pairing = false`,
//! requests pass through.
//!
//! **KB context**: the heavy KB plumbing (config, sqlite-vec store, embedder)
//! is built once per process via [`KB_CTX`], a [`OnceCell`]. The first request
//! to land triggers initialization; subsequent requests share the same store
//! handle and provider client. Init failures cache as `Err` and surface as
//! 503 on every call until the process restarts — this is intentional: we
//! fail fast rather than re-attempt on every webhook tick. Operators fix the
//! env and bounce the gateway.
//!
//! Feature-gated: the entire module is `#[cfg(feature = "kb")]` at the
//! gateway-import level so non-KB builds never see these routes or pay the
//! compile cost.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use crate::gateway::AppState;
use crate::kb::chunk::{smart_chunk_document, SmartChunkOptions};
use crate::kb::embed::{self, EmbeddingProvider};
use crate::kb::file::{detect_file_type, process_file, ProcessingOptions};
use crate::kb::intelligence::extract::llm::CombinedLlmExtractor;
use crate::kb::intelligence::types::{Entity, Relation};
use crate::kb::intelligence::{extract_document_intelligence, IntelligenceSummary};
use crate::kb::maintenance::{
    check_drift, run_bulk_re_embed, BulkReEmbedOptions, BulkReEmbedReport, DriftReport,
};
use crate::kb::rerank::{self, Reranker};
use crate::kb::retrieve::{RetrieveOptions, Retriever, SourceRef};
use crate::kb::store::sqlite::SqliteStore;
use crate::kb::store::{Graph, IntelligenceStore, KbStore};
use crate::kb::{Document, DocumentId, KbConfig, KbError, KbGroup, KbGroupSummary};

/// Upload size cap for the KB ingest route. 32 MiB covers a typical
/// scientific PDF / large markdown bundle without giving an unauthenticated
/// caller free buffer pool space. Operators who need more can fork.
const KB_UPLOAD_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Per-request timeout for the KB routes. Document ingest embeds every chunk
/// via the (possibly remote) embedding provider, so it needs far longer than
/// the gateway-wide `REQUEST_TIMEOUT_SECS` (120 s). 600 s covers a large
/// multi-MB document; genuinely huge corpora should move to async ingest.
const KB_REQUEST_TIMEOUT_SECS: u64 = 600;

/// Absolute ceiling on the number of graph nodes a single `/graph` request may
/// pull, independent of the configurable `KB_GRAPH_MAX_NODES` default. A resource
/// guard: 25× the 200-node default is ample for exploration, and it bounds the
/// node set only (the normal path requests ~200).
const GRAPH_HARD_CAP: usize = 5000;

/// Resolve the effective `/graph` node limit: the caller's request or the
/// configured default, clamped to [`GRAPH_HARD_CAP`].
fn effective_graph_limit(requested: Option<usize>, default: usize) -> usize {
    requested.unwrap_or(default).min(GRAPH_HARD_CAP)
}

/// Build the `/api/v1/kb/*` router. Merged into the main gateway router by
/// `gateway::run_gateway` so it shares state and timeout layers.
///
/// The gateway-wide [`RequestBodyLimitLayer`] (64 KiB) is too small for the
/// ingest route (PDFs / office docs / large markdown). We disable the default
/// body limit on this subtree and apply a larger per-route limit
/// ([`KB_UPLOAD_MAX_BYTES`]). Read-only routes (search/list/get/drift) still
/// receive small JSON bodies; the bigger limit is harmless because axum drops
/// the body if it exceeds Content-Length before any handler is called.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/kb/search", post(search))
        .route("/api/v1/kb/documents", post(ingest).get(list))
        .route("/api/v1/kb/documents/{id}", get(get_doc).delete(delete_doc))
        .route("/api/v1/kb/groups", get(list_groups).post(create_group))
        .route(
            "/api/v1/kb/groups/{id}",
            get(get_group).put(update_group).delete(delete_group),
        )
        .route(
            "/api/v1/kb/groups/{id}/documents",
            get(list_group_documents).post(add_group_document),
        )
        .route(
            "/api/v1/kb/groups/{id}/documents/{doc_id}",
            axum::routing::delete(remove_group_document),
        )
        .route("/api/v1/kb/drift", get(drift))
        .route("/api/v1/kb/re-embed", post(re_embed))
        .route(
            "/api/v1/kb/documents/{id}/intelligence",
            get(get_intelligence),
        )
        .route(
            "/api/v1/kb/documents/{id}/re-extract",
            post(re_extract_document),
        )
        .route("/api/v1/kb/graph", get(get_graph))
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(KB_UPLOAD_MAX_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(KB_REQUEST_TIMEOUT_SECS),
        ))
}

// ────────────────────────────────────────────────────────────────────────────
// KB context — lazy, process-wide, OnceCell-cached.
// ────────────────────────────────────────────────────────────────────────────

/// Bundled KB plumbing shared across handler invocations. Built once via
/// [`ensure_kb_ctx`]; never re-built within a process.
pub(crate) struct KbContext {
    pub cfg: KbConfig,
    pub store: Arc<dyn KbStore>,
    /// Same concrete `SqliteStore` as `store`, viewed through the
    /// [`IntelligenceStore`] seam. Held as a second handle because
    /// `Arc<dyn KbStore>` can't be upcast to `Arc<dyn IntelligenceStore>`;
    /// both alias one `SqliteStore` so they share state.
    pub intel: Arc<dyn IntelligenceStore>,
    pub embedder: Arc<dyn EmbeddingProvider>,
    /// Optional reranker. `None` when rerank is disabled (the default).
    pub reranker: Option<Arc<dyn Reranker>>,
}

/// Cached entry — the resolved DB path acts as the cache key. When the
/// path changes (only realistic in test bins that mutate `KB_DB_PATH`), the
/// next request builds a fresh context. In production the path is stable
/// for the gateway's lifetime, so the cache effectively pins one
/// [`KbContext`].
struct CachedCtx {
    path: PathBuf,
    ctx: Result<Arc<KbContext>, String>,
}

/// Cache cell. Holding the cell behind a `tokio::sync::Mutex` rather than a
/// `OnceCell` lets us (1) await the async constructor inside the
/// critical section and (2) rebuild when the cached path no longer matches
/// the env. The mutex stays cold in the happy path: handlers hit the cache
/// hit branch after the first request and never touch the lock body.
static KB_CTX: tokio::sync::Mutex<Option<CachedCtx>> = tokio::sync::Mutex::const_new(None);

async fn ensure_kb_ctx(state: &crate::gateway::AppState) -> Result<Arc<KbContext>, ApiError> {
    let path = super::cli::resolve_kb_db_path();
    // parking_lot guard is !Send — clone keys inside a scoped block, drop the
    // guard before the async build below.
    let (emb, vis) = {
        let c = state.config.lock();
        (
            c.knowledge.embedding_api_key.clone(),
            c.knowledge.vision_api_key.clone(),
        )
    };
    let mut guard = KB_CTX.lock().await;
    if let Some(cached) = guard.as_ref() {
        if cached.path == path {
            return match &cached.ctx {
                Ok(ctx) => Ok(Arc::clone(ctx)),
                Err(msg) if msg.starts_with("kb_not_configured") => Err(
                    ApiError::service_unavailable("kb_not_configured", msg.clone()),
                ),
                Err(msg) => Err(ApiError::service_unavailable("kb_unavailable", msg.clone())),
            };
        }
    }
    // Rebuild. Failures cache as `Err` so we don't hammer the embed/auth
    // endpoint on every retry; operators fix the env and bounce the
    // gateway, which clears the static.
    let outcome = build_ctx(&path, emb, vis).await;
    let snapshot = outcome.clone();
    *guard = Some(CachedCtx { path, ctx: outcome });
    match snapshot {
        Ok(ctx) => Ok(ctx),
        Err(msg) if msg.starts_with("kb_not_configured") => {
            Err(ApiError::service_unavailable("kb_not_configured", msg))
        }
        Err(msg) => Err(ApiError::service_unavailable("kb_unavailable", msg)),
    }
}

/// Drop the cached KB context so the next `ensure_kb_ctx` rebuilds it with the
/// current config (used after a `PUT /api/v1/config/knowledge` key change).
pub async fn clear_kb_ctx() {
    *KB_CTX.lock().await = None;
}

async fn build_ctx(
    path: &std::path::Path,
    embedding_key: Option<String>,
    vision_key: Option<String>,
) -> Result<Arc<KbContext>, String> {
    let cfg = KbConfig::from_env_with_keys(embedding_key.as_deref(), vision_key.as_deref())
        .map_err(|e| format!("kb config: {e}"))?;
    // If neither config nor env nor the OPENROUTER fallback yields an embedding
    // key, surface an actionable "not configured" error instead of a raw
    // provider auth failure downstream.
    if KbConfig::resolve_key(&cfg.embedding_api_key).is_empty() {
        return Err("kb_not_configured: no embedding API key. Add one via \
                    `rantaiclaw setup knowledge` or set KB_EMBEDDING_API_KEY."
            .into());
    }
    let store = SqliteStore::open(path, cfg.embedding_dim)
        .await
        .map_err(|e| format!("sqlite open ({}): {e}", path.display()))?;
    // Build the concrete store once, then alias it through both trait views.
    // `SqliteStore` implements `KbStore` and `IntelligenceStore`; sharing one
    // `Arc<SqliteStore>` keeps a single DB handle behind both seams.
    let concrete = Arc::new(store);
    let store: Arc<dyn KbStore> = concrete.clone();
    let intel: Arc<dyn IntelligenceStore> = concrete;
    let embedder = embed::make_provider(&cfg).map_err(|e| format!("embed provider: {e}"))?;
    let reranker = rerank::make_reranker(&cfg).map(Arc::from);
    Ok(Arc::new(KbContext {
        cfg,
        store,
        intel,
        embedder,
        reranker,
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// Auth helper — mirrors api_v1::check_auth so behavior stays uniform.
// ────────────────────────────────────────────────────────────────────────────

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if !state.pairing.require_pairing() {
        return Ok(());
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        });
    match token {
        Some(t) if state.pairing.is_authenticated(t) => Ok(()),
        _ => Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            body: ErrorBody {
                error: "unauthorized".into(),
                detail: Some(
                    "Pair via POST /pair, then send `Authorization: Bearer <token>`.".into(),
                ),
            },
        }),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Error plumbing — same shape as api_v1::ErrorBody.
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

struct ApiError {
    status: StatusCode,
    body: ErrorBody,
}

impl ApiError {
    fn service_unavailable(code: &str, detail: String) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: ErrorBody {
                error: code.into(),
                detail: Some(detail),
            },
        }
    }

    fn bad_request(detail: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: ErrorBody {
                error: "bad_request".into(),
                detail: Some(detail),
            },
        }
    }

    fn not_found(detail: String) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: ErrorBody {
                error: "not_found".into(),
                detail: Some(detail),
            },
        }
    }
}

impl From<ApiError> for (StatusCode, Json<ErrorBody>) {
    fn from(err: ApiError) -> Self {
        (err.status, Json(err.body))
    }
}

impl From<KbError> for ApiError {
    /// Map kb errors to HTTP status codes. NotFound → 404, Config /
    /// UnsupportedFileType → 400, embedding/chat upstream → 502, anything
    /// else → 500. Detail surfaces the error's `Display` rendering; we never
    /// log raw API keys because KbError variants don't carry them.
    fn from(err: KbError) -> Self {
        match err {
            KbError::NotFound(msg) => Self {
                status: StatusCode::NOT_FOUND,
                body: ErrorBody {
                    error: "not_found".into(),
                    detail: Some(msg),
                },
            },
            KbError::Config(msg) => Self {
                status: StatusCode::BAD_REQUEST,
                body: ErrorBody {
                    error: "bad_request".into(),
                    detail: Some(msg),
                },
            },
            KbError::UnsupportedFileType(msg) => Self {
                status: StatusCode::BAD_REQUEST,
                body: ErrorBody {
                    error: "unsupported_file_type".into(),
                    detail: Some(msg),
                },
            },
            KbError::EmbeddingApi { status, .. } => Self {
                status: StatusCode::BAD_GATEWAY,
                body: ErrorBody {
                    error: "embedding_upstream".into(),
                    detail: Some(format!("upstream embedding API returned status {status}")),
                },
            },
            KbError::ChatApi { status, .. } => Self {
                status: StatusCode::BAD_GATEWAY,
                body: ErrorBody {
                    error: "chat_upstream".into(),
                    detail: Some(format!("upstream chat API returned status {status}")),
                },
            },
            other => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: ErrorBody {
                    error: "internal_error".into(),
                    detail: Some(format!("{other}")),
                },
            },
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Request / response shapes.
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SearchRequest {
    query: String,
    #[serde(default)]
    top: Option<usize>,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    min_similarity: Option<f32>,
}

#[derive(Debug, Serialize)]
struct SearchResponse {
    context: String,
    sources: Vec<SourceJson>,
    chunks: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct SourceJson {
    document_title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    section: Option<String>,
    categories: Vec<String>,
}

impl From<&SourceRef> for SourceJson {
    fn from(s: &SourceRef) -> Self {
        Self {
            document_title: s.document_title.clone(),
            section: s.section.clone(),
            categories: s.categories.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct IngestResponse {
    document_id: String,
    chunks_stored: usize,
    elapsed_ms: u64,
    /// Characters of text extracted from the upload (so the UI can warn when a
    /// document extracted poorly).
    chars_extracted: usize,
    /// PDF page count when known.
    pages: Option<u32>,
    /// `true` when the extraction looks near-empty for the page count.
    low_text_density: bool,
}

#[derive(Debug, Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    organization: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct DeleteQuery {
    /// When `true`, permanently remove rows. Defaults to soft-delete.
    #[serde(default)]
    hard: Option<bool>,
}

#[derive(Debug, Serialize)]
struct DeleteResponse {
    id: String,
    mode: &'static str,
}

#[derive(Debug, Serialize)]
struct DriftResponse {
    current_model: String,
    by_model: Vec<DriftByModel>,
    total_chunks: usize,
    stale_chunks: usize,
    in_sync: bool,
}

#[derive(Debug, Serialize)]
struct DriftByModel {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    count: usize,
}

impl From<&DriftReport> for DriftResponse {
    fn from(r: &DriftReport) -> Self {
        let total: usize = r.by_model.iter().map(|(_, n)| n).sum();
        Self {
            current_model: r.current_model.clone(),
            by_model: r
                .by_model
                .iter()
                .map(|(m, n)| DriftByModel {
                    model: m.clone(),
                    count: *n,
                })
                .collect(),
            total_chunks: total,
            stale_chunks: r.stale_chunk_count,
            in_sync: r.in_sync,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct ReEmbedRequest {
    #[serde(default)]
    include_current: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    batch_size: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ReEmbedResponse {
    total_chunks_examined: usize,
    chunks_re_embedded: usize,
    chunks_skipped: usize,
    errors: Vec<String>,
    elapsed_ms: u64,
}

impl From<&BulkReEmbedReport> for ReEmbedResponse {
    fn from(r: &BulkReEmbedReport) -> Self {
        Self {
            total_chunks_examined: r.total_chunks_examined,
            chunks_re_embedded: r.chunks_re_embedded,
            chunks_skipped: r.chunks_skipped,
            errors: r.errors.clone(),
            elapsed_ms: r.elapsed_ms,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Intelligence / graph response shapes.
// ────────────────────────────────────────────────────────────────────────────

/// JSON-friendly entity view. `entity_type` is the string form of the typed
/// enum (via `EntityType::as_str`).
#[derive(Debug, Serialize)]
struct EntityJson {
    id: String,
    name: String,
    entity_type: String,
    confidence: f32,
}

impl From<&Entity> for EntityJson {
    fn from(e: &Entity) -> Self {
        Self {
            id: e.id.clone(),
            name: e.name.clone(),
            entity_type: e.entity_type.as_str().into_owned(),
            confidence: e.confidence,
        }
    }
}

/// JSON-friendly relation view. `relation_type` is the string form of the
/// typed enum.
#[derive(Debug, Serialize)]
struct RelationJson {
    id: String,
    source: String,
    target: String,
    relation_type: String,
    confidence: f32,
}

impl From<&Relation> for RelationJson {
    fn from(r: &Relation) -> Self {
        Self {
            id: r.id.clone(),
            source: r.source_entity_id.clone(),
            target: r.target_entity_id.clone(),
            relation_type: r.relation_type.as_str().into_owned(),
            confidence: r.confidence,
        }
    }
}

/// Aggregate counts over an entity/relation set, keyed by type.
#[derive(Debug, Serialize)]
struct IntelligenceStats {
    total_entities: usize,
    total_relations: usize,
    entity_types: std::collections::BTreeMap<String, usize>,
    relation_types: std::collections::BTreeMap<String, usize>,
}

#[derive(Debug, Serialize)]
struct IntelligenceResponse {
    entities: Vec<EntityJson>,
    relations: Vec<RelationJson>,
    stats: IntelligenceStats,
}

impl IntelligenceResponse {
    fn build(entities: &[Entity], relations: &[Relation]) -> Self {
        let mut entity_types: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for e in entities {
            *entity_types
                .entry(e.entity_type.as_str().into_owned())
                .or_default() += 1;
        }
        let mut relation_types: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for r in relations {
            *relation_types
                .entry(r.relation_type.as_str().into_owned())
                .or_default() += 1;
        }
        Self {
            entities: entities.iter().map(EntityJson::from).collect(),
            relations: relations.iter().map(RelationJson::from).collect(),
            stats: IntelligenceStats {
                total_entities: entities.len(),
                total_relations: relations.len(),
                entity_types,
                relation_types,
            },
        }
    }
}

/// JSON-friendly graph node — flattens the denormalized fan-out counts.
#[derive(Debug, Serialize)]
struct GraphNodeJson {
    id: String,
    name: String,
    entity_type: String,
    degree: usize,
    doc_count: usize,
}

#[derive(Debug, Serialize)]
struct GraphEdgeJson {
    source: String,
    target: String,
    relation_type: String,
    weight: usize,
}

#[derive(Debug, Serialize)]
struct GraphStats {
    total_nodes: usize,
    total_edges: usize,
    /// Scope-wide (group-scoped when `?group=` is set, else corpus-wide)
    /// entity count, computed before the top-N node cap `total_nodes` reflects.
    corpus_entities: usize,
    /// Scope-wide distinct `(source, target, relation_type)` relation count.
    corpus_relations: usize,
}

#[derive(Debug, Serialize)]
struct GraphResponse {
    nodes: Vec<GraphNodeJson>,
    edges: Vec<GraphEdgeJson>,
    stats: GraphStats,
}

impl From<Graph> for GraphResponse {
    fn from(g: Graph) -> Self {
        let stats = GraphStats {
            total_nodes: g.nodes.len(),
            total_edges: g.edges.len(),
            corpus_entities: g.total_entities,
            corpus_relations: g.total_relations,
        };
        Self {
            nodes: g
                .nodes
                .into_iter()
                .map(|n| GraphNodeJson {
                    id: n.id,
                    name: n.name,
                    entity_type: n.entity_type,
                    degree: n.degree,
                    doc_count: n.doc_count,
                })
                .collect(),
            edges: g
                .edges
                .into_iter()
                .map(|e| GraphEdgeJson {
                    source: e.source,
                    target: e.target,
                    relation_type: e.relation_type,
                    weight: e.weight,
                })
                .collect(),
            stats,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct GraphQuery {
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ReExtractResponse {
    document_id: String,
    entities: usize,
    relations: usize,
}

impl ReExtractResponse {
    fn new(document_id: String, summary: IntelligenceSummary) -> Self {
        Self {
            document_id,
            entities: summary.entities,
            relations: summary.relations,
        }
    }
}

/// Build the document-intelligence LLM extractor from KB config. Reuses the
/// same chat endpoint (`openrouter_chat_url`) and credential resolution
/// (`resolve_key` over the embedding key → `OPENROUTER_API_KEY`) that the
/// reranker / embedders use, so intelligence works with the same provider
/// setup. The key is passed by value into the extractor and never logged.
fn build_intelligence_extractor(cfg: &KbConfig) -> CombinedLlmExtractor {
    CombinedLlmExtractor::new(
        cfg.intelligence_model.clone(),
        cfg.openrouter_chat_url.clone(),
        KbConfig::resolve_key(&cfg.embedding_api_key),
    )
}

// ────────────────────────────────────────────────────────────────────────────
// Handlers.
// ────────────────────────────────────────────────────────────────────────────

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    if body.query.trim().is_empty() {
        return Err(ApiError::bad_request("query must not be empty".into()).into());
    }
    let ctx = ensure_kb_ctx(&state).await?;

    let mut retriever = Retriever::new(
        ctx.cfg.clone(),
        Arc::clone(&ctx.store),
        Arc::clone(&ctx.embedder),
    )
    .with_intelligence(Arc::clone(&ctx.intel));
    if let Some(r) = ctx.reranker.as_ref() {
        retriever = retriever.with_reranker(Arc::clone(r));
    }

    let result = retriever
        .retrieve(
            &body.query,
            RetrieveOptions {
                min_similarity: body.min_similarity,
                max_chunks: body.top,
                category_filter: body.category,
                group_ids: body.groups,
            },
        )
        .await
        .map_err(ApiError::from)?;

    let chunks = result
        .chunks
        .iter()
        .map(|c| serde_json::to_value(c).unwrap_or(serde_json::Value::Null))
        .collect();

    Ok(Json(SearchResponse {
        context: result.context,
        sources: result.sources.iter().map(SourceJson::from).collect(),
        chunks,
    }))
}

/// Metadata-only view of a [`Document`] for the list endpoint — omits the
/// denormalized full `content` (and raw `metadata`), which can be megabytes
/// per document. Fetch full content via `GET /api/v1/kb/documents/{id}`.
#[derive(Debug, Serialize)]
struct DocumentSummary {
    id: DocumentId,
    title: String,
    categories: Vec<String>,
    subcategory: Option<String>,
    file_type: Option<String>,
    mime_type: Option<String>,
    file_size: Option<u64>,
    session_id: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    retrieval_count: i64,
    last_retrieved_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<Document> for DocumentSummary {
    fn from(d: Document) -> Self {
        Self {
            id: d.id,
            title: d.title,
            categories: d.categories,
            subcategory: d.subcategory,
            file_type: d.file_type,
            mime_type: d.mime_type,
            file_size: d.file_size,
            session_id: d.session_id,
            created_at: d.created_at,
            updated_at: d.updated_at,
            retrieval_count: d.retrieval_count,
            last_retrieved_at: d.last_retrieved_at,
        }
    }
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<DocumentSummary>>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let docs = ctx
        .store
        .list_documents(q.organization.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(docs.into_iter().map(DocumentSummary::from).collect()))
}

async fn get_doc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Document>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let doc = ctx
        .store
        .get_document(&DocumentId(id.clone()))
        .await
        .map_err(ApiError::from)?;
    match doc {
        Some(d) => Ok(Json(d)),
        None => Err(ApiError::not_found(format!("document {id} not found")).into()),
    }
}

async fn delete_doc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<DeleteResponse>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let hard = q.hard.unwrap_or(false);
    let soft = !hard;
    ctx.store
        .delete_document(&DocumentId(id.clone()), soft)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(DeleteResponse {
        id,
        mode: if hard { "hard" } else { "soft" },
    }))
}

async fn drift(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DriftResponse>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let report = check_drift(&ctx.cfg, &ctx.store)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(DriftResponse::from(&report)))
}

async fn re_embed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ReEmbedRequest>,
) -> Result<Json<ReEmbedResponse>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let batch_size = body.batch_size.unwrap_or(100).max(1);
    let report = run_bulk_re_embed(
        &ctx.cfg,
        &ctx.store,
        &ctx.embedder,
        BulkReEmbedOptions {
            batch_size,
            include_already_current: body.include_current,
            dry_run: body.dry_run,
        },
    )
    .await
    .map_err(ApiError::from)?;
    Ok(Json(ReEmbedResponse::from(&report)))
}

// ────────────────────────────────────────────────────────────────────────────
// Document intelligence + cross-document graph.
// ────────────────────────────────────────────────────────────────────────────

async fn get_intelligence(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<IntelligenceResponse>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let (entities, relations) = ctx
        .intel
        .intelligence_for_document(&id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(IntelligenceResponse::build(&entities, &relations)))
}

async fn get_graph(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<GraphQuery>,
) -> Result<Json<GraphResponse>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let limit = effective_graph_limit(q.limit, ctx.cfg.graph_max_nodes);
    let graph = ctx
        .intel
        .graph(q.group.as_deref(), limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(GraphResponse::from(graph)))
}

async fn re_extract_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<ReExtractResponse>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;

    // Load the stored document content and re-chunk it so extraction sees the
    // same chunk boundaries as ingest.
    let doc = ctx
        .store
        .get_document(&DocumentId(id.clone()))
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found(format!("document {id} not found")))?;

    let primary_category = doc
        .categories
        .first()
        .cloned()
        .unwrap_or_else(|| "RANTAICLAW".to_string());
    let chunks = smart_chunk_document(
        &doc.content,
        &doc.title,
        &primary_category,
        doc.subcategory.as_deref(),
        SmartChunkOptions::default(),
    );
    let chunk_texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let chunk_refs: Vec<&str> = chunk_texts.iter().map(String::as_str).collect();

    let extractor = build_intelligence_extractor(&ctx.cfg);
    let summary = extract_document_intelligence(
        &*ctx.intel,
        &extractor,
        &id,
        &chunk_refs,
        &ctx.cfg.intelligence_resolution,
    )
    .await
    .map_err(ApiError::from)?;

    Ok(Json(ReExtractResponse::new(id, summary)))
}

// ────────────────────────────────────────────────────────────────────────────
// KB groups — CRUD + document membership.
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CreateGroupRequest {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateGroupRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeleteGroupResponse {
    id: String,
    deleted: bool,
}

#[derive(Debug, Deserialize)]
struct AddGroupDocumentRequest {
    document_id: String,
}

#[derive(Debug, Serialize)]
struct OkResponse {
    ok: bool,
}

async fn list_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<KbGroupSummary>>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let groups = ctx.store.list_groups().await.map_err(ApiError::from)?;
    Ok(Json(groups))
}

async fn create_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateGroupRequest>,
) -> Result<Json<KbGroup>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty".into()).into());
    }
    let ctx = ensure_kb_ctx(&state).await?;
    let group = ctx
        .store
        .create_group(
            body.name.trim(),
            body.description.as_deref(),
            body.color.as_deref(),
        )
        .await
        .map_err(ApiError::from)?;
    Ok(Json(group))
}

async fn get_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<KbGroup>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    match ctx.store.get_group(&id).await.map_err(ApiError::from)? {
        Some(g) => Ok(Json(g)),
        None => Err(ApiError::not_found(format!("group {id} not found")).into()),
    }
}

async fn update_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UpdateGroupRequest>,
) -> Result<Json<KbGroup>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    if let Some(n) = body.name.as_deref() {
        if n.trim().is_empty() {
            return Err(ApiError::bad_request("name must not be empty".into()).into());
        }
    }
    let ctx = ensure_kb_ctx(&state).await?;
    ctx.store
        .update_group(
            &id,
            body.name.as_deref().map(str::trim),
            body.description.as_deref(),
            body.color.as_deref(),
        )
        .await
        .map_err(ApiError::from)?;
    // Return the freshly-updated record so callers see canonical state.
    match ctx.store.get_group(&id).await.map_err(ApiError::from)? {
        Some(g) => Ok(Json(g)),
        None => Err(ApiError::not_found(format!("group {id} not found")).into()),
    }
}

async fn delete_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<DeleteGroupResponse>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let deleted = ctx.store.delete_group(&id).await.map_err(ApiError::from)?;
    Ok(Json(DeleteGroupResponse { id, deleted }))
}

async fn list_group_documents(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Vec<DocumentSummary>>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let docs = ctx
        .store
        .list_group_documents(&id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(docs.into_iter().map(DocumentSummary::from).collect()))
}

async fn add_group_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<AddGroupDocumentRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    if body.document_id.trim().is_empty() {
        return Err(ApiError::bad_request("document_id must not be empty".into()).into());
    }
    let ctx = ensure_kb_ctx(&state).await?;
    ctx.store
        .add_document_to_group(body.document_id.trim(), &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(OkResponse { ok: true }))
}

async fn remove_group_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, doc_id)): Path<(String, String)>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let ok = ctx
        .store
        .remove_document_from_group(&doc_id, &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(OkResponse { ok }))
}

// ────────────────────────────────────────────────────────────────────────────
// Ingest — multipart upload.
//
// Form fields (all optional except `file`):
// - `file`     — file bytes; filename is used as the default title and to
//                derive the file_type extension hint.
// - `title`    — overrides the file-stem-derived title.
// - `categories` — comma-separated list, e.g. "FAQ,product".
// - `groups`    — comma-separated list of KB group IDs.
//
// Implementation strategy: stream the uploaded bytes into a tempfile on disk
// so we can reuse the existing `process_file` pipeline (which dispatches by
// extension and may exec subprocesses for PDF/image). The tempfile is
// dropped after ingestion. KISS over building a parallel byte-based pipeline.
// ────────────────────────────────────────────────────────────────────────────

async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<IngestResponse>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx(&state).await?;
    let started = std::time::Instant::now();

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    let mut title_override: Option<String> = None;
    let mut categories: Vec<String> = Vec::new();
    let mut groups: Vec<String> = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(format!("multipart parse: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                file_name = field.file_name().map(str::to_string);
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::bad_request(format!("file bytes: {e}")))?;
                file_bytes = Some(data.to_vec());
            }
            "title" => {
                let v = field
                    .text()
                    .await
                    .map_err(|e| ApiError::bad_request(format!("title field: {e}")))?;
                if !v.trim().is_empty() {
                    title_override = Some(v.trim().to_string());
                }
            }
            "categories" => {
                let v = field
                    .text()
                    .await
                    .map_err(|e| ApiError::bad_request(format!("categories field: {e}")))?;
                categories.extend(split_csv(&v));
            }
            "groups" => {
                let v = field
                    .text()
                    .await
                    .map_err(|e| ApiError::bad_request(format!("groups field: {e}")))?;
                groups.extend(split_csv(&v));
            }
            _ => {
                // Drain unknown fields so the multipart parser advances cleanly.
                let _ = field.bytes().await;
            }
        }
    }

    let bytes = file_bytes
        .ok_or_else(|| ApiError::bad_request("multipart 'file' field is required".into()))?;
    let original_name = file_name.unwrap_or_else(|| "upload.md".to_string());

    // Stage the upload to a per-request tempdir so `process_file` can
    // dispatch by extension. We build the dir under `std::env::temp_dir()`
    // with a UUID suffix so concurrent requests don't collide, and clean
    // it up via [`StagedDirGuard`] regardless of how the handler exits.
    let staged_dir = std::env::temp_dir().join(format!("rantaiclaw-kb-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&staged_dir)
        .await
        .map_err(|e| ApiError::bad_request(format!("stage dir: {e}")))?;
    let _guard = StagedDirGuard(staged_dir.clone());
    let staged = staged_dir.join(sanitize_filename(&original_name));
    tokio::fs::write(&staged, &bytes)
        .await
        .map_err(|e| ApiError::bad_request(format!("write staged: {e}")))?;

    // Up-front fail-fast: the file_type detector rejects unknown extensions
    // anyway, but emitting the error before launching the extractor keeps
    // the error message attached to the uploaded filename.
    if detect_file_type(&staged).is_none() {
        return Err(ApiError::from(KbError::UnsupportedFileType(original_name)).into());
    }

    let processed = process_file(&ctx.cfg, &staged, ProcessingOptions::default())
        .await
        .map_err(ApiError::from)?;

    let title = title_override.unwrap_or_else(|| {
        PathBuf::from(&original_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .unwrap_or(original_name.clone())
    });

    let primary_category = categories
        .first()
        .cloned()
        .unwrap_or_else(|| "RANTAICLAW".to_string());

    // Extraction-quality signal for observability + the ingest response.
    let chars_extracted = processed.content.chars().count();
    let pages = crate::kb::extract::pdf_splitter::get_page_count(&bytes)
        .await
        .ok();
    let low_density = low_text_density(chars_extracted, pages);

    let chunks = smart_chunk_document(
        &processed.content,
        &title,
        &primary_category,
        None,
        SmartChunkOptions::default(),
    );
    if chunks.is_empty() {
        tracing::warn!(
            target: "kb::ingest",
            filename = %original_name,
            chars = chars_extracted,
            pages = ?pages,
            "ingest produced no chunks"
        );
        return Err(ApiError::bad_request(format!(
            "no chunks produced from upload {original_name}"
        ))
        .into());
    }

    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let embeddings = ctx.embedder.embed_many(&texts).await.map_err(|e| {
        tracing::warn!(
            target: "kb::ingest",
            filename = %original_name,
            error = %e,
            "embedding failed during ingest"
        );
        ApiError::from(e)
    })?;

    let doc_id = DocumentId(uuid::Uuid::new_v4().to_string());
    let now = chrono::Utc::now();
    let metadata = serde_json::json!({
        "source": "http_upload",
        "filename": original_name,
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
        // u64 cast: file_size column is u64; bytes.len() is usize, narrows
        // safely on 64-bit and is bounded by axum's body limit on 32-bit.
        file_size: Some(bytes.len() as u64),
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
    if let Err(e) = crate::kb::store::store_document_with_chunks(
        ctx.store.as_ref(),
        &document,
        &chunks,
        &embeddings,
        ctx.embedder.model(),
    )
    .await
    {
        tracing::error!(
            target: "kb::ingest",
            filename = %original_name,
            error = %e,
            "persist failed; document rolled back if it was created"
        );
        return Err(ApiError::from(e).into());
    }

    // Attach the document to any groups named in the `groups` form field
    // (single id or comma-separated, already split into `groups`). Idempotent
    // per-group via INSERT OR IGNORE in the store.
    for group_id in &groups {
        ctx.store
            .add_document_to_group(&doc_id.0, group_id)
            .await
            .map_err(ApiError::from)?;
    }

    // Document intelligence: fire-and-forget. tokio::spawn detaches the
    // (LLM-bound, slow) extraction so the ingest response returns immediately
    // and an extraction failure never affects the upload result. Mirrors the
    // `record_retrieval_hits` detach pattern in `kb::retrieve`. Gated on the
    // `intelligence_enabled` flag (off by default). The API key is never
    // logged — only the doc id / count surface on warn.
    if ctx.cfg.intelligence_enabled {
        let cfg = ctx.cfg.clone();
        let intel = Arc::clone(&ctx.intel);
        let extract_doc_id = doc_id.0.clone();
        let chunk_texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
        tokio::spawn(async move {
            let extractor = build_intelligence_extractor(&cfg);
            let chunk_refs: Vec<&str> = chunk_texts.iter().map(String::as_str).collect();
            if let Err(e) = extract_document_intelligence(
                &*intel,
                &extractor,
                &extract_doc_id,
                &chunk_refs,
                &cfg.intelligence_resolution,
            )
            .await
            {
                tracing::warn!(
                    target: "kb::ingest",
                    document_id = %extract_doc_id,
                    error = %e,
                    "document intelligence extraction failed (fire-and-forget)"
                );
            }
        });
    }

    // u128 → u64 ms: see comments on the CLI side; the cast is safe in any
    // realistic ingest duration.
    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = started.elapsed().as_millis() as u64;

    if low_density {
        tracing::warn!(
            target: "kb::ingest",
            filename = %original_name,
            chars = chars_extracted,
            pages = ?pages,
            chunks = chunks.len(),
            "ingest ok but low text density — document may retrieve poorly; consider OCR"
        );
    } else {
        tracing::info!(
            target: "kb::ingest",
            filename = %original_name,
            chars = chars_extracted,
            pages = ?pages,
            chunks = chunks.len(),
            "ingest ok"
        );
    }

    Ok(Json(IngestResponse {
        document_id: doc_id.0,
        chunks_stored: chunks.len(),
        elapsed_ms,
        chars_extracted,
        pages,
        low_text_density: low_density,
    }))
}

/// True when extracted text is suspiciously thin for the page count — a
/// near-empty extraction the operator (and UI) should be warned about.
/// Conservative (~100 chars/page) so legitimately sparse design documents
/// aren't flagged.
fn low_text_density(chars: usize, pages: Option<u32>) -> bool {
    let pages = pages.unwrap_or(1).max(1) as usize;
    chars < 100usize.saturating_mul(pages)
}

/// Split a CSV cell into trimmed, non-empty entries. Used for `categories`
/// and `groups` multipart fields so callers can submit either repeated
/// fields (handled by extending) or a single comma-separated value.
fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// RAII guard that recursively removes a staged-upload directory when the
/// handler returns (success or error). Failures during cleanup are logged
/// at `warn` because the tempdir naming (UUID suffix) means a leak can only
/// accumulate one directory per failed cleanup — observable but not fatal.
struct StagedDirGuard(PathBuf);

impl Drop for StagedDirGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            // Don't surface filename — could carry user data.
            tracing::warn!(error = %e, "staged upload dir cleanup failed");
        }
    }
}

/// Defensive filename sanitizer for the staged-tempfile path. Rejects any
/// path separator or `..` segment so a hostile upload can't escape the
/// per-request tempdir. Fallback when sanitization strips everything:
/// `"upload"`.
fn sanitize_filename(raw: &str) -> String {
    let basename = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim_matches('.');
    if basename.is_empty() || basename == ".." {
        "upload".to_string()
    } else {
        basename.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clear_kb_ctx_resets_cache() {
        clear_kb_ctx().await;
        let guard = KB_CTX.lock().await;
        assert!(guard.is_none());
    }

    #[test]
    fn resolve_key_empty_when_no_key_and_no_openrouter_env() {
        std::env::remove_var("OPENROUTER_API_KEY");
        assert!(crate::kb::config::KbConfig::resolve_key("").is_empty());
    }

    #[test]
    fn split_csv_drops_empty_and_trims() {
        assert_eq!(
            split_csv("a, b ,,c"),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        );
        assert!(split_csv("").is_empty());
        assert!(split_csv("   ").is_empty());
    }

    #[test]
    fn effective_graph_limit_clamps_to_hard_cap() {
        // A caller-supplied limit above the hard cap is clamped.
        assert_eq!(effective_graph_limit(Some(100_000), 200), GRAPH_HARD_CAP);
        // No request falls back to the configured default.
        assert_eq!(effective_graph_limit(None, 200), 200);
        // A modest request passes through unchanged.
        assert_eq!(effective_graph_limit(Some(50), 200), 50);
    }

    #[test]
    fn low_text_density_flags_near_empty_extraction() {
        // ~100 chars/page floor: near-empty extraction is flagged.
        assert!(low_text_density(40, Some(1))); // 40 < 100
        assert!(low_text_density(200, Some(8))); // 200 < 800
                                                 // Legitimately sparse design docs (a few hundred chars/page) are not.
        assert!(!low_text_density(9000, Some(30))); // 9000 >= 3000
        assert!(!low_text_density(500, None)); // None -> 1 page; 500 >= 100
    }

    #[test]
    fn sanitize_filename_rejects_traversal() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\windows\\boot.ini"), "boot.ini");
        assert_eq!(sanitize_filename("normal.md"), "normal.md");
        assert_eq!(sanitize_filename(".."), "upload");
        assert_eq!(sanitize_filename(""), "upload");
    }

    #[test]
    fn drift_response_maps_report() {
        let report = DriftReport {
            current_model: "qwen/qwen3-embedding-8b".into(),
            by_model: vec![
                (Some("qwen/qwen3-embedding-8b".into()), 10),
                (Some("old/model".into()), 3),
                (None, 1),
            ],
            stale_chunk_count: 4,
            in_sync: false,
        };
        let resp = DriftResponse::from(&report);
        assert_eq!(resp.total_chunks, 14);
        assert_eq!(resp.stale_chunks, 4);
        assert!(!resp.in_sync);
        assert_eq!(resp.by_model.len(), 3);
    }

    #[test]
    fn kb_error_to_api_error_status_mapping() {
        let cases = [
            (
                KbError::NotFound("x".into()),
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                KbError::Config("x".into()),
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (
                KbError::UnsupportedFileType("x".into()),
                StatusCode::BAD_REQUEST,
                "unsupported_file_type",
            ),
            (
                KbError::EmbeddingApi {
                    status: 503,
                    body: "x".into(),
                },
                StatusCode::BAD_GATEWAY,
                "embedding_upstream",
            ),
            (
                KbError::ChatApi {
                    status: 500,
                    body: "x".into(),
                },
                StatusCode::BAD_GATEWAY,
                "chat_upstream",
            ),
            (
                KbError::Other("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
            ),
        ];
        for (err, want_status, want_code) in cases {
            let api: ApiError = err.into();
            assert_eq!(api.status, want_status, "status mismatch for {want_code}");
            assert_eq!(api.body.error, want_code);
        }
    }
}
