//! Config API handlers for live configuration updates.
//!
//! These handlers allow the platform to hot-swap configuration on a running
//! container without restart. All endpoints require Bearer token auth.

use std::collections::HashMap;

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::channels::traits::Channel;
use crate::channels::{
    DiscordChannel, MattermostChannel, SignalChannel, SlackChannel, TelegramChannel,
};
use crate::config::schema::{
    DiscordConfig, MattermostConfig, McpServerConfig, SignalConfig, SlackConfig, TelegramConfig,
};

use super::AppState;

// ── Response type ────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ConfigResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ConfigResponse {
    fn success(applied: serde_json::Value) -> Self {
        Self {
            ok: true,
            applied: Some(applied),
            errors: None,
            error: None,
        }
    }

    fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            applied: None,
            errors: None,
            error: Some(msg.into()),
        }
    }

    fn partial(applied: serde_json::Value, errors: HashMap<String, String>) -> Self {
        Self {
            ok: errors.is_empty(),
            applied: Some(applied),
            errors: if errors.is_empty() { None } else { Some(errors) },
            error: None,
        }
    }
}

// ── Auth helper ──────────────────────────────────────────────────

/// Verify Bearer token auth. Returns `None` if authorized, or an error response.
fn check_auth(state: &AppState, headers: &HeaderMap) -> Option<(StatusCode, Json<ConfigResponse>)> {
    if !state.pairing.require_pairing() {
        return None;
    }
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = auth.strip_prefix("Bearer ").unwrap_or("");
    if !state.pairing.is_authenticated(token) {
        Some((
            StatusCode::UNAUTHORIZED,
            Json(ConfigResponse::error(
                "Unauthorized — pair first via POST /pair",
            )),
        ))
    } else {
        None
    }
}

// ── GET /config ──────────────────────────────────────────────────

/// Returns the current running config as JSON.
pub async fn get_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(err) = check_auth(&state, &headers) {
        return err;
    }

    let config = state.config.read().await;
    match serde_json::to_value(&*config) {
        Ok(val) => (StatusCode::OK, Json(ConfigResponse::success(val))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ConfigResponse::error(format!("Failed to serialize config: {e}"))),
        ),
    }
}

// ── GET /config/channels ─────────────────────────────────────────

/// Returns channel status map from the registry.
pub async fn get_channels_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(err) = check_auth(&state, &headers) {
        return err;
    }

    let registry = state.channel_registry.read().await;
    let statuses = registry.list_channels();
    let val = serde_json::to_value(&statuses).unwrap_or_default();
    (StatusCode::OK, Json(ConfigResponse::success(val)))
}

// ── GET /config/mcp-servers ──────────────────────────────────────

/// Returns MCP server status map from the registry.
pub async fn get_mcp_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(err) = check_auth(&state, &headers) {
        return err;
    }

    let registry = state.mcp_registry.read().await;
    let statuses = registry.list_servers();
    let val = serde_json::to_value(&statuses).unwrap_or_default();
    (StatusCode::OK, Json(ConfigResponse::success(val)))
}

// ── PATCH /config/channels ───────────────────────────────────────

