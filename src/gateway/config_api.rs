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
    Router::new()
        .route("/api/v1/config", get(get_config))
        .route("/api/v1/config/model", put(set_model))
        .route("/api/v1/config/autonomy", put(set_autonomy))
        .route("/api/v1/secrets", get(get_secrets).put(set_secrets))
        .route(
            "/api/v1/config/mcp_servers/{name}",
            post(add_mcp_server).delete(remove_mcp_server),
        )
        // Experimental: connect/disconnect a messaging channel from the console.
        .route(
            "/api/v1/channels/telegram",
            post(connect_telegram).delete(disconnect_telegram),
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
    // Channel credentials are secrets too — never return a live bot token over
    // the API (the connect flow already avoids echoing it). Clear before serialising.
    if let Some(tg) = cfg.channels_config.telegram.as_mut() {
        tg.bot_token.clear();
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

// ── POST/DELETE /channels/telegram (experimental connect) ────────────────────

#[derive(Deserialize)]
struct TelegramConnectBody {
    /// Bot API token from @BotFather. Validated live (`getMe`) before persisting.
    bot_token: String,
    /// Telegram user ids/usernames allowed to talk to the bot. Empty = deny all
    /// (the channel stays secure until owners are added).
    #[serde(default)]
    allowed_users: Vec<String>,
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
    let token = body.bot_token.trim().to_string();
    if token.is_empty() {
        return Err(err_400("bot_token must not be empty"));
    }
    // The token is interpolated into the Telegram API URL path
    // (`/bot{token}/getMe`), so enforce the real bot-token shape
    // (`<digits>:<alphanumeric/_-/>`) up front — this both rejects garbage early
    // and prevents URL-significant characters (`/ ? # @` or whitespace) from
    // manipulating the request path.
    if !is_valid_telegram_token(&token) {
        return Err(err_400(
            "bot_token is not a valid Telegram token (expected `<digits>:<token>`)",
        ));
    }

    // Validate the token live BEFORE persisting — fail closed so we never save a
    // credential that doesn't work. Uses a side-effect-free `getMe` probe (not a
    // full TelegramChannel, which would set up pairing + print a code).
    let bot_username = crate::channels::telegram::validate_bot_token(&token)
        .await
        .map_err(|e| {
            // `e` does not contain the token. We can't always tell a bad token
            // from an unreachable Telegram, so the message covers both.
            err_400(format!(
                "could not validate the bot token with Telegram (invalid token, or Telegram unreachable): {e}"
            ))
        })?;

    // Build TelegramConfig via serde so the optional fields inherit their
    // configured defaults (stream mode, draft interval, …) without duplicating
    // them here.
    let tg: TelegramConfig = serde_json::from_value(json!({
        "bot_token": token,
        "allowed_users": body.allowed_users,
    }))
    .map_err(err_500)?;

    let mut cfg = state.config.lock().clone();
    cfg.channels_config.telegram = Some(tg);
    persist_and_swap(&state, cfg).await?;

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
        "experimental": true,
        "warning": warning,
        "note": "Validated + saved. The channel starts receiving messages when the channels runtime (`rantaiclaw channels`) (re)starts.",
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
fn apply_secrets(cfg: &mut crate::config::Config, body: &SecretsBody) {
    if let Some(k) = body.api_key.as_ref() {
        let k = k.trim();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

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
