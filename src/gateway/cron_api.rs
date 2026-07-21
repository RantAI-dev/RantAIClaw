//! Cron/schedule API (`/api/v1/cron*`) — lets the web console list, create,
//! edit, delete, force-run, and inspect the run history of scheduled jobs.
//!
//! Auth mirrors the rest of `/api/v1`: when the gateway requires pairing, every
//! endpoint needs `Authorization: Bearer <token>`.
//!
//! Cron jobs live in the per-profile sqlite store (`workspace_dir/cron/jobs.db`),
//! NOT in `config.toml`, so these handlers do not touch the config write lock.
//! They clone the running `Config` (for `workspace_dir` + `autonomy`) and call
//! the synchronous `crate::cron` store functions inside `spawn_blocking`
//! (rusqlite is blocking).

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post, put},
    Router,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::cron::{self, CronJobPatch, DeliveryConfig, JobType, Schedule, SessionTarget};
use crate::security::SecurityPolicy;

/// Build the `/api/v1/cron*` router. Merged alongside `api_v1::router()` so it
/// shares the small-body limit + timeout middleware (and, at the call site in
/// `mod.rs`, the same `api_rate_limit` layer as `config_api`).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/cron", get(list_cron).post(create_cron))
        .route("/api/v1/cron/{id}", put(update_cron).delete(delete_cron))
        .route("/api/v1/cron/{id}/run", post(run_cron))
        .route("/api/v1/cron/{id}/runs", get(list_cron_runs))
}

type ApiError = (StatusCode, Json<serde_json::Value>);

// NOTE: `check_auth`/`err_*` duplicate `api_v1.rs` + `config_api.rs`. This is the
// established per-module pattern; the third copy now justifies a shared helper —
// a low-risk follow-up (extract `pub(super) fn check_auth` in `mod.rs`), left out
// here to keep this high-risk gateway change surgical.
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
        })
        .unwrap_or("");
    if state.pairing.is_authenticated(token) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "unauthorized",
                "detail": "Pair via POST /pair, then send `Authorization: Bearer <token>`."
            })),
        ))
    }
}

fn err_500(msg: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal_error", "detail": msg.to_string() })),
    )
}

fn err_400(msg: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "bad_request", "detail": msg.to_string() })),
    )
}

fn err_404(msg: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not_found", "detail": msg.to_string() })),
    )
}

/// Map a `crate::cron` store error to 404 when it's a missing-job error, else 400.
/// (`get_job`/`update_job`/`remove_job` return `... not found` on a bad id.)
fn map_store_error(e: anyhow::Error) -> ApiError {
    let s = e.to_string();
    if s.contains("not found") {
        err_404(s)
    } else {
        err_400(s)
    }
}

/// Clone the running config for store/scheduler calls (workspace_dir + autonomy).
fn cfg_snapshot(state: &AppState) -> crate::config::Config {
    state.config.lock().clone()
}

// ── GET /cron ────────────────────────────────────────────────────────────────
async fn list_cron(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = cfg_snapshot(&state);
    let jobs = tokio::task::spawn_blocking(move || cron::list_jobs(&cfg))
        .await
        .map_err(err_500)?
        .map_err(err_500)?;
    let count = jobs.len();
    Ok(Json(json!({ "jobs": jobs, "count": count })))
}

// ── GET /cron/{id}/runs ──────────────────────────────────────────────────────
#[derive(Deserialize)]
struct RunsQuery {
    #[serde(default = "default_runs_limit")]
    limit: usize,
}
fn default_runs_limit() -> usize {
    50
}

async fn list_cron_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<RunsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = cfg_snapshot(&state);
    let runs = tokio::task::spawn_blocking(move || cron::list_runs(&cfg, &id, q.limit))
        .await
        .map_err(err_500)?
        .map_err(err_500)?;
    let count = runs.len();
    Ok(Json(json!({ "runs": runs, "count": count })))
}

// ── POST /cron ───────────────────────────────────────────────────────────────
#[derive(Deserialize)]
struct CreateCronBody {
    schedule: Schedule,
    #[serde(default)]
    job_type: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    session_target: Option<String>,
    #[serde(default)]
    delivery: Option<DeliveryConfig>,
    #[serde(default)]
    delete_after_run: Option<bool>,
}

/// Resolve the job kind: an explicit `job_type` wins; otherwise infer from which
/// of `prompt` (agent) / `command` (shell) is provided.
fn resolve_job_kind(body: &CreateCronBody) -> Result<JobType, ApiError> {
    if let Some(jt) = body.job_type.as_deref() {
        return JobType::try_from(jt).map_err(err_400);
    }
    let has_prompt = body
        .prompt
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty());
    let has_command = body
        .command
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty());
    match (has_prompt, has_command) {
        (true, false) => Ok(JobType::Agent),
        (false, true) => Ok(JobType::Shell),
        (true, true) => Err(err_400(
            "provide either 'prompt' (agent) or 'command' (shell), not both",
        )),
        (false, false) => Err(err_400(
            "provide 'prompt' (agent job) or 'command' (shell job)",
        )),
    }
}

