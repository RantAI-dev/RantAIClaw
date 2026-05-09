//! Control-plane API (`/api/v1/*`) — HTTP equivalents for the CLI/TUI surfaces
//! that previously had no remote-driven access.
//!
//! Auth: bearer token verified against [`PairingGuard`]. When the gateway is
//! configured with `require_pairing = false` (default for local dev) all
//! requests are accepted; when `true`, every endpoint here requires
//! `Authorization: Bearer <token>` issued by `POST /pair`.
//!
//! Endpoints intentionally mirror the CLI subcommand layout so a curl-driven
//! test rig can exercise the same backend code paths the TUI hits via slash
//! commands.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::{get, post, put},
    Router,
};
use serde::{Deserialize, Serialize};

use super::AppState;

/// Build the `/api/v1/*` router. Mounted via `.merge()` from the main gateway
/// router so it shares state, body limit, and timeout middleware.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/version", get(version))
        .route("/api/v1/status", get(status))
        .route("/api/v1/doctor", get(doctor))
        .route("/api/v1/agent/chat", post(agent_chat))
        .route("/api/v1/sessions", get(sessions_list))
        .route("/api/v1/sessions/search", post(sessions_search))
        .route("/api/v1/sessions/{id}", get(sessions_get))
        .route("/api/v1/sessions/{id}/title", put(sessions_set_title))
        .route("/api/v1/insights", get(insights))
        .route("/api/v1/skills", get(skills_list))
        .route("/api/v1/skills/{name}", get(skills_show))
        .route("/api/v1/memory", get(memory_list))
        .route("/api/v1/memory/stats", get(memory_stats))
        .route(
            "/api/v1/personality",
            get(personality_get).put(personality_set),
        )
        .route("/api/v1/channels", get(channels_list))
        .route("/api/v1/providers", get(providers_list))
}

// ────────────────────────────────────────────────────────────────────────────
// Auth helper
// ────────────────────────────────────────────────────────────────────────────

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), (StatusCode, Json<ErrorBody>)> {
    if !state.pairing.require_pairing() {
        return Ok(());
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("bearer "))
        });
    match token {
        Some(t) if state.pairing.is_authenticated(t) => Ok(()),
        _ => Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody {
                error: "unauthorized".into(),
                detail: Some(
                    "Pair via POST /pair, then send `Authorization: Bearer <token>`.".into(),
                ),
            }),
        )),
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

fn err_500(e: anyhow::Error) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody {
            error: "internal_error".into(),
            detail: Some(format!("{e:#}")),
        }),
    )
}

fn err_404(msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: "not_found".into(),
            detail: Some(msg.into()),
        }),
    )
}

fn err_400(msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody {
            error: "bad_request".into(),
            detail: Some(msg.into()),
        }),
    )
}

// ────────────────────────────────────────────────────────────────────────────
// Session DB helper — same path the CLI/TUI use.
// ────────────────────────────────────────────────────────────────────────────

fn open_session_store() -> anyhow::Result<crate::sessions::SessionStore> {
    let data_dir = directories::ProjectDirs::from("", "", "rantaiclaw")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from(".rantaiclaw"));
    std::fs::create_dir_all(&data_dir)?;
    crate::sessions::SessionStore::open(&data_dir.join("sessions.db"))
}

// ────────────────────────────────────────────────────────────────────────────
// version + status + doctor
// ────────────────────────────────────────────────────────────────────────────

async fn version() -> impl IntoResponse {
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "name": "rantaiclaw",
    }))
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let cfg = state.config.lock();
    Ok(Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "provider": cfg.default_provider.clone().unwrap_or_default(),
        "model": cfg.default_model.clone().unwrap_or_default(),
        "memory_backend": cfg.memory.backend,
        "autonomy": format!("{:?}", cfg.autonomy.level),
        "workspace_dir": cfg.workspace_dir.display().to_string(),
        "paired": state.pairing.is_paired(),
        "runtime": crate::health::snapshot_json(),
    })))
}