/// Add/update/remove channels. Body: `{ "id": config | null }`.
/// `null` removes the channel; a config object adds or updates it.
pub async fn patch_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<HashMap<String, Option<serde_json::Value>>>,
) -> impl IntoResponse {
    if let Some(err) = check_auth(&state, &headers) {
        return err;
    }

    let mut registry = state.channel_registry.write().await;
    let mut applied = Vec::new();
    let mut errors = HashMap::new();

    for (id, maybe_config) in body {
        match maybe_config {
            None => {
                // Remove channel
                match registry.remove_channel(&id).await {
                    Ok(()) => {
                        info!("[ConfigAPI] Removed channel '{}'", id);
                        applied.push(id.clone());
                    }
                    Err(e) => {
                        warn!("[ConfigAPI] Failed to remove channel '{}': {}", id, e);
                        errors.insert(id.clone(), e.to_string());
                    }
                }
            }
            Some(config) => {
                let channel_id = id.clone();
                let cfg = config.clone();
                match registry
                    .update_channel(id.clone(), config, |c| {
                        build_channel_from_config(channel_id.clone(), c)
                    })
                    .await
                {
                    Ok(()) => {
                        info!("[ConfigAPI] Updated channel '{}'", id);
                        applied.push(id.clone());
                    }
                    Err(e) => {
                        warn!("[ConfigAPI] Failed to update channel '{}': {}", id, e);
                        // Try add if update failed because channel didn't exist
                        let cid = id.clone();
                        match registry
                            .add_channel(id.clone(), cfg, |c| {
                                build_channel_from_config(cid, c)
                            })
                            .await
                        {
                            Ok(()) => {
                                info!("[ConfigAPI] Added channel '{}'", id);
                                applied.push(id.clone());
                            }
                            Err(e2) => {
                                errors.insert(id.clone(), format!("{e}; add also failed: {e2}"));
                            }
                        }
                    }
                }
            }
        }
    }

    let val = serde_json::to_value(&applied).unwrap_or_default();
    (StatusCode::OK, Json(ConfigResponse::partial(val, errors)))
}

/// Build a channel from its ID and JSON config.
///
/// Mirrors the factories in `register_configured_channels` so that live PATCH
/// /config/channels creates the same channel objects as startup-time registration.
async fn build_channel_from_config(
    channel_id: String,
    config: serde_json::Value,
) -> anyhow::Result<Box<dyn Channel + Send + Sync>> {
    match channel_id.as_str() {
        "telegram" => {
            let tg: TelegramConfig = serde_json::from_value(config)?;
            let ch = TelegramChannel::new(tg.bot_token, tg.allowed_users, tg.mention_only)
                .with_streaming(tg.stream_mode, tg.draft_update_interval_ms);
            Ok(Box::new(ch))
        }
        "discord" => {
            let dc: DiscordConfig = serde_json::from_value(config)?;
            let ch = DiscordChannel::new(
                dc.bot_token,
                dc.guild_id,
                dc.allowed_users,
                dc.listen_to_bots,
                dc.mention_only,
            );
            Ok(Box::new(ch))
        }
        "slack" => {
            let sl: SlackConfig = serde_json::from_value(config)?;
            let ch = SlackChannel::new(sl.bot_token, sl.channel_id, sl.allowed_users);
            Ok(Box::new(ch))
        }
        "mattermost" => {
            let mm: MattermostConfig = serde_json::from_value(config)?;
            let ch = MattermostChannel::new(
                mm.url,
                mm.bot_token,
                mm.channel_id,
                mm.allowed_users,
                mm.thread_replies.unwrap_or(true),
                mm.mention_only.unwrap_or(false),
            );
            Ok(Box::new(ch))
        }
        "signal" => {
            let sig: SignalConfig = serde_json::from_value(config)?;
            let ch = SignalChannel::new(
                sig.http_url,
                sig.account,
                sig.group_id,
                sig.allowed_from,
                sig.ignore_attachments,
                sig.ignore_stories,
            );
            Ok(Box::new(ch))
        }
        other => Err(anyhow::anyhow!(
            "Unknown channel type '{}'. Supported: telegram, discord, slack, mattermost, signal",
            other
        )),
    }
}

// ── PATCH /config/mcp-servers ────────────────────────────────────

