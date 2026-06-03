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

use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use tower_http::limit::RequestBodyLimitLayer;

use crate::gateway::AppState;
use crate::kb::chunk::{smart_chunk_document, SmartChunkOptions};
use crate::kb::embed::{self, EmbeddingProvider};
use crate::kb::file::{detect_file_type, process_file, ProcessingOptions};
use crate::kb::maintenance::{
    check_drift, run_bulk_re_embed, BulkReEmbedOptions, BulkReEmbedReport, DriftReport,
};
use crate::kb::rerank::{self, Reranker};
use crate::kb::retrieve::{RetrieveOptions, Retriever, SourceRef};
use crate::kb::store::sqlite::SqliteStore;
use crate::kb::store::KbStore;
use crate::kb::{Document, DocumentId, KbConfig, KbError};

/// Upload size cap for the KB ingest route. 32 MiB covers a typical
/// scientific PDF / large markdown bundle without giving an unauthenticated
/// caller free buffer pool space. Operators who need more can fork.
const KB_UPLOAD_MAX_BYTES: usize = 32 * 1024 * 1024;

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
        .route(
            "/api/v1/kb/documents/{id}",
            get(get_doc).delete(delete_doc),
        )
        .route("/api/v1/kb/drift", get(drift))
        .route("/api/v1/kb/re-embed", post(re_embed))
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(KB_UPLOAD_MAX_BYTES))
}

// ────────────────────────────────────────────────────────────────────────────
// KB context — lazy, process-wide, OnceCell-cached.
// ────────────────────────────────────────────────────────────────────────────

/// Bundled KB plumbing shared across handler invocations. Built once via
/// [`ensure_kb_ctx`]; never re-built within a process.
pub(crate) struct KbContext {
    pub cfg: KbConfig,
    pub store: Arc<dyn KbStore>,
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

async fn ensure_kb_ctx() -> Result<Arc<KbContext>, ApiError> {
    let path = super::cli::resolve_kb_db_path();
    let mut guard = KB_CTX.lock().await;
    if let Some(cached) = guard.as_ref() {
        if cached.path == path {
            return match &cached.ctx {
                Ok(ctx) => Ok(Arc::clone(ctx)),
                Err(msg) => Err(ApiError::service_unavailable(
                    "kb_unavailable",
                    msg.clone(),
                )),
            };
        }
    }
    // Rebuild. Failures cache as `Err` so we don't hammer the embed/auth
    // endpoint on every retry; operators fix the env and bounce the
    // gateway, which clears the static.
    let outcome = build_ctx(&path).await;
    let snapshot = outcome.clone();
    *guard = Some(CachedCtx {
        path,
        ctx: outcome,
    });
    match snapshot {
        Ok(ctx) => Ok(ctx),
        Err(msg) => Err(ApiError::service_unavailable("kb_unavailable", msg)),
    }
}

async fn build_ctx(path: &std::path::Path) -> Result<Arc<KbContext>, String> {
    let cfg = KbConfig::from_env().map_err(|e| format!("kb config: {e}"))?;
    let store = SqliteStore::open(path, cfg.embedding_dim)
        .await
        .map_err(|e| format!("sqlite open ({}): {e}", path.display()))?;
    let store: Arc<dyn KbStore> = Arc::new(store);
    let embedder = embed::make_provider(&cfg).map_err(|e| format!("embed provider: {e}"))?;
    let reranker = rerank::make_reranker(&cfg).map(Arc::from);
    Ok(Arc::new(KbContext {
        cfg,
        store,
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
    let ctx = ensure_kb_ctx().await?;

    let mut retriever = Retriever::new(
        ctx.cfg.clone(),
        Arc::clone(&ctx.store),
        Arc::clone(&ctx.embedder),
    );
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

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Document>>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx().await?;
    let docs = ctx
        .store
        .list_documents(q.organization.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(docs))
}

async fn get_doc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Document>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let ctx = ensure_kb_ctx().await?;
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
    let ctx = ensure_kb_ctx().await?;
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
    let ctx = ensure_kb_ctx().await?;
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
    let ctx = ensure_kb_ctx().await?;
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
    let ctx = ensure_kb_ctx().await?;
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
    let chunks = smart_chunk_document(
        &processed.content,
        &title,
        &primary_category,
        None,
        SmartChunkOptions::default(),
    );
    if chunks.is_empty() {
        return Err(ApiError::bad_request(format!(
            "no chunks produced from upload {original_name}"
        ))
        .into());
    }

    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let embeddings = ctx.embedder.embed_many(&texts).await.map_err(ApiError::from)?;

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
    ctx.store
        .create_document(&document)
        .await
        .map_err(ApiError::from)?;
    ctx.store
        .store_chunks(&doc_id, &chunks, &embeddings, ctx.embedder.model())
        .await
        .map_err(ApiError::from)?;

    // u128 → u64 ms: see comments on the CLI side; the cast is safe in any
    // realistic ingest duration.
    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = started.elapsed().as_millis() as u64;

    Ok(Json(IngestResponse {
        document_id: doc_id.0,
        chunks_stored: chunks.len(),
        elapsed_ms,
    }))
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
            (KbError::Config("x".into()), StatusCode::BAD_REQUEST, "bad_request"),
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