async fn doctor(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let lib_config = state.config.lock().clone();
    let profile = crate::profile::ProfileManager::active().map_err(|e| err_500(e.into()))?;
    let ctx = crate::doctor::DoctorContext {
        profile,
        config: lib_config,
        offline: true, // brief mode — no live network probes
    };
    let results = crate::doctor::run_all(ctx, true).await;
    let summary: Vec<_> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "name": r.name,
                "category": r.category,
                "severity": format!("{:?}", r.severity),
                "message": r.message,
                "hint": r.hint,
                "duration_ms": r.duration_ms,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "results": summary })))
}

// ────────────────────────────────────────────────────────────────────────────
// agent/chat
// ────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ChatRequestBody {
    message: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    temperature: Option<f64>,
}

#[derive(Serialize)]
struct ChatResponseBody {
    text: String,
    model: String,
    provider: String,
    duration_ms: u128,
}

async fn agent_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ChatRequestBody>,
) -> Result<Json<ChatResponseBody>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    if body.message.trim().is_empty() {
        return Err(err_400("message must not be empty"));
    }

    let mut config = state.config.lock().clone();
    if let Some(p) = body.provider {
        config.default_provider = Some(p);
    }
    if let Some(m) = body.model {
        config.default_model = Some(m);
    }
    if let Some(t) = body.temperature {
        config.default_temperature = t;
    }

    let provider = config
        .default_provider
        .clone()
        .unwrap_or_else(|| "openrouter".to_string());
    let model = config
        .default_model
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    let started = std::time::Instant::now();
    let mut agent = crate::agent::Agent::from_config(&config).map_err(|e| err_500(e))?;
    let text = agent.turn(&body.message).await.map_err(|e| err_500(e))?;
    if let Ok(store) = open_session_store() {
        if let Err(err) = record_api_chat_session(&store, &model, &body.message, &text) {
            tracing::warn!(error = %err, "api agent chat session persistence failed");
        }
    }
    Ok(Json(ChatResponseBody {
        text,
        model,
        provider,
        duration_ms: started.elapsed().as_millis(),
    }))
}

fn record_api_chat_session(
    store: &crate::sessions::SessionStore,
    model: &str,
    user_message: &str,
    assistant_message: &str,
) -> anyhow::Result<String> {
    let session = store.new_session(model, "api")?;
    store.append_message(&crate::sessions::Message::user(&session.id, user_message))?;
    store.append_message(&crate::sessions::Message::assistant(
        &session.id,
        assistant_message,
    ))?;
    let title = crate::sessions::derive_session_title(user_message);
    if !title.is_empty() {
        store.set_title(&session.id, &title)?;
    }
    store.end_session(&session.id)?;
    Ok(session.id)
}

// ────────────────────────────────────────────────────────────────────────────
// sessions
// ────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
}

async fn sessions_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let store = open_session_store().map_err(err_500)?;
    let limit = q.limit.unwrap_or(50).min(500);
    let sessions = store.list_sessions(limit).map_err(err_500)?;
    let json: Vec<_> = sessions
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "title": s.title,
                "model": s.model,
                "started_at": s.started_at,
                "message_count": s.message_count,
            })
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "sessions": json, "count": json.len() }),
    ))
}

