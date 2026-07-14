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
use crate::config::schema::{McpServerConfig, TelegramConfig};
use crate::security::AutonomyLevel;

/// Build the `/api/v1/config*` router. Merged alongside `api_v1::router()` so
/// it shares the small-body limit + timeout middleware.
pub fn router() -> Router<AppState> {
    #[cfg_attr(not(feature = "kb"), allow(unused_mut))]
    let mut router = Router::new()
        .route("/api/v1/config", get(get_config))
        .route("/api/v1/config/model", put(set_model))
        .route("/api/v1/config/autonomy", put(set_autonomy))
        .route("/api/v1/secrets", get(get_secrets).put(set_secrets))
        .route(
            "/api/v1/config/mcp_servers/{name}",
            post(add_mcp_server).delete(remove_mcp_server),
        )
        // Connect / update (allowlist) / disconnect a Telegram channel from the console.
        .route(
            "/api/v1/channels/telegram",
            post(connect_telegram).delete(disconnect_telegram),
        );
    // Knowledge Base credential status/setter — only when the KB feature is built.
    #[cfg(feature = "kb")]
    {
        router = router.route(
            "/api/v1/config/knowledge",
            get(get_knowledge).put(set_knowledge),
        );
    }
    router
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
    // Never expose raw secrets over the API. This is a response-only copy; the
    // in-memory + on-disk config keep their real values.
    redact_config_secrets(&mut cfg);
    let val = serde_json::to_value(&cfg).map_err(err_500)?;
    Ok(Json(val))
}

