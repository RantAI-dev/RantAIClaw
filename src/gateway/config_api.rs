//! Live config API (`/api/v1/config*`).
//!
//! Lets the web console read the running configuration and mutate the model,
//! autonomy policy, and MCP-server settings. Every mutation is persisted to
//! `config.toml` via [`Config::save`] (which encrypts secrets), so changes
//! survive — and MCP servers connect on — the next daemon restart.
//!
//! Auth mirrors the rest of `/api/v1`: when the gateway requires pairing,
//! every endpoint needs `Authorization: Bearer <token>`; otherwise (local dev
//! default) requests are accepted.

use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post, put},
    Router,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::config::schema::McpServerConfig;
use crate::security::AutonomyLevel;

/// Build the `/api/v1/config*` router. Merged alongside `api_v1::router()` so
/// it shares the small-body limit + timeout middleware.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/config", get(get_config))
        .route("/api/v1/config/model", put(set_model))
        .route("/api/v1/config/autonomy", put(set_autonomy))
        .route(
            "/api/v1/config/mcp_servers/{name}",
            post(add_mcp_server).delete(remove_mcp_server),
        )
}

type ApiError = (StatusCode, Json<serde_json::Value>);

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

fn err_400(msg: impl Into<String>) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "bad_request", "detail": msg.into() })),
    )
}

// ── GET /config ──────────────────────────────────────────────────────────────

/// Returns the running config as JSON, with provider/API secrets redacted —
/// the console only needs non-secret fields (model, autonomy, MCP servers).
async fn get_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let mut cfg = state.config.lock().clone();
    // Redact exactly the fields Config::save() treats as secrets — never expose
    // raw provider keys over the API. This is a response-only copy; the
    // in-memory + on-disk config keep their real values.
    cfg.api_key = None;
    cfg.composio.api_key = None;
    cfg.browser.computer_use.api_key = None;
    cfg.web_search.brave_api_key = None;
    cfg.storage.provider.config.db_url = None;
    for agent in cfg.agents.values_mut() {
        agent.api_key = None;
    }
    let val = serde_json::to_value(&cfg).map_err(err_500)?;
    Ok(Json(val))
}

// ── PUT /config/model ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ModelBody {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    temperature: Option<f64>,
}

/// Persist a snapshot of the mutated config, then swap it into the running
/// state. Cloning out from under the lock keeps the (sync) mutex un-held across
/// the async `save()`.
async fn persist_and_swap(state: &AppState, cfg: crate::config::Config) -> Result<(), ApiError> {
    cfg.save().await.map_err(err_500)?;
    *state.config.lock() = cfg;
    Ok(())
}

async fn set_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ModelBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let mut cfg = state.config.lock().clone();
    if let Some(p) = body.provider {
        cfg.default_provider = if p.trim().is_empty() {
            None
        } else {
            Some(p.trim().to_string())
        };
    }
    if let Some(m) = body.model {
        cfg.default_model = if m.trim().is_empty() {
            None
        } else {
            Some(m.trim().to_string())
        };
    }
    if let Some(t) = body.temperature {
        cfg.default_temperature = t;
    }
    let resp = json!({
        "default_provider": cfg.default_provider,
        "default_model": cfg.default_model,
        "default_temperature": cfg.default_temperature,
    });
    persist_and_swap(&state, cfg).await?;
    Ok(Json(resp))
}

// ── PUT /config/autonomy ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AutonomyBody {
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    auto_approve: Option<Vec<String>>,
    #[serde(default)]
    always_ask: Option<Vec<String>>,
    #[serde(default)]
    allowed_commands: Option<Vec<String>>,
    #[serde(default)]
    forbidden_paths: Option<Vec<String>>,
    #[serde(default)]
    max_actions_per_hour: Option<u32>,
    #[serde(default)]
    max_cost_per_day_cents: Option<u32>,
    #[serde(default)]
    workspace_only: Option<bool>,
    #[serde(default)]
    block_high_risk_commands: Option<bool>,
    #[serde(default)]
    require_approval_for_medium_risk: Option<bool>,
}

/// Accept both `read_only` (UI spelling) and `readonly` (enum serde spelling).
fn parse_level(s: &str) -> Option<AutonomyLevel> {
    match s.trim().to_lowercase().replace('_', "").as_str() {
        "readonly" => Some(AutonomyLevel::ReadOnly),
        "supervised" => Some(AutonomyLevel::Supervised),
        "full" => Some(AutonomyLevel::Full),
        _ => None,
    }
}

async fn set_autonomy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AutonomyBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let mut cfg = state.config.lock().clone();
    if let Some(l) = body.level {
        cfg.autonomy.level =
            parse_level(&l).ok_or_else(|| err_400(format!("invalid autonomy level: {l}")))?;
    }
    if let Some(v) = body.auto_approve {
        cfg.autonomy.auto_approve = v;
    }
    if let Some(v) = body.always_ask {
        cfg.autonomy.always_ask = v;
    }
    if let Some(v) = body.allowed_commands {
        cfg.autonomy.allowed_commands = v;
    }
    if let Some(v) = body.forbidden_paths {
        cfg.autonomy.forbidden_paths = v;
    }
    if let Some(v) = body.max_actions_per_hour {
        cfg.autonomy.max_actions_per_hour = v;
    }
    if let Some(v) = body.max_cost_per_day_cents {
        cfg.autonomy.max_cost_per_day_cents = v;
    }
    if let Some(v) = body.workspace_only {
        cfg.autonomy.workspace_only = v;
    }
    if let Some(v) = body.block_high_risk_commands {
        cfg.autonomy.block_high_risk_commands = v;
    }
    if let Some(v) = body.require_approval_for_medium_risk {
        cfg.autonomy.require_approval_for_medium_risk = v;
    }
    let resp = serde_json::to_value(&cfg.autonomy).map_err(err_500)?;
    persist_and_swap(&state, cfg).await?;
    Ok(Json(resp))
}

// ── POST/DELETE /config/mcp_servers/{name} ───────────────────────────────────

#[derive(Deserialize)]
struct McpServerBody {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

async fn add_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<McpServerBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(err_400("server name must not be empty"));
    }
    if body.command.trim().is_empty() {
        return Err(err_400("command must not be empty"));
    }
    let mut cfg = state.config.lock().clone();
    cfg.mcp_servers.insert(
        name.clone(),
        McpServerConfig {
            command: body.command.trim().to_string(),
            args: body.args,
            env: body.env,
        },
    );
    let count = cfg.mcp_servers.len();
    persist_and_swap(&state, cfg).await?;
    Ok(Json(json!({ "name": name, "added": true, "count": count })))
}

async fn remove_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let mut cfg = state.config.lock().clone();
    let removed = cfg.mcp_servers.remove(&name).is_some();
    let count = cfg.mcp_servers.len();
    persist_and_swap(&state, cfg).await?;
    Ok(Json(
        json!({ "name": name, "removed": removed, "count": count }),
    ))
}