async fn sessions_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let store = open_session_store().map_err(err_500)?;
    let sessions = store.list_sessions(500).map_err(err_500)?;
    let matched: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(&id)).collect();
    let session = match matched.len() {
        0 => return Err(err_404(format!("no session matches `{id}`"))),
        1 => matched[0],
        n => return Err(err_400(format!("`{id}` is ambiguous ({n} matches)"))),
    };
    let messages = store.get_messages(&session.id).map_err(err_500)?;
    Ok(Json(serde_json::json!({
        "id": session.id,
        "title": session.title,
        "model": session.model,
        "started_at": session.started_at,
        "messages": messages.iter().map(|m| serde_json::json!({
            "role": m.role,
            "content": m.content,
            "timestamp": m.timestamp,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct SearchBody {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

async fn sessions_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SearchBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    if body.query.trim().is_empty() {
        return Err(err_400("query must not be empty"));
    }
    let store = open_session_store().map_err(err_500)?;
    let limit = body.limit.unwrap_or(20).min(200);
    let results = store.search(&body.query, limit).map_err(err_500)?;
    let json: Vec<_> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "session_id": r.session_id,
                "session_title": r.session_title,
                "role": r.role,
                "content": r.content,
                "timestamp": r.timestamp,
                "rank": r.rank,
            })
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "results": json, "count": json.len() }),
    ))
}

#[derive(Deserialize)]
struct TitleBody {
    title: String,
}

async fn sessions_set_title(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<TitleBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let store = open_session_store().map_err(err_500)?;
    let sessions = store.list_sessions(500).map_err(err_500)?;
    let matched: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(&id)).collect();
    let session = match matched.len() {
        0 => return Err(err_404(format!("no session matches `{id}`"))),
        1 => matched[0],
        n => return Err(err_400(format!("`{id}` is ambiguous ({n} matches)"))),
    };
    store.set_title(&session.id, &body.title).map_err(err_500)?;
    Ok(Json(
        serde_json::json!({ "id": session.id, "title": body.title }),
    ))
}

// ────────────────────────────────────────────────────────────────────────────
// insights
// ────────────────────────────────────────────────────────────────────────────

async fn insights(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let store = open_session_store().map_err(err_500)?;
    let sessions = store.list_sessions(10_000).map_err(err_500)?;
    let total_sessions = sessions.len();
    let total_messages: i64 = sessions.iter().map(|s| s.message_count).sum();
    let avg = if total_sessions > 0 {
        total_messages as f64 / total_sessions as f64
    } else {
        0.0
    };
    Ok(Json(serde_json::json!({
        "total_sessions": total_sessions,
        "total_messages": total_messages,
        "avg_messages_per_session": avg,
        "latest_session_id": sessions.first().map(|s| s.id.clone()),
        "latest_session_started_at": sessions.first().map(|s| s.started_at),
    })))
}

// ────────────────────────────────────────────────────────────────────────────
// skills
// ────────────────────────────────────────────────────────────────────────────

async fn skills_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let cfg = state.config.lock().clone();
    let skills = crate::skills::load_skills_with_config(&cfg.workspace_dir, &cfg);
    let json: Vec<_> = skills
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "version": s.version,
                "description": s.description,
                "tags": s.tags,
                "tools": s.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "skills": json, "count": json.len() }),
    ))
}

async fn skills_show(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let cfg = state.config.lock().clone();
    let skills = crate::skills::load_skills_with_config(&cfg.workspace_dir, &cfg);
    let s = skills
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(&name))
        .ok_or_else(|| err_404(format!("skill `{name}` not found")))?;
    Ok(Json(serde_json::json!({
        "name": s.name,
        "version": s.version,
        "description": s.description,
        "tags": s.tags,
        "tools": s.tools.iter().map(|t| serde_json::json!({
            "name": t.name,
            "description": t.description,
        })).collect::<Vec<_>>(),
    })))
}

// ────────────────────────────────────────────────────────────────────────────
// memory
// ────────────────────────────────────────────────────────────────────────────

async fn memory_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let mem: Arc<dyn crate::memory::Memory> = Arc::clone(&state.mem);
    let total = mem.count().await.map_err(err_500)?;
    let healthy = mem.health_check().await;
    Ok(Json(serde_json::json!({
        "backend": mem.name(),
        "total_entries": total,
        "healthy": healthy,
    })))
}

async fn memory_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let mem = Arc::clone(&state.mem);
    let limit = q.limit.unwrap_or(50).min(500);
    let entries = mem.list(None, None).await.map_err(err_500)?;
    let json: Vec<_> = entries
        .iter()
        .take(limit)
        .map(|e| {
            serde_json::json!({
                "key": e.key,
                "category": e.category.to_string(),
                "content": e.content,
                "timestamp": e.timestamp,
                "session_id": e.session_id,
            })
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "entries": json, "count": json.len() }),
    ))
}

// ────────────────────────────────────────────────────────────────────────────
// personality
// ────────────────────────────────────────────────────────────────────────────