/// Clear every secret field before a Config is serialized into an API response.
/// Keep in sync with the encrypt/decrypt lists in config::schema.
fn redact_config_secrets(cfg: &mut crate::config::Config) {
    cfg.api_key = None;
    // Per-provider keys are the same credential class as `api_key` and are
    // decrypted in memory — clear the whole map so none leak in the response.
    cfg.provider_api_keys.clear();
    cfg.composio.api_key = None;
    cfg.browser.computer_use.api_key = None;
    cfg.web_search.brave_api_key = None;
    cfg.storage.provider.config.db_url = None;
    for agent in cfg.agents.values_mut() {
        agent.api_key = None;
    }
    // Channel credentials are secrets too — never return a live bot token over
    // the API (the connect flow already avoids echoing it).
    if let Some(tg) = cfg.channels_config.telegram.as_mut() {
        tg.bot_token.clear();
    }
    // Knowledge Base keys are encrypted at rest like `api_key`; redact them too.
    cfg.knowledge.embedding_api_key = None;
    cfg.knowledge.vision_api_key = None;
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

/// If a provider switch left the active provider without a usable credential,
/// return a warning to surface in the UI. The switch still persisted — this is a
/// heads-up that channels (and web chat) can't use the new provider until a key
/// is configured. Hedged wording so it's also correct for keyless providers.
fn provider_switch_warning(cfg: &crate::config::Config, provider_changed: bool) -> Option<String> {
    if !provider_changed {
        return None;
    }
    let provider = cfg.default_provider.as_deref()?;
    if crate::providers::has_usable_credential(provider, cfg.api_key.as_deref()) {
        return None;
    }
    Some(format!(
        "No API key found for '{provider}'. If it needs one, channels and chat \
         can't use it until you add it in Configuration."
    ))
}

async fn set_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ModelBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let provider_changed = body.provider.is_some();
    let mut cfg = state.config.lock().clone();
    if let Some(p) = body.provider {
        let new_provider = if p.trim().is_empty() {
            None
        } else {
            Some(p.trim().to_string())
        };
        switch_active_provider(&mut cfg, new_provider);
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
    let warning = provider_switch_warning(&cfg, provider_changed);
    let mut resp = json!({
        "default_provider": cfg.default_provider,
        "default_model": cfg.default_model,
        "default_temperature": cfg.default_temperature,
    });
    if let Some(w) = warning {
        resp["warning"] = json!(w);
    }
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
    // Keep the on-disk preset marker (which the agent's system prompt reads via
    // `read_active_preset`) in step with the enforced policy, so the model never
    // narrates a stale approval mode. Marker-only: the enforcement gate reads
    // `config.toml` (updated by `persist_and_swap` below), so this touches
    // nothing the gate depends on. Best-effort — a marker write failure must not
    // fail the autonomy update itself.
    if let Ok(profile) = crate::profile::ProfileManager::active() {
        let preset = crate::approval::policy_writer::preset_for_autonomy(&cfg.autonomy);
        if let Err(e) =
            crate::approval::policy_writer::write_active_preset(&profile.policy_dir(), preset)
        {
            tracing::warn!(error = %e, "failed to sync policy preset marker after autonomy change");
        }
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

// ── POST/DELETE /channels/telegram (experimental connect) ────────────────────

#[derive(Deserialize)]
struct TelegramConnectBody {
    /// Bot API token from @BotFather. Validated live (`getMe`) before persisting.
    /// Optional: omit (or send empty) to update `allowed_users` on an
    /// already-connected channel without re-entering the token.
    #[serde(default)]
    bot_token: String,
    /// Telegram user ids/usernames allowed to talk to the bot. Empty = deny all
    /// (the channel stays secure until owners are added).
    #[serde(default)]
    allowed_users: Vec<String>,
}

/// What to do with the Telegram bot token on a connect / allowlist-update request.
#[derive(Debug)]
enum TokenPlan {
    /// A new, shape-valid token was supplied — the caller must live-validate it
    /// (`getMe`) before persisting.
    Validate(String),
    /// No token supplied but one is already configured — keep the saved token so
    /// an operator can update the allowlist without re-entering it.
    KeepExisting,
}

/// Decide how to treat the token on a `POST /channels/telegram` request: a
/// supplied token is shape-checked (and must then be live-validated by the
/// caller); an omitted token keeps the existing one (allowlist-only update), or
/// errors when nothing is configured yet.
fn plan_telegram_token(
    existing: Option<&TelegramConfig>,
    provided: &str,
) -> Result<TokenPlan, ApiError> {
    let token = provided.trim();
    if token.is_empty() {
        return if existing.is_some() {
            Ok(TokenPlan::KeepExisting)
        } else {
            Err(err_400(
                "bot_token is required to connect a new Telegram channel",
            ))
        };
    }
    if !is_valid_telegram_token(token) {
        return Err(err_400(
            "bot_token is not a valid Telegram token (expected `<digits>:<token>`)",
        ));
    }
    Ok(TokenPlan::Validate(token.to_string()))
}

/// Build the `TelegramConfig` to persist from the existing one (if any) plus
/// this request's changes. A `new_token` (already validated) replaces the token;
/// omitting it keeps the saved token for an allowlist-only update. Unrelated
/// options (stream mode, mention-only, …) are always preserved.
fn apply_telegram_update(
    existing: Option<TelegramConfig>,
    new_token: Option<&str>,
    allowed_users: Vec<String>,
) -> Result<TelegramConfig, ApiError> {
    // Start from the existing config so options survive; otherwise a minimal one
    // whose optional fields inherit their configured defaults via serde.
    let mut tg = match existing {
        Some(tg) => tg,
        None => serde_json::from_value(json!({ "bot_token": "", "allowed_users": [] }))
            .map_err(err_500)?,
    };
    if let Some(token) = new_token {
        tg.bot_token = token.to_string();
    }
    tg.allowed_users = allowed_users;
    Ok(tg)
}

/// After a channel config change, ask a running managed daemon to reload so the
/// channels runtime picks up the new / removed channel. The channels supervisor
/// captures its channel set (and each channel's allowlist) at startup and is not
/// hot-reloaded from disk by the gateway, so a connect / allowlist edit / disconnect
/// only takes effect once the runtime restarts.
///
/// Spawned detached with a short delay so the HTTP response flushes before a
/// systemd restart bounces this process — a `restart` job is owned by the service
/// manager, so it completes even though this process is replaced. No-op
/// (`Ok(false)`) when the runtime isn't a managed service; the operator restarts
/// `rantaiclaw daemon` manually in that case.
fn schedule_daemon_reload() {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        match tokio::task::spawn_blocking(crate::channels::reload_managed_daemon).await {
            Ok(Ok(true)) => {
                tracing::info!(target: "gateway", "channel change: reloaded managed daemon service")
            }
            Ok(Ok(false)) => tracing::info!(
                target: "gateway",
                "channel change saved; no managed daemon service to reload (restart `rantaiclaw daemon` to apply)"
            ),
            Ok(Err(e)) => tracing::warn!(
                target: "gateway",
                "channel change saved but managed daemon reload failed: {e}"
            ),
            Err(e) => {
                tracing::warn!(target: "gateway", "managed daemon reload task failed to join: {e}")
            }
        }
    });
}

/// Connect a Telegram channel from the console: validate the token against
/// Telegram, then persist it into `channels_config.telegram`. The token is a
/// secret and is never echoed back in responses. NOTE: channel tokens are
/// currently stored in plaintext in `config.toml` (unlike `api_key`, they are
/// not yet routed through the at-rest secret encryption) — treat the host /
/// config file as trusted. `get_config` redacts the token from reads.
///
/// The polling runtime is a separate process (`rantaiclaw channels`), so this
/// configures + validates the channel; it begins receiving messages when that
/// runtime (re)starts. Experimental.
async fn connect_telegram(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TelegramConnectBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;

    // Snapshot just the current Telegram config (short-lived lock) to decide
    // whether this is a fresh connect, a token replacement, or an allowlist-only
    // update. The token shape is enforced here so no URL-significant character can
    // reach the interpolated `getMe` request path.
    let existing = state.config.lock().channels_config.telegram.clone();
    let plan = plan_telegram_token(existing.as_ref(), &body.bot_token)?;

    // Only a newly supplied token needs the live `getMe` probe (fail closed so we
    // never save a credential that doesn't work). An allowlist-only update keeps
    // the already-validated saved token and skips the network call. The probe is
    // side-effect-free (not a full TelegramChannel, which would set up pairing +
    // print a code).
    let (new_token, bot_username) = match plan {
        TokenPlan::Validate(token) => {
            let username = crate::channels::telegram::validate_bot_token(&token)
                .await
                .map_err(|e| {
                    // `e` does not contain the token. We can't always tell a bad
                    // token from an unreachable Telegram, so the message covers both.
                    err_400(format!(
                        "could not validate the bot token with Telegram (invalid token, or Telegram unreachable): {e}"
                    ))
                })?;
            (Some(token), Some(username))
        }
        TokenPlan::KeepExisting => (None, None),
    };

    let tg = apply_telegram_update(existing, new_token.as_deref(), body.allowed_users.clone())?;

    // Clone the full config only now (after the await) to keep the window where a
    // concurrent config write could be clobbered as small as possible.
    let mut cfg = state.config.lock().clone();
    cfg.channels_config.telegram = Some(tg);
    persist_and_swap(&state, cfg).await?;

    // The running channels runtime doesn't hot-reload channel config from disk,
    // so ask a managed daemon to restart and pick up the change (detached, after
    // the response flushes).
    schedule_daemon_reload();

    let warning = if body.allowed_users.is_empty() {
        Some("allowed_users is empty — the bot will deny ALL senders until you add Telegram user ids/usernames.")
    } else if body.allowed_users.iter().any(|u| u.trim() == "*") {
        Some("allowed_users contains \"*\" — the bot will respond to ANYONE who messages it. Use specific user ids/usernames unless this is intentional.")
    } else {
        None
    };
    Ok(Json(json!({
        "connected": true,
        "channel": "telegram",
        "bot_username": bot_username,
        "allowed_users": body.allowed_users.len(),
        "warning": warning,
        "note": "Saved. Reloading the runtime to apply — automatic if RantaiClaw runs as a managed service, otherwise restart `rantaiclaw daemon`.",
    })))
}

/// Whether `token` matches the Telegram bot-token shape `<digits>:<token-chars>`.
/// Conservative on purpose: only ASCII digits before the colon and
/// `[A-Za-z0-9_-]` after it, so no URL-significant character can reach the
/// interpolated request path.
fn is_valid_telegram_token(token: &str) -> bool {
    let Some((id, secret)) = token.split_once(':') else {
        return false;
    };
    !id.is_empty()
        && id.bytes().all(|b| b.is_ascii_digit())
        && secret.len() >= 20
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Disconnect the Telegram channel: clear `channels_config.telegram` + persist.
async fn disconnect_telegram(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let mut cfg = state.config.lock().clone();
    let was_configured = cfg.channels_config.telegram.is_some();
    cfg.channels_config.telegram = None;
    persist_and_swap(&state, cfg).await?;

    // Only bounce the runtime if we actually removed a running channel.
    if was_configured {
        schedule_daemon_reload();
    }

    Ok(Json(
        json!({ "disconnected": was_configured, "channel": "telegram" }),
    ))
}

// ── GET/PUT /secrets ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SecretsBody {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_url: Option<String>,
}

/// True when a non-empty provider key is configured.
fn api_key_present(cfg: &crate::config::Config) -> bool {
    cfg.api_key
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Non-secret view of the active provider credential: which provider is selected,
/// whether a key is present (never the key itself), the optional base-URL override,
/// and whether at-rest encryption is on.
fn secrets_view(cfg: &crate::config::Config) -> serde_json::Value {
    json!({
        "provider": cfg.default_provider.clone().unwrap_or_default(),
        "api_url": cfg.api_url,
        "api_key_present": api_key_present(cfg),
        "encrypt_at_rest": cfg.secrets.encrypt,
    })
}

/// Apply a secrets mutation: a provided field sets the value (empty string clears
/// it), an omitted field leaves the existing value untouched.
/// Switch the active provider, carrying per-provider keys correctly: preserve
/// the outgoing provider's key in the per-provider store (covers keys that only
/// ever lived in the top-level `api_key`), then point the top-level `api_key` at
/// the new provider's stored key (`None` if it has none yet, so the console
/// prompts for it). This is what stops a switch from sending the previous
/// provider's key to the new one.
fn switch_active_provider(cfg: &mut crate::config::Config, new_provider: Option<String>) {
    if let (Some(old), Some(key)) = (cfg.default_provider.as_deref(), cfg.api_key.as_deref()) {
        let key = key.trim();
        if !key.is_empty() {
            let canon = crate::providers::normalize_provider_name(old);
            cfg.provider_api_keys
                .entry(canon)
                .or_insert_with(|| key.to_string());
        }
    }
    cfg.api_key = new_provider
        .as_deref()
        .map(crate::providers::normalize_provider_name)
        .and_then(|canon| cfg.provider_api_keys.get(&canon).cloned());
    cfg.default_provider = new_provider;
}

fn apply_secrets(cfg: &mut crate::config::Config, body: &SecretsBody) {
    if let Some(k) = body.api_key.as_ref() {
        let k = k.trim();
        // Mirror the key into the per-provider store, keyed by the active
        // provider, so switching providers later resolves the right credential
        // (and switching back restores this one). Empty clears both.
        if let Some(p) = cfg.default_provider.as_deref() {
            let canon = crate::providers::normalize_provider_name(p);
            if k.is_empty() {
                cfg.provider_api_keys.remove(&canon);
            } else {
                cfg.provider_api_keys.insert(canon, k.to_string());
            }
        }
        cfg.api_key = if k.is_empty() {
            None
        } else {
            Some(k.to_string())
        };
    }
    if let Some(u) = body.api_url.as_ref() {
        let u = u.trim();
        cfg.api_url = if u.is_empty() {
            None
        } else {
            Some(u.to_string())
        };
    }
}

/// `GET /secrets` — presence-only view; the raw key is never returned.
async fn get_secrets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = state.config.lock().clone();
    Ok(Json(secrets_view(&cfg)))
}

/// `PUT /secrets {api_key?, api_url?}` — set the active provider's key/base-URL and
/// persist (encrypted at rest via [`Config::save`]). Returns presence, not the key.
async fn set_secrets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SecretsBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let mut cfg = state.config.lock().clone();
    apply_secrets(&mut cfg, &body);
    let present = api_key_present(&cfg);
    persist_and_swap(&state, cfg).await?;
    Ok(Json(json!({ "ok": true, "api_key_present": present })))
}