async fn create_cron(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateCronBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let kind = resolve_job_kind(&body)?;
    let cfg = cfg_snapshot(&state);

    let job = match kind {
        JobType::Agent => {
            let prompt = body
                .prompt
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| err_400("agent job requires a non-empty 'prompt'"))?;
            let target = body
                .session_target
                .as_deref()
                .map(SessionTarget::parse)
                .unwrap_or_default();
            let delete_after = body
                .delete_after_run
                .unwrap_or(matches!(body.schedule, Schedule::At { .. }));
            let (name, model, delivery, schedule) = (
                body.name.clone(),
                body.model.clone(),
                body.delivery.clone(),
                body.schedule.clone(),
            );
            tokio::task::spawn_blocking(move || {
                cron::add_agent_job(
                    &cfg,
                    name,
                    schedule,
                    &prompt,
                    target,
                    model,
                    delivery,
                    delete_after,
                )
            })
            .await
            .map_err(err_500)?
            .map_err(err_400)? // validate_schedule failures → 400
        }
        JobType::Shell => {
            let command = body
                .command
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| err_400("shell job requires a non-empty 'command'"))?;
            // Exposure hardening: a shell job created over HTTP will later run as
            // `sh -lc` on the host. Security-check the command up-front (stricter
            // than CLI `cron add`, matching the HTTP surface's blast radius).
            let security = SecurityPolicy::from_config(&cfg.autonomy, &cfg.workspace_dir);
            if !security.is_command_allowed(&command) {
                return Err(err_400(format!(
                    "command blocked by security policy: {command}"
                )));
            }
            let (name, schedule) = (body.name.clone(), body.schedule.clone());
            tokio::task::spawn_blocking(move || cron::add_shell_job(&cfg, name, schedule, &command))
                .await
                .map_err(err_500)?
                .map_err(err_400)?
        }
    };
    Ok(Json(serde_json::to_value(job).map_err(err_500)?))
}

// ── PUT /cron/{id} ───────────────────────────────────────────────────────────
#[derive(Deserialize)]
struct UpdateCronBody {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    schedule: Option<Schedule>,
    #[serde(default)]
    session_target: Option<String>,
    #[serde(default)]
    delivery: Option<DeliveryConfig>,
    #[serde(default)]
    delete_after_run: Option<bool>,
}

async fn update_cron(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UpdateCronBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = cfg_snapshot(&state);
    // Security-check a newly supplied shell command before persisting.
    if let Some(cmd) = body.command.as_deref().filter(|s| !s.trim().is_empty()) {
        let security = SecurityPolicy::from_config(&cfg.autonomy, &cfg.workspace_dir);
        if !security.is_command_allowed(cmd) {
            return Err(err_400(format!(
                "command blocked by security policy: {cmd}"
            )));
        }
    }
    let patch = CronJobPatch {
        schedule: body.schedule,
        command: body.command,
        prompt: body.prompt,
        name: body.name,
        enabled: body.enabled,
        delivery: body.delivery,
        model: body.model,
        session_target: body.session_target.as_deref().map(SessionTarget::parse),
        delete_after_run: body.delete_after_run,
    };
    let updated = tokio::task::spawn_blocking(move || cron::update_job(&cfg, &id, patch))
        .await
        .map_err(err_500)?
        .map_err(map_store_error)?; // not-found → 404, validate → 400
    Ok(Json(serde_json::to_value(updated).map_err(err_500)?))
}

// ── DELETE /cron/{id} ────────────────────────────────────────────────────────
async fn delete_cron(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = cfg_snapshot(&state);
    let id_for_store = id.clone();
    let result = tokio::task::spawn_blocking(move || cron::remove_job(&cfg, &id_for_store))
        .await
        .map_err(err_500)?;
    match result {
        Ok(()) => Ok(Json(json!({ "id": id, "deleted": true }))),
        Err(e) if e.to_string().contains("not found") => Err(err_404(e.to_string())),
        Err(e) => Err(err_500(e)),
    }
}

// ── POST /cron/{id}/run ──────────────────────────────────────────────────────
#[derive(Deserialize)]
struct RunQuery {
    #[serde(default)]
    approved: bool,
}

async fn run_cron(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<RunQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = cfg_snapshot(&state);
    let cfg_for_get = cfg.clone();
    let id_for_get = id.clone();
    let job = tokio::task::spawn_blocking(move || cron::get_job(&cfg_for_get, &id_for_get))
        .await
        .map_err(err_500)?
        .map_err(map_store_error)?;

    // Security/approval gate — mirror the `cron_run` tool.
    let security = SecurityPolicy::from_config(&cfg.autonomy, &cfg.workspace_dir);
    if !security.can_act() {
        return Err(err_400(
            "security policy: read-only mode, cannot run a cron job",
        ));
    }
    if matches!(job.job_type, JobType::Shell) {
        if let Err(reason) = security.validate_command_execution(&job.command, q.approved) {
            return Err(err_400(reason));
        }
    }

    let (success, output) = cron::scheduler::run_job_manual(&cfg, &job).await;
    Ok(Json(
        json!({ "id": job.id, "success": success, "output": output }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(prompt: Option<&str>, command: Option<&str>, job_type: Option<&str>) -> CreateCronBody {
        CreateCronBody {
            schedule: Schedule::Every { every_ms: 60_000 },
            job_type: job_type.map(Into::into),
            prompt: prompt.map(Into::into),
            command: command.map(Into::into),
            name: None,
            model: None,
            session_target: None,
            delivery: None,
            delete_after_run: None,
        }
    }

    #[test]
    fn resolve_job_kind_infers_from_fields() {
        assert_eq!(
            resolve_job_kind(&body(Some("hi"), None, None)).unwrap(),
            JobType::Agent
        );
        assert_eq!(
            resolve_job_kind(&body(None, Some("echo hi"), None)).unwrap(),
            JobType::Shell
        );
    }

    #[test]
    fn resolve_job_kind_rejects_both_and_neither() {
        assert!(resolve_job_kind(&body(Some("hi"), Some("echo"), None)).is_err());
        assert!(resolve_job_kind(&body(None, None, None)).is_err());
    }

    #[test]
    fn resolve_job_kind_honors_explicit_job_type() {
        assert_eq!(
            resolve_job_kind(&body(None, Some("x"), Some("agent"))).unwrap(),
            JobType::Agent
        );
        assert!(resolve_job_kind(&body(None, None, Some("nonsense"))).is_err());
    }
}