async fn personality_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let profile = crate::profile::ProfileManager::active().map_err(err_500)?;
    match crate::persona::read_persona_toml(&profile).map_err(err_500)? {
        Some(p) => Ok(Json(serde_json::json!({
            "profile": profile.name,
            "preset": p.preset.slug(),
            "name": p.name,
            "timezone": p.timezone,
            "role": p.role,
            "tone": p.tone,
            "avoid": p.avoid,
        }))),
        None => Ok(Json(serde_json::json!({
            "profile": profile.name,
            "preset": null,
            "configured": false,
        }))),
    }
}

#[derive(Deserialize)]
struct PersonalityBody {
    preset: String,
}

async fn personality_set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PersonalityBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let preset = match body.preset.as_str() {
        "default" => crate::persona::PresetId::Default,
        "concise_pro" => crate::persona::PresetId::ConcisePro,
        "friendly_companion" => crate::persona::PresetId::FriendlyCompanion,
        "research_analyst" => crate::persona::PresetId::ResearchAnalyst,
        "executive_assistant" => crate::persona::PresetId::ExecutiveAssistant,
        other => return Err(err_400(format!("unknown preset `{other}`"))),
    };
    let profile = crate::profile::ProfileManager::active().map_err(err_500)?;
    let existing = crate::persona::read_persona_toml(&profile).map_err(err_500)?;
    let timezone = existing
        .as_ref()
        .map(|e| e.timezone.clone())
        .unwrap_or_else(|| "UTC".to_string());
    let name = existing
        .as_ref()
        .map(|e| e.name.clone())
        .unwrap_or_else(|| "RantaiClawAgent".to_string());
    let mut next =
        existing.unwrap_or_else(|| crate::persona::PersonaToml::default_for(&name, &timezone));
    next.preset = preset;
    crate::persona::write_persona_toml(&profile, &next).map_err(err_500)?;
    crate::persona::render_system_md(&profile, &next).map_err(err_500)?;
    Ok(Json(serde_json::json!({ "preset": preset.slug() })))
}

// ────────────────────────────────────────────────────────────────────────────
// channels (read-only listing)
// ────────────────────────────────────────────────────────────────────────────

async fn channels_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let cfg = state.config.lock();
    let mut configured: Vec<&str> = Vec::new();
    if cfg.channels_config.telegram.is_some() {
        configured.push("telegram");
    }
    if cfg.channels_config.discord.is_some() {
        configured.push("discord");
    }
    if cfg.channels_config.slack.is_some() {
        configured.push("slack");
    }
    if cfg.channels_config.mattermost.is_some() {
        configured.push("mattermost");
    }
    if cfg.channels_config.imessage.is_some() {
        configured.push("imessage");
    }
    if cfg.channels_config.signal.is_some() {
        configured.push("signal");
    }
    if cfg.channels_config.whatsapp.is_some() {
        configured.push("whatsapp");
    }
    Ok(Json(serde_json::json!({
        "configured": configured,
        "count": configured.len(),
    })))
}

// ────────────────────────────────────────────────────────────────────────────
// providers (read-only catalog)
// ────────────────────────────────────────────────────────────────────────────

async fn providers_list(
    State(_state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    let providers = crate::providers::list_providers();
    let json: Vec<_> = providers
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.name,
                "display_name": p.display_name,
                "aliases": p.aliases,
                "local": p.local,
            })
        })
        .collect();
    Ok(Json(
        serde_json::json!({ "providers": json, "count": json.len() }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_api_chat_session_persists_user_and_assistant_messages() {
        let store = crate::sessions::SessionStore::in_memory().unwrap();

        let id = record_api_chat_session(
            &store,
            "test-model",
            "Summarize the runtime contract",
            "Runtime contract summary.",
        )
        .unwrap();

        let session = store.get_session(&id).unwrap().unwrap();
        assert_eq!(session.source, "api");
        assert_eq!(session.model, "test-model");
        assert_eq!(session.message_count, 2);
        assert_eq!(
            session.title.as_deref(),
            Some("Summarize the runtime contract")
        );
        assert!(session.ended_at.is_some());

        let messages = store.get_messages(&id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "Summarize the runtime contract");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "Runtime contract summary.");
    }
}