// ── GET/PUT /config/knowledge (Knowledge Base credentials) ───────────────────

#[cfg(feature = "kb")]
#[derive(serde::Deserialize)]
struct KnowledgeBody {
    #[serde(default)]
    embedding_api_key: Option<String>,
    #[serde(default)]
    vision_api_key: Option<String>,
}

/// Effective source of a resolved key, reported without revealing it.
#[cfg(feature = "kb")]
fn knowledge_source(env_var: &str, cfg_val: Option<&str>) -> &'static str {
    if std::env::var(env_var)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        "env"
    } else if cfg_val.map(|v| !v.is_empty()).unwrap_or(false) {
        "config"
    } else {
        "none"
    }
}

/// `GET /config/knowledge` — presence + source only; a key value is never returned.
#[cfg(feature = "kb")]
async fn get_knowledge(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = state.config.lock().clone();
    let emb_src = knowledge_source(
        "KB_EMBEDDING_API_KEY",
        cfg.knowledge.embedding_api_key.as_deref(),
    );
    let vis_src = knowledge_source(
        "KB_EXTRACT_VISION_API_KEY",
        cfg.knowledge.vision_api_key.as_deref(),
    );
    Ok(Json(json!({
        "embedding_configured": emb_src != "none",
        "vision_configured": vis_src != "none",
        "source": emb_src,
    })))
}