/// Add/update/remove MCP servers. Body: `{ "id": McpServerConfig | null }`.
pub async fn patch_mcp_servers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<HashMap<String, Option<McpServerConfig>>>,
) -> impl IntoResponse {
    if let Some(err) = check_auth(&state, &headers) {
        return err;
    }

    let mut registry = state.mcp_registry.write().await;
    let mut config = state.config.write().await;
    let mut applied = Vec::new();
    let mut errors = HashMap::new();

    for (id, maybe_config) in body {
        match maybe_config {
            None => {
                // Remove server
                match registry.remove_server(&id).await {
                    Ok(()) => {
                        config.mcp_servers.remove(&id);
                        info!("[ConfigAPI] Removed MCP server '{}'", id);
                        applied.push(id.clone());
                    }
                    Err(e) => {
                        warn!("[ConfigAPI] Failed to remove MCP server '{}': {}", id, e);
                        errors.insert(id.clone(), e.to_string());
                    }
                }
            }
            Some(server_config) => {
                match registry.update_server(id.clone(), server_config.clone()).await {
                    Ok(()) => {
                        config.mcp_servers.insert(id.clone(), server_config);
                        info!("[ConfigAPI] Updated MCP server '{}'", id);
                        applied.push(id.clone());
                    }
                    Err(e) => {
                        warn!("[ConfigAPI] Failed to update MCP server '{}': {}", id, e);
                        errors.insert(id.clone(), e.to_string());
                    }
                }
            }
        }
    }

    // Broadcast updated config
    let _ = state.config_tx.send(config.clone());

    let val = serde_json::to_value(&applied).unwrap_or_default();
    (StatusCode::OK, Json(ConfigResponse::partial(val, errors)))
}

// ── PATCH /config/model ──────────────────────────────────────────

#[derive(Deserialize)]
pub struct PatchModelBody {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub temperature: Option<f64>,
}

/// Hot-swap provider/model/temperature.
pub async fn patch_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PatchModelBody>,
) -> impl IntoResponse {
    if let Some(err) = check_auth(&state, &headers) {
        return err;
    }

    let mut config = state.config.write().await;
    let mut changes = serde_json::Map::new();

    if let Some(ref provider) = body.provider {
        config.default_provider = Some(provider.clone());
        changes.insert("default_provider".into(), serde_json::json!(provider));
    }
    if let Some(ref model) = body.model {
        config.default_model = Some(model.clone());
        changes.insert("default_model".into(), serde_json::json!(model));
    }
    if let Some(temp) = body.temperature {
        config.default_temperature = temp;
        changes.insert("default_temperature".into(), serde_json::json!(temp));
    }

    // Persist via runtime overrides
    let config_path = config.config_path.clone();
    if let Some(ref provider) = body.provider {
        if let Err(e) = crate::config::runtime::write_runtime_section(
            &config_path,
            "default_provider",
            toml::Value::String(provider.clone()),
        ) {
            warn!("[ConfigAPI] Failed to persist default_provider: {}", e);
        }
    }
    if let Some(ref model) = body.model {
        if let Err(e) = crate::config::runtime::write_runtime_section(
            &config_path,
            "default_model",
            toml::Value::String(model.clone()),
        ) {
            warn!("[ConfigAPI] Failed to persist default_model: {}", e);
        }
    }
    if let Some(temp) = body.temperature {
        if let Err(e) = crate::config::runtime::write_runtime_section(
            &config_path,
            "default_temperature",
            toml::Value::Float(temp),
        ) {
            warn!("[ConfigAPI] Failed to persist default_temperature: {}", e);
        }
    }

    // Broadcast updated config
    let _ = state.config_tx.send(config.clone());
    info!("[ConfigAPI] Model config updated: {:?}", changes);

    (
        StatusCode::OK,
        Json(ConfigResponse::success(serde_json::Value::Object(changes))),
    )
}

// ── PATCH /config/tools ──────────────────────────────────────────

#[derive(Deserialize)]
pub struct PatchToolsBody {
    pub auto_approve: Option<Vec<String>>,
}