/// `PUT /config/knowledge {embedding_api_key?, vision_api_key?}` — set/clear the KB
/// keys (persisted encrypted at rest), flush the KB cache, and reload the daemon.
/// An omitted field leaves the existing value untouched; an empty string clears it.
/// Returns presence booleans only, never the key.
#[cfg(feature = "kb")]
async fn set_knowledge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<KnowledgeBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let mut cfg = state.config.lock().clone();
    if let Some(k) = body.embedding_api_key {
        let k = k.trim();
        cfg.knowledge.embedding_api_key = if k.is_empty() {
            None
        } else {
            Some(k.to_string())
        };
    }
    if let Some(k) = body.vision_api_key {
        let k = k.trim();
        cfg.knowledge.vision_api_key = if k.is_empty() {
            None
        } else {
            Some(k.to_string())
        };
    }
    persist_and_swap(&state, cfg).await?;
    // New credentials invalidate any cached KB embedding/extraction context.
    crate::kb::axi::clear_kb_ctx().await;
    schedule_daemon_reload();
    let cfg = state.config.lock().clone();
    Ok(Json(json!({
        "embedding_configured": cfg.knowledge.embedding_api_key.is_some(),
        "vision_configured": cfg.knowledge.vision_api_key.is_some(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn redact_config_secrets_clears_knowledge_keys() {
        let mut cfg = Config::default();
        cfg.knowledge.embedding_api_key = Some("sk-embed-secret".into());
        cfg.knowledge.vision_api_key = Some("sk-vision-secret".into());
        redact_config_secrets(&mut cfg);
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("sk-embed-secret"));
        assert!(!json.contains("sk-vision-secret"));
        assert_eq!(cfg.knowledge.embedding_api_key, None);
        assert_eq!(cfg.knowledge.vision_api_key, None);
    }

    #[test]
    fn provider_switch_warning_flags_a_provider_without_a_credential() {
        // An unknown provider name has no env candidates, so with no config key it
        // resolves no credential -> warn. Env-independent, hence deterministic.
        let mut cfg = Config::default();
        cfg.default_provider = Some("totally-unknown-provider-xyz".into());
        cfg.api_key = None;
        assert!(
            provider_switch_warning(&cfg, true).is_some(),
            "warns when the switched provider has no usable credential"
        );

        // A configured key resolves a credential -> no warning.
        cfg.api_key = Some("sk-configured".into());
        assert!(provider_switch_warning(&cfg, true).is_none());

        // No provider change -> never warns.
        cfg.api_key = None;
        assert!(provider_switch_warning(&cfg, false).is_none());
    }

    #[test]
    fn redact_config_secrets_clears_per_provider_keys() {
        // `provider_api_keys` is decrypted in memory and serialized; it must not
        // survive redaction into a `GET /config` response.
        let mut cfg = Config::default();
        cfg.provider_api_keys
            .insert("openai".into(), "sk-openai-secret".into());
        cfg.provider_api_keys
            .insert("minimax".into(), "mm-secret".into());
        redact_config_secrets(&mut cfg);
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("sk-openai-secret"), "leaked in:\n{json}");
        assert!(!json.contains("mm-secret"), "leaked in:\n{json}");
        assert!(cfg.provider_api_keys.is_empty());
    }

    #[test]
    fn apply_secrets_mirrors_key_into_per_provider_store() {
        let mut cfg = Config::default();
        cfg.default_provider = Some("openai".into());
        apply_secrets(
            &mut cfg,
            &SecretsBody {
                api_key: Some("  sk-openai  ".into()),
                api_url: None,
            },
        );
        assert_eq!(cfg.api_key.as_deref(), Some("sk-openai"));
        assert_eq!(
            cfg.provider_api_keys.get("openai").map(String::as_str),
            Some("sk-openai")
        );
    }

    #[test]
    fn switch_active_provider_carries_per_provider_keys() {
        let mut cfg = Config::default();
        // Pre-existing setup: minimax active with its key only in top-level.
        cfg.default_provider = Some("minimax".into());
        cfg.api_key = Some("minimax-key".into());

        // Switch to openai (no key yet): top-level clears, minimax key preserved.
        switch_active_provider(&mut cfg, Some("openai".into()));
        assert_eq!(cfg.default_provider.as_deref(), Some("openai"));
        assert_eq!(cfg.api_key, None, "openai has no saved key yet");
        assert_eq!(
            cfg.provider_api_keys.get("minimax").map(String::as_str),
            Some("minimax-key"),
            "previous provider's key must be preserved"
        );

        // Save the openai key, switch back to minimax: its key returns.
        apply_secrets(
            &mut cfg,
            &SecretsBody {
                api_key: Some("openai-key".into()),
                api_url: None,
            },
        );
        switch_active_provider(&mut cfg, Some("minimax".into()));
        assert_eq!(cfg.api_key.as_deref(), Some("minimax-key"));
        assert_eq!(
            cfg.resolve_key_for_provider("openai").as_deref(),
            Some("openai-key")
        );
        assert_eq!(
            cfg.resolve_key_for_provider("minimax").as_deref(),
            Some("minimax-key")
        );
    }

    fn tg_config(token: &str, allowed: &[&str]) -> TelegramConfig {
        serde_json::from_value(json!({
            "bot_token": token,
            "allowed_users": allowed.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        }))
        .expect("valid TelegramConfig")
    }

    fn tg_config_mention(token: &str, allowed: &[&str], mention_only: bool) -> TelegramConfig {
        serde_json::from_value(json!({
            "bot_token": token,
            "allowed_users": allowed.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "mention_only": mention_only,
        }))
        .expect("valid TelegramConfig")
    }

    #[test]
    fn apply_update_keeps_token_and_options_on_allowlist_only_change() {
        // Allowlist-only update (no new token): the saved token AND unrelated
        // options (mention_only) must survive; only allowed_users changes.
        let existing =
            tg_config_mention("123456789:AAaa_bb-cc11223344556677889900", &["alice"], true);
        let updated =
            apply_telegram_update(Some(existing), None, vec!["bob".to_string()]).expect("update");
        assert_eq!(
            updated.bot_token,
            "123456789:AAaa_bb-cc11223344556677889900"
        );
        assert!(updated.mention_only, "unrelated options must be preserved");
        assert_eq!(updated.allowed_users, vec!["bob".to_string()]);
    }

    #[test]
    fn apply_update_builds_a_fresh_channel_from_a_new_token() {
        let updated = apply_telegram_update(
            None,
            Some("123456789:BBbb_cc-dd11223344556677889900"),
            vec!["alice".to_string()],
        )
        .expect("new channel");
        assert_eq!(
            updated.bot_token,
            "123456789:BBbb_cc-dd11223344556677889900"
        );
        assert_eq!(updated.allowed_users, vec!["alice".to_string()]);
    }

    #[test]
    fn apply_update_replaces_token_but_preserves_options() {
        let existing = tg_config_mention("111:oldoldoldoldoldoldoldold", &["alice"], true);
        let updated = apply_telegram_update(
            Some(existing),
            Some("222:newnewnewnewnewnewnewnew"),
            vec!["alice".to_string()],
        )
        .expect("replace token");
        assert_eq!(updated.bot_token, "222:newnewnewnewnewnewnewnew");
        assert!(
            updated.mention_only,
            "options preserved when token replaced"
        );
    }

    #[test]
    fn plan_keeps_existing_token_for_allowlist_only_update() {
        // Telegram already configured; caller omits the token → keep the saved
        // one so an operator can edit the allowlist without re-entering it.
        let existing = tg_config("123456789:AAaa_bb-cc11223344556677889900", &["alice"]);
        let plan = plan_telegram_token(Some(&existing), "").expect("keep existing");
        assert!(matches!(plan, TokenPlan::KeepExisting));
    }

    #[test]
    fn plan_requires_token_to_connect_a_new_channel() {
        // No token and nothing configured yet → cannot connect.
        let err = plan_telegram_token(None, "").expect_err("must require a token");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn plan_validates_a_newly_supplied_token() {
        let token = format!("123456789:{}", "A".repeat(35));
        let plan = plan_telegram_token(None, &token).expect("valid new token");
        match plan {
            TokenPlan::Validate(t) => assert_eq!(t, token),
            TokenPlan::KeepExisting => panic!("expected the new token to be validated"),
        }
    }

    #[test]
    fn plan_rejects_a_malformed_new_token() {
        let err = plan_telegram_token(None, "not-a-token").expect_err("bad shape");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn telegram_token_shape_is_enforced() {
        // Real shape: <digits>:<>=20 token chars>.
        assert!(is_valid_telegram_token(&format!(
            "123456789:{}",
            "A".repeat(35)
        )));
        assert!(is_valid_telegram_token("42:AA_bb-cc11223344556677889900"));
        // Rejected: missing colon, non-digit id, short secret, empty.
        assert!(!is_valid_telegram_token("nope"));
        assert!(!is_valid_telegram_token("abc:AAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(!is_valid_telegram_token("123:short"));
        assert!(!is_valid_telegram_token(""));
        // Rejected: URL-significant chars can't reach the interpolated path.
        assert!(!is_valid_telegram_token(
            "123:AAAA/AAAA/../../evilAAAAAAAAA"
        ));
        assert!(!is_valid_telegram_token("123:AAAA?x=1AAAAAAAAAAAAAAAAAAAA"));
        assert!(!is_valid_telegram_token("123:AAAA AAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(!is_valid_telegram_token("123:AAAA@host.comAAAAAAAAAAAAAAA"));
    }

    #[test]
    fn apply_secrets_sets_then_clears_on_empty() {
        let mut cfg = Config::default();
        apply_secrets(
            &mut cfg,
            &SecretsBody {
                api_key: Some("  sk-test  ".into()),
                api_url: Some("https://api.example.com".into()),
            },
        );
        assert_eq!(cfg.api_key.as_deref(), Some("sk-test"));
        assert_eq!(cfg.api_url.as_deref(), Some("https://api.example.com"));
        assert!(api_key_present(&cfg));

        apply_secrets(
            &mut cfg,
            &SecretsBody {
                api_key: Some(String::new()),
                api_url: Some("   ".into()),
            },
        );
        assert!(cfg.api_key.is_none(), "empty key clears the credential");
        assert!(cfg.api_url.is_none(), "blank url clears the override");
        assert!(!api_key_present(&cfg));
    }

    #[test]
    fn apply_secrets_omitted_field_preserves_existing() {
        let mut cfg = Config::default();
        cfg.api_key = Some("keep-me".into());
        apply_secrets(
            &mut cfg,
            &SecretsBody {
                api_key: None,
                api_url: Some("http://override".into()),
            },
        );
        assert_eq!(
            cfg.api_key.as_deref(),
            Some("keep-me"),
            "an omitted api_key must not wipe the existing key"
        );
        assert_eq!(cfg.api_url.as_deref(), Some("http://override"));
    }

    #[test]
    fn secrets_view_never_serializes_the_raw_key() {
        let mut cfg = Config::default();
        cfg.default_provider = Some("openai".into());
        cfg.api_key = Some("super-secret-key".into());
        let view = secrets_view(&cfg);
        assert_eq!(view["provider"], "openai");
        assert_eq!(view["api_key_present"], true);
        assert!(
            !view.to_string().contains("super-secret-key"),
            "GET /secrets must never expose the raw key"
        );
    }
}