/// Update tool permissions (auto_approve list).
pub async fn patch_tools(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PatchToolsBody>,
) -> impl IntoResponse {
    if let Some(err) = check_auth(&state, &headers) {
        return err;
    }

    let mut config = state.config.write().await;
    let mut changes = serde_json::Map::new();

    if let Some(ref auto_approve) = body.auto_approve {
        config.autonomy.auto_approve = auto_approve.clone();
        changes.insert("auto_approve".into(), serde_json::json!(auto_approve));

        // Persist via runtime overrides — write the full [autonomy] section
        let config_path = config.config_path.clone();
        let mut autonomy_table = toml::map::Map::new();
        autonomy_table.insert(
            "auto_approve".into(),
            toml::Value::Array(
                auto_approve
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
        );
        if let Err(e) = crate::config::runtime::write_runtime_section(
            &config_path,
            "autonomy",
            toml::Value::Table(autonomy_table),
        ) {
            warn!("[ConfigAPI] Failed to persist autonomy.auto_approve: {}", e);
        }
    }

    // Broadcast updated config
    let _ = state.config_tx.send(config.clone());
    info!("[ConfigAPI] Tools config updated: {:?}", changes);

    (
        StatusCode::OK,
        Json(ConfigResponse::success(serde_json::Value::Object(changes))),
    )
}

// ── PATCH /config/agent ──────────────────────────────────────────

#[derive(Deserialize)]
pub struct PatchAgentBody {
    /// System prompt to write as workspace/SYSTEM_PROMPT.md
    pub system_prompt: Option<String>,
    /// Workspace files to write: { "relative/path": "contents" }
    pub workspace_files: Option<HashMap<String, String>>,
}

/// Update system prompt and/or write workspace files to disk.
pub async fn patch_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PatchAgentBody>,
) -> impl IntoResponse {
    if let Some(err) = check_auth(&state, &headers) {
        return err;
    }

    let config = state.config.read().await;
    let workspace_dir = config.workspace_dir.clone();
    drop(config);

    let mut changes = serde_json::Map::new();
    let mut errors = HashMap::new();

    // Write system prompt as a workspace file
    if let Some(ref prompt) = body.system_prompt {
        let prompt_path = workspace_dir.join("SYSTEM_PROMPT.md");
        match std::fs::write(&prompt_path, prompt) {
            Ok(()) => {
                info!(
                    "[ConfigAPI] Wrote system prompt ({} bytes) to {}",
                    prompt.len(),
                    prompt_path.display()
                );
                changes.insert("system_prompt".into(), serde_json::json!(true));
            }
            Err(e) => {
                warn!("[ConfigAPI] Failed to write system prompt: {}", e);
                errors.insert("system_prompt".into(), e.to_string());
            }
        }
    }

    // Write workspace files
    if let Some(ref files) = body.workspace_files {
        let mut written = Vec::new();
        for (rel_path, contents) in files {
            // Prevent path traversal
            if rel_path.contains("..") {
                errors.insert(rel_path.clone(), "Path traversal not allowed".into());
                continue;
            }

            let full_path = workspace_dir.join(rel_path);
            // Ensure parent directory exists
            if let Some(parent) = full_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    errors.insert(rel_path.clone(), format!("Failed to create directory: {e}"));
                    continue;
                }
            }

            match std::fs::write(&full_path, contents) {
                Ok(()) => {
                    info!(
                        "[ConfigAPI] Wrote workspace file '{}' ({} bytes)",
                        rel_path,
                        contents.len()
                    );
                    written.push(rel_path.clone());
                }
                Err(e) => {
                    warn!("[ConfigAPI] Failed to write workspace file '{}': {}", rel_path, e);
                    errors.insert(rel_path.clone(), e.to_string());
                }
            }
        }
        changes.insert("workspace_files".into(), serde_json::json!(written));
    }

    let val = serde_json::Value::Object(changes);
    if errors.is_empty() {
        (StatusCode::OK, Json(ConfigResponse::success(val)))
    } else {
        (StatusCode::OK, Json(ConfigResponse::partial(val, errors)))
    }
}
