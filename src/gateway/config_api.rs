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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(feature = "whatsapp-web")]
use axum::response::{
    sse::{Event as SseEvent, KeepAlive, Sse},
    IntoResponse, Response,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post, put},
    Router,
};
use serde::Deserialize;
use serde_json::json;
#[cfg(feature = "whatsapp-web")]
use std::convert::Infallible;

use super::AppState;
use crate::config::api_url::{looks_like_api_key, validate_api_url};
use crate::config::schema::{
    DiscordConfig, LarkConfig, McpServerConfig, SlackConfig, TelegramConfig,
};
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
        // Connect / update (allowlist) / disconnect a channel from the console.
        // One pair per channel rather than one polymorphic route: each carries a
        // different body and a different credential check, and a `{channel}`
        // segment would have to fan back out to exactly this anyway.
        .route(
            "/api/v1/channels/telegram",
            post(connect_telegram).delete(disconnect_telegram),
        )
        .route(
            "/api/v1/channels/discord",
            post(connect_discord).delete(disconnect_discord),
        )
        .route(
            "/api/v1/channels/slack",
            post(connect_slack).delete(disconnect_slack),
        )
        .route(
            "/api/v1/channels/lark",
            post(connect_lark).delete(disconnect_lark),
        )
        // Plan 367: WhatsApp Web can be linked and unlinked from the console.
        // The connect/disconnect routes are always registered; allowlist-only
        // edits apply live through the runtime's existing override. The pair
        // route is gated behind the `whatsapp-web` feature because it shells
        // out to the wa-rs pairing flow, which only compiles with the feature.
        .route(
            "/api/v1/channels/whatsapp_web",
            post(whatsapp_web_connect).delete(whatsapp_web_disconnect),
        );
    #[cfg(feature = "whatsapp-web")]
    {
        router = router.route(
            "/api/v1/channels/whatsapp_web/pair",
            post(whatsapp_web_pair),
        );
    }
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
    // Log the specific cause server-side, but NEVER relay it to the browser. The
    // detail carried by these errors has included the absolute `config.toml`
    // path and the gateway's host:port — disclosing host filesystem layout and
    // internal addressing to any console session. Return a stable, non-specific
    // message; the `error` code lets the console map it to a friendly string.
    tracing::error!("config API internal error: {msg}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": "internal_error",
            "detail": "internal error — check the gateway logs",
        })),
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
    let mut val = serde_json::to_value(&cfg).map_err(err_500)?;
    // Backstop: the typed redactor above only clears a hardcoded set and silently
    // missed every non-Telegram channel credential (Discord/Slack/WhatsApp/…),
    // the gateway login hash, tunnel tokens, and paired tokens. Recursively null
    // ANY field whose key names a secret, so a channel — including one added
    // later — can't leak a credential over this endpoint by omission.
    redact_secrets_in_json(&mut val);
    Ok(Json(val))
}

/// Recursively redact every JSON field whose key names a secret. Runs on the
/// serialized config response as a completeness guarantee behind the typed
/// `redact_config_secrets`.
///
/// Matches by secret-bearing SUFFIX (`_token`, `_secret`, `_password`, `_key`)
/// so credentials across ALL channels — and future ones — are caught by shape:
/// `server_password`/`nickserv_password` (IRC), `encrypt_key`/`verification_token`
/// (Lark), `verify_token` (WhatsApp), etc. The suffixes are chosen NOT to hit
/// non-secret look-alikes: `secrets` (a config section), `max_tokens` and
/// other counts ending in the plural `_tokens`, and
/// `rate_limit_max_keys` / `idempotency_max_keys` (counts ending in `_keys`).
pub(crate) fn redact_secrets_in_json(v: &mut serde_json::Value) {
    fn is_secret_key(k: &str) -> bool {
        let k = k.to_ascii_lowercase();
        k.ends_with("_token")           // bot_token, access_token, verify_token, verification_token, …
            || k == "token"             // bare matrix / tunnel token
            || k == "paired_tokens"     // hashed pairing tokens
            || k.ends_with("_secret")   // app_secret, webhook_secret, signing_secret, client_secret
            || k == "secret"            // webhook.secret
            || k.ends_with("_password") // server_password, nickserv_password, sasl_password
            || k == "password"
            || k == "password_hash"
            || k.ends_with("_key")      // api_key, encrypt_key, private_key, brave_api_key, …
            || k == "api_keys"          // reliability key list
            || k == "provider_api_keys" // per-provider key map
            || k.contains("credential")
            || k == "db_url"
    }
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                if is_secret_key(k) && !child.is_null() {
                    // Preserve the JSON type where possible: a string secret
                    // becomes "" (not null) so the console, if it types the field
                    // as a non-nullable string, doesn't break on the redacted
                    // response. Key maps/lists become null.
                    *child = if child.is_string() {
                        serde_json::Value::String(String::new())
                    } else {
                        serde_json::Value::Null
                    };
                } else {
                    redact_secrets_in_json(child);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                redact_secrets_in_json(item);
            }
        }
        _ => {}
    }
}

/// Clear every secret field before a Config is serialized into an API response.
/// Keep in sync with the encrypt/decrypt lists in config::schema.
pub(crate) fn redact_config_secrets(cfg: &mut crate::config::Config) {
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
    // Skill literal API keys are encrypted at rest like `api_key` too (plan
    // 045); redact them so a `source = "literal"` value never leaves the
    // config API even after it's decrypted into memory.
    for entry in cfg.skills.entries.values_mut() {
        if let Some(api_key) = entry.api_key.as_mut() {
            api_key.value = None;
        }
    }
    // `api_url` can carry a credential (a pasted key, or a `user:pass@` / `?key=`
    // URL). `secrets_view` already withholds such a value; `get_config` must apply
    // the same policy so the two endpoints don't disagree.
    cfg.api_url = cfg.api_url.as_deref().and_then(sanitize_api_url);
    // MCP servers are launched like `npx -y <server> --api-key <token>`; the
    // key-suffix JSON walk can't see arg values (no keys) or operator-named env
    // vars (`DATABASE_URL`, `PGPASSWORD`). Blank every env value, arg values that
    // follow a credential-shaped flag or look like a key, and a key-shaped command.
    for server in cfg.mcp_servers.values_mut() {
        for value in server.env.values_mut() {
            value.clear();
        }
        redact_mcp_args(&mut server.args);
        if looks_like_api_key(&server.command) {
            server.command.clear();
        }
    }
    // Corporate proxies are conventionally `http://user:password@host:port`; strip
    // the userinfo so the credential never leaves the gateway while the host/port
    // stays visible for the operator to see which proxy is configured.
    for proxy in [
        &mut cfg.proxy.http_proxy,
        &mut cfg.proxy.https_proxy,
        &mut cfg.proxy.all_proxy,
    ] {
        if let Some(url) = proxy.as_deref() {
            *proxy = Some(strip_url_userinfo(url));
        }
    }
}

/// Blank arg values that carry a credential: any arg that looks like a key, and
/// the token following a credential-shaped flag (`--api-key`, `--token`, …).
fn redact_mcp_args(args: &mut [String]) {
    let mut mask_next = false;
    for arg in args.iter_mut() {
        if mask_next {
            arg.clear();
            mask_next = false;
            continue;
        }
        if looks_like_api_key(arg) {
            arg.clear();
            continue;
        }
        if arg.starts_with("--") {
            let flag = arg.to_ascii_lowercase();
            let secret_flag = ["key", "token", "secret", "password"]
                .iter()
                .any(|k| flag.contains(k));
            // `--api-key=VALUE` carries the secret inline; `--api-key VALUE`
            // carries it in the next arg.
            if secret_flag {
                if let Some((name, _value)) = arg.split_once('=') {
                    let name = name.to_string();
                    *arg = format!("{name}=");
                } else {
                    mask_next = true;
                }
            }
        }
    }
}

/// Strip the `user:pass@` userinfo component from a URL, leaving host/port. Not a
/// full URL parse — a best-effort scrub that never widens what's returned.
fn strip_url_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    match rest.split_once('@') {
        Some((_userinfo, host_and_path)) => format!("{scheme}://{host_and_path}"),
        None => url.to_string(),
    }
}

/// Non-secret form of `api_url`: `None` when it holds a credential, otherwise the
/// URL with any `user:pass@` userinfo and `key=`/`api_key=`/`access_token=` query
/// parameter stripped. Shared by `get_config` redaction and `secrets_view`.
fn sanitize_api_url(value: &str) -> Option<String> {
    if looks_like_api_key(value) {
        return None;
    }
    let without_userinfo = strip_url_userinfo(value);
    let cleaned = match without_userinfo.split_once('?') {
        Some((base, query)) => {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|param| {
                    let name = param.split_once('=').map(|(n, _)| n).unwrap_or(param);
                    !matches!(
                        name.to_ascii_lowercase().as_str(),
                        "key" | "api_key" | "access_token"
                    )
                })
                .collect();
            if kept.is_empty() {
                base.to_string()
            } else {
                format!("{base}?{}", kept.join("&"))
            }
        }
        None => without_userinfo,
    };
    Some(cleaned)
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

/// Serializes config-file mutations made through the gateway config API behind a
/// process-global lock. `config.toml` is a single shared resource: two concurrent
/// gateway writers that each cloned the in-memory config, changed one field, and
/// saved the whole file would clobber each other's fields — this lock plus the
/// per-write fresh disk read (`lock_and_load`) prevents that.
///
/// SCOPE (do not overstate): this covers the config_api writers only. The
/// out-of-band writers — a Telegram `/claim` (`persist_approval_owner`) and
/// client pairing (`persist_pairing_tokens`) — do NOT take this lock. The
/// per-write fresh disk read shrinks the window in which one of those could be
/// clobbered by an interleaving gateway write from "the whole gateway uptime"
/// (the pre-lock bug) down to the few milliseconds of a single read-modify-write,
/// but does not fully eliminate it. Making it airtight would require those paths
/// to share this lock (they live in other modules — a follow-up).
static CONFIG_WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Take the config write lock and return the FRESHEST config from disk (not the
/// possibly-stale in-memory `state.config`), so a write applies its field delta
/// onto on-disk truth. The caller MUST keep the returned guard in scope across
/// the subsequent [`persist_and_swap`] for the read-modify-write to be atomic.
async fn lock_and_load(
    state: &AppState,
) -> Result<(tokio::sync::MutexGuard<'static, ()>, crate::config::Config), ApiError> {
    let guard = CONFIG_WRITE_LOCK.lock().await;
    let running = state.config.lock().clone();
    // No file at the path the gateway booted with — it was deleted under the
    // running process, or this gateway was constructed from a config that was
    // never written. Fall back to what it is actually serving rather than
    // re-resolving to whatever path the environment names now; that
    // re-resolution is the defect this function exists to remove. A file that
    // exists but fails to load still errors, so a corrupt config is not
    // silently replaced by a stale copy.
    let cfg = if running.config_path.exists() {
        load_running_config(&running.config_path)
            .await
            .map_err(err_500)?
    } else {
        running
    };
    Ok((guard, cfg))
}

/// Read the config the gateway is RUNNING, from the path it booted with.
///
/// Deliberately not `Config::load_or_init`: that re-resolves the path from the
/// environment and the `active_workspace.toml` marker every time. The gateway
/// is already bound to one file, so a marker that changed after boot made a
/// console write read-modify-save a *different* file and swap it into the
/// running state. The pairing-token writer already loads by path; this did not.
///
/// Env overrides are applied because `load_or_init` applies them and the
/// running config was built that way. Without this, the first console write
/// would drop an env-supplied credential out of the running process — a
/// regression worse than the split-brain being fixed. (`save()` strips them
/// again on the way to disk, so they never become permanent.)
async fn load_running_config(
    config_path: &std::path::Path,
) -> anyhow::Result<crate::config::Config> {
    let mut cfg = crate::config::Config::load_from_path(config_path).await?;
    cfg.apply_env_overrides();
    Ok(cfg)
}

/// Persist the mutated config, then swap it into the running state. Must be
/// called while holding the [`lock_and_load`] guard so a concurrent writer can't
/// interleave between the disk read and this save.
async fn persist_and_swap(
    state: &AppState,
    cfg: crate::config::Config,
    change_summary: &str,
) -> Result<(), ApiError> {
    // Reject a config the loader would refuse, at the write boundary — otherwise a
    // console write can persist a state that then bricks the next startup (e.g.
    // autonomy.max_actions_per_hour = 0) or an out-of-range temperature that 400s
    // every provider call, both far from the request that caused it.
    cfg.validate().map_err(|e| err_400(e.to_string()))?;
    cfg.save().await.map_err(err_500)?;
    // Stamp the fingerprint of what we just wrote BEFORE the file watcher fires,
    // so the reloader's fingerprint-gate self-suppresses this write's event (no
    // redundant second load) and `GET /version` reports the new fingerprint
    // immediately instead of ~500ms stale.
    *state.config_fingerprint.lock() =
        crate::config::fingerprint::fingerprint_file(&cfg.config_path);
    audit_config_change(&cfg, change_summary);
    *state.config.lock() = cfg;
    Ok(())
}

/// Record a config-API mutation to the audit log so a policy-weakening change over
/// HTTP leaves a trail (who, when, which section). Field/section NAMES only — never
/// values — so no secret can leak. Best-effort; the blocking append runs off the
/// async worker.
///
/// `SecurityConfig` (which owns the operator-facing `[security.audit]` block) is not
/// wired into `Config` today, so there is no reachable per-deployment audit config to
/// read; config-change auditing therefore uses `AuditConfig::default()` (enabled). If
/// a future change threads `SecurityConfig` into `Config`, source the config here.
fn audit_config_change(cfg: &crate::config::Config, change_summary: &str) {
    let Some(dir) = cfg.config_path.parent().map(std::path::Path::to_path_buf) else {
        return;
    };
    let audit_cfg = crate::config::AuditConfig::default();
    let event = config_change_event(change_summary);
    tokio::task::spawn_blocking(move || {
        if let Ok(logger) = crate::security::AuditLogger::new(audit_cfg, dir) {
            if let Err(e) = logger.log(&event) {
                tracing::warn!(target: "gateway", error = %e, "failed to write config-change audit record");
            }
        }
    });
}

/// Build the `ConfigChange` audit record for a config-API mutation. The actor is
/// the anonymous console principal (pairing tokens are hashed, not individually
/// labelled) and the "command" is the changed section NAME — never a value.
fn config_change_event(change_summary: &str) -> crate::security::AuditEvent {
    crate::security::AuditEvent::new(crate::security::AuditEventType::ConfigChange)
        .with_actor("web-console".to_string(), None, None)
        .with_action(
            change_summary.to_string(),
            "config".to_string(),
            // The console principal is paired, not prompted: no approval dialog
            // stands between a paired caller and a config write.
            crate::security::ApprovalOutcome::NotRequired,
            true,
        )
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
    // Ask the same question the send path asks, with the same inputs: the
    // per-provider key (not just the top-level one) and the directory holding
    // this install's auth profiles.
    if crate::providers::has_usable_credential(
        provider,
        cfg.resolve_key_for_provider(provider).as_deref(),
        Some(&crate::auth::state_dir_from_config(cfg)),
    ) {
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
    let (_guard, mut cfg) = lock_and_load(&state).await?;
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
    persist_and_swap(&state, cfg, "model").await?;
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
    /// Accepted and ignored for one release.
    ///
    /// The key it wrote (`autonomy.max_cost_per_day_cents`) is gone: it was a
    /// money ceiling that was never enforced, replaced by `[cost]
    /// max_tokens_per_day`, which is. A console built against the old shape
    /// still sends this field, and rejecting the whole request over it would
    /// break every other autonomy edit in the same PATCH. Remove after the
    /// console has shipped its side.
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
    let (_guard, mut cfg) = lock_and_load(&state).await?;
    if let Some(l) = body.level {
        cfg.autonomy.level =
            parse_level(&l).ok_or_else(|| err_400(format!("invalid autonomy level: {l}")))?;
    }
    if let Some(v) = body.auto_approve {
        cfg.autonomy.auto_approve = validate_tool_entries(v)?;
    }
    if let Some(v) = body.always_ask {
        cfg.autonomy.always_ask = validate_tool_entries(v)?;
    }
    if let Some(v) = body.allowed_commands {
        // Validate each entry into a single basename (the shell gate matches by
        // basename), rejecting multi-token/glob values that would silently never
        // match. The high-risk warning is advisory and dropped here.
        let mut cleaned = Vec::with_capacity(v.len());
        for entry in &v {
            match crate::approval::permissions::validate_allow_basename(entry) {
                Ok((base, _)) => cleaned.push(base),
                Err(msg) => return Err(err_400(msg)),
            }
        }
        cfg.autonomy.allowed_commands = cleaned;
    }
    if let Some(v) = body.forbidden_paths {
        cfg.autonomy.forbidden_paths = v;
    }
    if let Some(v) = body.max_actions_per_hour {
        cfg.autonomy.max_actions_per_hour = v;
    }
    if body.max_cost_per_day_cents.is_some() {
        tracing::info!(
            "ignoring max_cost_per_day_cents: the money ceiling was never enforced and is \
             replaced by [cost] max_tokens_per_day"
        );
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
    persist_and_swap(&state, cfg, "autonomy").await?;
    // A tightening must revoke prior "Always" grants — otherwise a blanket grant
    // made under a looser preset is re-seeded into the next turn's manager and
    // keeps skipping the prompt.
    crate::gateway::web_approval::clear_all_session_grants();
    Ok(Json(resp))
}

// ── POST/DELETE /config/mcp_servers/{name} ───────────────────────────────────

/// Max MCP servers accepted over the config API. Mirrors `MAX_MCP_SERVERS` in
/// `src/mcp/mod.rs` (the runtime registry cap) so the write boundary can't grow
/// the config past what the registry will actually run.
const MAX_MCP_SERVERS_API: usize = 10;

/// `env`/`args` are `Option` so an omitted field means "keep the existing value"
/// (matching the `/secrets` and `/config/knowledge` write contract) — re-adding a
/// server to fix a typo must not wipe its stored env/args.
#[derive(Deserialize)]
struct McpServerBody {
    command: String,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
}

/// Reject a command that is a shell expression rather than a program to spawn:
/// this route persists a command the agent later runs via `Command::new`, so a
/// value like `sh -c 'curl … | sh'` must not be smuggled through.
fn validate_mcp_command(command: &str) -> Result<(), ApiError> {
    if command
        .chars()
        .any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '<' | '>' | '\n' | '\r'))
    {
        return Err(err_400(
            "MCP command must be a program name or path, not a shell expression",
        ));
    }
    Ok(())
}

/// Env vars that steer the dynamic loader turn an MCP spawn into arbitrary code
/// execution, so they must never be injectable through a config write.
fn is_loader_env_key(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    k == "LD_PRELOAD" || k == "LD_LIBRARY_PATH" || k.starts_with("DYLD_")
}

/// Validate autonomy tool-list entries. A malformed entry (empty, whitespace, or
/// multiple tokens) can never match the gate's exact-string comparison, so it
/// would silently fail open while being echoed back as if enforced — reject it.
/// A well-formed but unknown name can't be caught without the full runtime tool
/// registry (a known gap; the `"*"` wildcard is the safe catch-all). Entries are
/// trimmed so a stray-space value doesn't quietly never match.
fn validate_tool_entries(entries: Vec<String>) -> Result<Vec<String>, ApiError> {
    let mut cleaned = Vec::with_capacity(entries.len());
    for entry in entries {
        let t = entry.trim();
        if t.is_empty() {
            return Err(err_400("tool name must not be empty"));
        }
        if t.split_whitespace().count() != 1 {
            return Err(err_400(format!(
                "tool name '{entry}' must be a single token (or '*')"
            )));
        }
        cleaned.push(t.to_string());
    }
    Ok(cleaned)
}

/// Merge an MCP write onto an existing entry: the command is required, but an
/// omitted (`None`) args/env keeps the existing value rather than clearing it.
fn merge_mcp_server(
    existing: Option<&McpServerConfig>,
    command: String,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
) -> McpServerConfig {
    McpServerConfig {
        command,
        args: args
            .or_else(|| existing.map(|e| e.args.clone()))
            .unwrap_or_default(),
        env: env
            .or_else(|| existing.map(|e| e.env.clone()))
            .unwrap_or_default(),
    }
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
    let command = body.command.trim().to_string();
    if command.is_empty() {
        return Err(err_400("command must not be empty"));
    }
    validate_mcp_command(&command)?;
    if let Some(env) = body.env.as_ref() {
        if let Some(bad) = env.keys().find(|k| is_loader_env_key(k)) {
            return Err(err_400(format!(
                "MCP env var '{bad}' is not allowed (influences the dynamic loader)"
            )));
        }
    }
    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let existing = cfg.mcp_servers.get(&name).cloned();
    if existing.is_none() && cfg.mcp_servers.len() >= MAX_MCP_SERVERS_API {
        return Err(err_400(format!(
            "too many MCP servers (max {MAX_MCP_SERVERS_API})"
        )));
    }
    let merged = merge_mcp_server(existing.as_ref(), command, body.args, body.env);
    cfg.mcp_servers.insert(name.clone(), merged);
    let count = cfg.mcp_servers.len();
    persist_and_swap(&state, cfg, "mcp_servers").await?;
    Ok(Json(json!({ "name": name, "added": true, "count": count })))
}

async fn remove_mcp_server(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let removed = cfg.mcp_servers.remove(&name).is_some();
    let count = cfg.mcp_servers.len();
    persist_and_swap(&state, cfg, "mcp_servers").await?;
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
        // Plan 370: the in-daemon variant. The blocking form would shell out
        // to `systemctl restart` and wait for the unit's stop job, which
        // sends SIGTERM to this very process; the runtime drop then waits for
        // the spawned task and the whole thing deadlocks until
        // `TimeoutStopSec` SIGKILLs the daemon. The non-blocking variant
        // queues the job and returns at once, so the response can flush
        // before the service manager replaces us.
        match tokio::task::spawn_blocking(crate::channels::reload_managed_daemon_non_blocking).await
        {
            Ok(Ok(true)) => {
                tracing::info!(target: "gateway", "channel change: requested managed daemon service restart");
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
                tracing::warn!(target: "gateway", "managed daemon reload task failed to join: {e}");
            }
        }
    });
}

/// Whether a `POST /channels/telegram` needs the channels runtime restarted.
///
/// Only a new token does. It creates or replaces the channel itself, which the
/// running runtime cannot swap in place.
///
/// An allowlist-only edit must NOT restart: the runtime re-reads config on its
/// next message and pushes the new list into the live channel through
/// `Channel::apply_allowed_senders`. Restarting would be actively harmful — the
/// daemon hosts this gateway, so it kills the request that made the edit, and
/// with no debounce repeated saves can trip systemd's start limit and leave the
/// unit `failed`.
///
/// Extracted as a pure function so the decision is testable without observing
/// the detached restart task.
fn needs_runtime_restart(token_changed: bool) -> bool {
    token_changed
}

/// Whether a Discord save needs the channels runtime restarted.
///
/// A credential change does, and so does a `guild_id` change, because the guild
/// filter is passed into the channel object at construction and cannot be
/// swapped in place.
///
/// What must NOT restart is a request that merely *carries* a field whose value
/// is unchanged. The daemon hosts this gateway, so every restart kills the
/// request that asked for it, and with no debounce a handful of no-op saves
/// trips systemd's start limit and leaves the unit `failed`. Presence in the
/// body is not a change; this compares against the config just loaded.
fn discord_restart_needed(
    token_changed: bool,
    before: Option<&DiscordConfig>,
    after: &DiscordConfig,
) -> bool {
    let guild_changed = before.and_then(|b| b.guild_id.clone()) != after.guild_id;
    needs_runtime_restart(token_changed) || guild_changed
}

/// The Slack counterpart. The app token decides the transport itself (Socket
/// Mode or polling) and `channel_id` is read at construction, so a change to
/// either needs the listener rebuilt — but only a real change.
fn slack_restart_needed(
    token_changed: bool,
    before: Option<&SlackConfig>,
    after: &SlackConfig,
) -> bool {
    let app_token_changed = before.and_then(|b| b.app_token.clone()) != after.app_token;
    let channel_changed = before.and_then(|b| b.channel_id.clone()) != after.channel_id;
    needs_runtime_restart(token_changed) || app_token_changed || channel_changed
}

/// The operator-facing note for a save, derived from the same flag the response
/// reports as `restarts_runtime`.
///
/// One function so the two cannot drift. They already had: the console printed
/// its own hint saying every save reloads the runtime, directly above this
/// reply saying it does not — both on screen at once, about the same click.
fn runtime_restart_note(restarts_runtime: bool) -> &'static str {
    if restarts_runtime {
        "Saved. Reloading the runtime to apply the new token — automatic if RantaiClaw runs as a managed service, otherwise restart `rantaiclaw daemon`."
    } else {
        "Saved. The running channel picks this up on its next message — no restart."
    }
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

    // Serialize + apply onto the freshest on-disk config so a concurrent
    // out-of-band write (e.g. a Telegram `/claim` persisting an approval owner)
    // can't be clobbered. Build on the fresh Telegram config, not the snapshot
    // taken before the `getMe` await.
    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let tg = apply_telegram_update(
        cfg.channels_config.telegram.clone(),
        new_token.as_deref(),
        body.allowed_users.clone(),
    )?;
    cfg.channels_config.telegram = Some(tg);
    persist_and_swap(&state, cfg, "channels.telegram").await?;

    // A new token creates or replaces the channel itself, which the running
    // runtime cannot swap in place — that still needs a restart.
    //
    // An allowlist-only edit does NOT. The channels runtime re-reads config on
    // its next message and pushes the new list into the live channel via
    // `Channel::apply_allowed_senders`, so restarting here would be pure cost:
    // the daemon hosts this gateway, so it would kill the request that made the
    // edit — and with no debounce, repeated saves could trip systemd's start
    // limit and leave the unit `failed`.
    let restarts_runtime = needs_runtime_restart(new_token.is_some());
    if restarts_runtime {
        schedule_daemon_reload();
    }

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
        // The same decision as `note`, as data. A console cannot reliably tell
        // "the runtime is restarting" from prose, and it needs to: showing a
        // reload banner for a save that does not restart leaves the operator
        // watching for something that never happens.
        "restarts_runtime": restarts_runtime,
        "note": runtime_restart_note(restarts_runtime),
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
    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let was_configured = cfg.channels_config.telegram.is_some();
    cfg.channels_config.telegram = None;
    persist_and_swap(&state, cfg, "channels.telegram").await?;

    // Only bounce the runtime if we actually removed a running channel.
    if was_configured {
        schedule_daemon_reload();
    }

    // Same field name as the connect/allowlist responses, so a client has one
    // thing to read rather than having to know that `disconnected` implies a
    // bounce here but `allowed_users` does not there.
    Ok(Json(json!({
        "disconnected": was_configured,
        "channel": "telegram",
        "restarts_runtime": was_configured,
    })))
}

// ── POST/DELETE /channels/discord and /channels/slack ───────────────────────

#[derive(Deserialize)]
struct DiscordConnectBody {
    /// Bot token from the Discord developer portal. Validated live before
    /// persisting. Optional: omit to edit `allowed_users` on an already
    /// connected channel without re-entering it.
    #[serde(default)]
    bot_token: String,
    /// Discord user ids allowed to talk to the bot. Empty = deny all.
    #[serde(default)]
    allowed_users: Vec<String>,
    /// Optional guild filter. Absent leaves the saved value untouched.
    #[serde(default)]
    guild_id: Option<String>,
}

#[derive(Deserialize)]
struct SlackConnectBody {
    /// Bot token (`xoxb-…`). Validated live before persisting. Optional, as for
    /// Discord.
    #[serde(default)]
    bot_token: String,
    /// App-level token (`xapp-…`) that turns on Socket Mode. Absent leaves the
    /// saved value untouched.
    #[serde(default)]
    app_token: Option<String>,
    /// Slack user ids allowed to talk to the bot. Empty = deny all.
    #[serde(default)]
    allowed_users: Vec<String>,
    /// Optional conversation filter. Absent leaves the saved value untouched.
    #[serde(default)]
    channel_id: Option<String>,
}

/// The Discord counterpart of [`plan_telegram_token`].
///
/// No shape check: a Discord bot token has no documented grammar, and the token
/// never reaches a URL path (it travels in an `Authorization` header), so the
/// reason Telegram's shape check exists does not apply. The live probe is the
/// check.
fn plan_discord_token(
    existing: Option<&DiscordConfig>,
    provided: &str,
) -> Result<TokenPlan, ApiError> {
    plan_channel_token(existing.is_some(), provided, "Discord")
}

/// The Slack counterpart. Same reasoning as Discord's: `xoxb-` is a convention,
/// not a grammar, and the token travels in a header.
fn plan_slack_token(existing: Option<&SlackConfig>, provided: &str) -> Result<TokenPlan, ApiError> {
    plan_channel_token(existing.is_some(), provided, "Slack")
}

/// Shared by both planners: a supplied token is validated live, an omitted one
/// keeps the saved credential, and an omitted one with nothing saved is refused
/// rather than written as an empty credential.
fn plan_channel_token(
    has_existing: bool,
    provided: &str,
    channel: &str,
) -> Result<TokenPlan, ApiError> {
    let token = provided.trim();
    if token.is_empty() {
        return if has_existing {
            Ok(TokenPlan::KeepExisting)
        } else {
            Err(err_400(format!(
                "bot_token is required to connect a new {channel} channel"
            )))
        };
    }
    Ok(TokenPlan::Validate(token.to_string()))
}

/// Whether `token` looks like a Slack app-level token.
///
/// The console refuses a malformed one rather than warning, because it can ask
/// again; the CLI wizard keeps warn-and-save, since abandoning a headless setup
/// over a typo is worse there. Neither probes it: `apps.connections.open` is the
/// only real check and it opens a live socket, so `doctor` owns that verdict.
fn is_valid_slack_app_token(token: &str) -> bool {
    let token = token.trim();
    token.starts_with(crate::channels::slack::APP_TOKEN_PREFIX) && token.len() > 8
}

/// The caveat F-3 recorded on 2026-09-11: under Socket Mode, a `channel_id`
/// filter drops direct messages, because a DM does not arrive on that channel.
///
/// Only when both are set. With no app token there is no Socket Mode, and with
/// no channel filter nothing is dropped.
fn socket_mode_dm_caveat(
    app_token: Option<&str>,
    channel_id: Option<&str>,
) -> Option<&'static str> {
    let filtering = channel_id.is_some_and(|c| !c.trim().is_empty());
    let socket_mode = app_token.is_some_and(|t| !t.trim().is_empty());
    (socket_mode && filtering).then_some(
        "Socket Mode is on and channel_id is set: the bot will ignore direct messages and every \
         conversation except that one. Clear channel_id to accept all of them.",
    )
}

/// Build the `DiscordConfig` to persist from the existing one plus this
/// request's changes, preserving every option the request does not mention.
fn apply_discord_update(
    existing: Option<DiscordConfig>,
    new_token: Option<&str>,
    allowed_users: Vec<String>,
    guild_id: Option<&str>,
) -> Result<DiscordConfig, ApiError> {
    let mut dc = match existing {
        Some(dc) => dc,
        None => serde_json::from_value(json!({ "bot_token": "", "allowed_users": [] }))
            .map_err(err_500)?,
    };
    if let Some(token) = new_token {
        dc.bot_token = token.to_string();
    }
    dc.allowed_users = allowed_users;
    if let Some(guild) = guild_id {
        let guild = guild.trim();
        dc.guild_id = (!guild.is_empty()).then(|| guild.to_string());
    }
    Ok(dc)
}

/// The Slack counterpart, which also enforces the app-token shape: a malformed
/// one is a 400 here rather than a warning.
fn apply_slack_update(
    existing: Option<SlackConfig>,
    new_token: Option<&str>,
    allowed_users: Vec<String>,
    app_token: Option<&str>,
    channel_id: Option<&str>,
) -> Result<SlackConfig, ApiError> {
    let mut sc = match existing {
        Some(sc) => sc,
        None => serde_json::from_value(json!({ "bot_token": "", "allowed_users": [] }))
            .map_err(err_500)?,
    };
    if let Some(token) = new_token {
        sc.bot_token = token.to_string();
    }
    sc.allowed_users = allowed_users;
    if let Some(app) = app_token {
        let app = app.trim();
        if app.is_empty() {
            sc.app_token = None;
        } else if is_valid_slack_app_token(app) {
            sc.app_token = Some(app.to_string());
        } else {
            return Err(err_400(format!(
                "app_token is not a Slack app-level token (expected `{}…`)",
                crate::channels::slack::APP_TOKEN_PREFIX
            )));
        }
    }
    if let Some(channel) = channel_id {
        let channel = channel.trim();
        sc.channel_id = (!channel.is_empty()).then(|| channel.to_string());
    }
    Ok(sc)
}

/// The connect response for Discord. Carries no credential: the identity comes
/// from the probe, and the allowlist is reported as a count.
fn discord_connect_response(
    bot_identity: Option<&str>,
    allowed_users: usize,
    warning: Option<&str>,
    restarts_runtime: bool,
) -> serde_json::Value {
    json!({
        "connected": true,
        "channel": "discord",
        "bot_username": bot_identity,
        "allowed_users": allowed_users,
        "warning": warning,
        "restarts_runtime": restarts_runtime,
        "note": runtime_restart_note(restarts_runtime),
    })
}

/// The Slack counterpart, same shape so a console reads one thing.
fn slack_connect_response(
    bot_identity: Option<&str>,
    allowed_users: usize,
    warning: Option<&str>,
    restarts_runtime: bool,
) -> serde_json::Value {
    json!({
        "connected": true,
        "channel": "slack",
        "bot_username": bot_identity,
        "allowed_users": allowed_users,
        "warning": warning,
        "restarts_runtime": restarts_runtime,
        "note": runtime_restart_note(restarts_runtime),
    })
}

/// Turn a probe verdict into either the identity to report or a 400.
///
/// Both `Rejected` and `Inconclusive` refuse. Rule 2 of this effort says a
/// credential that cannot be validated is not stored, and the gateway already
/// fails closed for Telegram. The two messages differ so an offline operator is
/// not told their token is bad.
fn refuse_unless_accepted(
    verdict: crate::onboard::provision::validate::verdict::ProbeVerdict,
    channel: &str,
) -> Result<(), ApiError> {
    use crate::onboard::provision::validate::verdict::ProbeVerdict;
    match verdict {
        ProbeVerdict::Accepted => Ok(()),
        ProbeVerdict::Rejected(detail) => Err(err_400(format!(
            "{channel} rejected the bot token: {detail}"
        ))),
        ProbeVerdict::Inconclusive(detail) => Err(err_400(format!(
            "could not check the bot token with {channel} (it was not rejected, but nothing \
             confirmed it either): {detail}"
        ))),
    }
}

/// The allowlist warning both channels share with Telegram.
fn allowlist_warning(allowed_users: &[String], subject: &str) -> Option<String> {
    if allowed_users.is_empty() {
        Some(format!(
            "allowed_users is empty — the bot will deny ALL senders until you add {subject} user ids."
        ))
    } else if allowed_users.iter().any(|u| u.trim() == "*") {
        Some(format!(
            "allowed_users contains \"*\" — the bot will respond to ANYONE. Use specific {subject} user ids unless this is intentional."
        ))
    } else {
        None
    }
}

async fn connect_discord(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DiscordConnectBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;

    let existing = state.config.lock().channels_config.discord.clone();
    let plan = plan_discord_token(existing.as_ref(), &body.bot_token)?;

    let new_token = match plan {
        TokenPlan::Validate(token) => {
            let verdict = crate::channels::discord::validate_bot_token(&token).await;
            refuse_unless_accepted(verdict, "Discord")?;
            Some(token)
        }
        TokenPlan::KeepExisting => None,
    };

    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let previous = cfg.channels_config.discord.clone();
    let dc = apply_discord_update(
        previous.clone(),
        new_token.as_deref(),
        body.allowed_users.clone(),
        body.guild_id.as_deref(),
    )?;
    // Decided against the freshly loaded config, before the new one replaces it,
    // so resending an unchanged `guild_id` does not bounce the daemon.
    let restarts_runtime = discord_restart_needed(new_token.is_some(), previous.as_ref(), &dc);
    cfg.channels_config.discord = Some(dc);
    persist_and_swap(&state, cfg, "channels.discord").await?;

    if restarts_runtime {
        schedule_daemon_reload();
    }

    let warning = allowlist_warning(&body.allowed_users, "Discord");
    Ok(Json(discord_connect_response(
        None,
        body.allowed_users.len(),
        warning.as_deref(),
        restarts_runtime,
    )))
}

async fn disconnect_discord(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let was_configured = cfg.channels_config.discord.is_some();
    cfg.channels_config.discord = None;
    persist_and_swap(&state, cfg, "channels.discord").await?;
    if was_configured {
        schedule_daemon_reload();
    }
    Ok(Json(json!({
        "disconnected": was_configured,
        "channel": "discord",
        "restarts_runtime": was_configured,
    })))
}

async fn connect_slack(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SlackConnectBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;

    let existing = state.config.lock().channels_config.slack.clone();
    let plan = plan_slack_token(existing.as_ref(), &body.bot_token)?;

    let new_token = match plan {
        TokenPlan::Validate(token) => {
            let verdict = crate::channels::slack::validate_bot_token(&token).await;
            refuse_unless_accepted(verdict, "Slack")?;
            Some(token)
        }
        TokenPlan::KeepExisting => None,
    };

    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let previous = cfg.channels_config.slack.clone();
    let sc = apply_slack_update(
        previous.clone(),
        new_token.as_deref(),
        body.allowed_users.clone(),
        body.app_token.as_deref(),
        body.channel_id.as_deref(),
    )?;
    let effective_app_token = sc.app_token.clone();
    let effective_channel_id = sc.channel_id.clone();
    // Same rule as Discord's: a real change to the transport or the filter, not
    // the mere presence of the field in the body.
    let restarts_runtime = slack_restart_needed(new_token.is_some(), previous.as_ref(), &sc);
    cfg.channels_config.slack = Some(sc);
    persist_and_swap(&state, cfg, "channels.slack").await?;

    if restarts_runtime {
        schedule_daemon_reload();
    }

    // The DM caveat outranks the allowlist warning: it describes messages the
    // operator will never see, which is the more surprising of the two.
    let caveat = socket_mode_dm_caveat(
        effective_app_token.as_deref(),
        effective_channel_id.as_deref(),
    )
    .map(str::to_string)
    .or_else(|| allowlist_warning(&body.allowed_users, "Slack"));
    Ok(Json(slack_connect_response(
        None,
        body.allowed_users.len(),
        caveat.as_deref(),
        restarts_runtime,
    )))
}

async fn disconnect_slack(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let was_configured = cfg.channels_config.slack.is_some();
    cfg.channels_config.slack = None;
    persist_and_swap(&state, cfg, "channels.slack").await?;
    if was_configured {
        schedule_daemon_reload();
    }
    Ok(Json(json!({
        "disconnected": was_configured,
        "channel": "slack",
        "restarts_runtime": was_configured,
    })))
}

// ── POST/DELETE /channels/lark ───────────────────────────────────────────────

#[derive(Deserialize)]
struct LarkConnectBody {
    /// App ID from the Lark/Feishu developer console. Optional: omit (with
    /// `app_secret`) to edit the allowlist or region on an already connected
    /// channel without re-entering the credential pair.
    #[serde(default)]
    app_id: String,
    /// App Secret. Validated live (via the tenant-access-token endpoint)
    /// before persisting. Must be supplied together with `app_id`.
    #[serde(default)]
    app_secret: String,
    /// Encrypt key for webhook event-body decryption. Absent leaves the saved
    /// value untouched; present (even empty) sets or clears it. This build
    /// does not decrypt event bodies, so a non-empty key here means Lark
    /// refuses to start until it is cleared — the connect response warns.
    #[serde(default)]
    encrypt_key: Option<String>,
    /// Verification token for webhook validation. Same absent/present
    /// convention as `encrypt_key`.
    #[serde(default)]
    verification_token: Option<String>,
    /// Lark/Feishu user or union IDs allowed to talk to the bot. Empty = deny
    /// all, "*" = allow all. Always replaces the saved list, same convention
    /// as every other channel here.
    #[serde(default)]
    allowed_users: Vec<String>,
    /// Region: `true` selects Feishu (CN), `false`/omitted selects Lark
    /// International (D-2's default). `None` (omitted from the request body)
    /// keeps the saved region so an allowlist-only edit cannot silently flip
    /// it back to the default.
    #[serde(default)]
    use_feishu: Option<bool>,
}

/// How a `POST /channels/lark` request treats the credential pair.
///
/// Unlike Discord/Slack/Telegram's single bot token, Lark's credential is two
/// fields that must travel together: a probe needs both to mint a tenant
/// access token, and persisting one without the other can never be valid.
#[derive(Debug)]
enum LarkCredentialPlan {
    /// A new, non-empty pair was supplied — the caller must live-validate it
    /// against Lark/Feishu before persisting.
    Validate { app_id: String, app_secret: String },
    /// Both fields were omitted but a channel is already configured — keep
    /// the saved pair so an operator can edit the allowlist or region without
    /// re-entering credentials.
    KeepExisting,
}

/// Decide how to treat the credential pair on a `POST /channels/lark`
/// request. Mirrors [`plan_channel_token`]'s three cases, adapted for a
/// two-field credential: both empty keeps the saved pair (or refuses a fresh
/// connect), one empty and the other not is refused as malformed, both
/// present is a fresh pair to validate.
fn plan_lark_credentials(
    existing: Option<&LarkConfig>,
    app_id: &str,
    app_secret: &str,
) -> Result<LarkCredentialPlan, ApiError> {
    let app_id = app_id.trim();
    let app_secret = app_secret.trim();
    if app_id.is_empty() && app_secret.is_empty() {
        return if existing.is_some() {
            Ok(LarkCredentialPlan::KeepExisting)
        } else {
            Err(err_400(
                "app_id and app_secret are required to connect a new Lark channel",
            ))
        };
    }
    if app_id.is_empty() || app_secret.is_empty() {
        return Err(err_400("app_id and app_secret must be provided together"));
    }
    Ok(LarkCredentialPlan::Validate {
        app_id: app_id.to_string(),
        app_secret: app_secret.to_string(),
    })
}

/// Build the `LarkConfig` to persist from the existing one (if any) plus this
/// request's changes, preserving every option the request does not mention.
/// `receive_mode`/`port` are out of this endpoint's scope (D-1/D-2: the
/// console connects websocket-mode Lark International/Feishu only) and are
/// left at whatever the existing config had, or the schema default for a
/// fresh connect.
fn apply_lark_update(
    existing: Option<LarkConfig>,
    new_credentials: Option<(&str, &str)>,
    allowed_users: Vec<String>,
    encrypt_key: Option<&str>,
    verification_token: Option<&str>,
    use_feishu: bool,
) -> Result<LarkConfig, ApiError> {
    let mut lc = match existing {
        Some(lc) => lc,
        None => {
            serde_json::from_value(json!({ "app_id": "", "app_secret": "", "allowed_users": [] }))
                .map_err(err_500)?
        }
    };
    if let Some((app_id, app_secret)) = new_credentials {
        lc.app_id = app_id.to_string();
        lc.app_secret = app_secret.to_string();
    }
    lc.allowed_users = allowed_users;
    lc.use_feishu = use_feishu;
    if let Some(key) = encrypt_key {
        let key = key.trim();
        lc.encrypt_key = (!key.is_empty()).then(|| key.to_string());
    }
    if let Some(token) = verification_token {
        let token = token.trim();
        lc.verification_token = (!token.is_empty()).then(|| token.to_string());
    }
    Ok(lc)
}

/// Whether a Lark save needs the channels runtime restarted.
///
/// A real credential change does (the WS/webhook client is built from it at
/// construction), and so does a real region change (it selects the host the
/// long-connection dials). Per plan 380's correction to the shape Discord and
/// Slack established: this compares VALUES, not presence — resubmitting the
/// same app_id/app_secret/region the config already has must not restart,
/// because the daemon hosts this gateway and a debounce-free restart storm can
/// trip systemd's start limit.
fn lark_restart_needed(credentials_changed: bool, region_before: bool, region_after: bool) -> bool {
    credentials_changed || region_before != region_after
}

/// Whether a validated credential pair actually differs from what is already
/// saved. Extracted as a pure function (mirroring how [`discord_restart_needed`]
/// takes `before`/`after` directly) so the VALUES-not-presence guarantee is
/// unit-testable without a live Lark/Feishu probe: `new_credentials.is_some()`
/// alone would be true even when an operator resubmits the unchanged pair,
/// which must not restart the channels runtime.
fn lark_credentials_changed(
    previous: Option<&LarkConfig>,
    new_app_id: &str,
    new_app_secret: &str,
) -> bool {
    previous.is_none_or(|p| p.app_id != new_app_id || p.app_secret != new_app_secret)
}

/// The connect response for Lark. Carries no credential: `app_id` is a public
/// application identifier (not the secret), reported the way `bot_username`
/// confirms identity for the other channels.
fn lark_connect_response(
    app_id: &str,
    allowed_users: usize,
    warning: Option<&str>,
    restarts_runtime: bool,
) -> serde_json::Value {
    json!({
        "connected": true,
        "channel": "lark",
        "app_id": app_id,
        "allowed_users": allowed_users,
        "warning": warning,
        "restarts_runtime": restarts_runtime,
        "note": runtime_restart_note(restarts_runtime),
    })
}

/// A non-empty `encrypt_key` bricks Lark's own startup (`lark.rs`'s
/// `encrypt_key` guard refuses to run because this build cannot decrypt event
/// bodies). The provisioner never asks for it for the same reason; this
/// endpoint accepts it for schema parity but warns, so a console operator
/// finds out at save time instead of from a channel that silently never
/// starts.
fn lark_encrypt_key_warning(encrypt_key: Option<&str>) -> Option<&'static str> {
    encrypt_key
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|_| {
            "encrypt_key is set — this build does not decrypt event bodies, and Lark will refuse \
             to start until it is cleared."
        })
}

async fn connect_lark(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LarkConnectBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;

    let existing = state.config.lock().channels_config.lark.clone();
    let plan = plan_lark_credentials(existing.as_ref(), &body.app_id, &body.app_secret)?;

    let effective_use_feishu = body
        .use_feishu
        .unwrap_or_else(|| existing.as_ref().map(|c| c.use_feishu).unwrap_or(false));

    // The probe (and any refusal) happens BEFORE `lock_and_load`/
    // `persist_and_swap` are ever reached, so a rejected credential writes
    // nothing — enforced by control flow (the `?` below), not by a
    // separate rollback step.
    let new_credentials = match &plan {
        LarkCredentialPlan::Validate { app_id, app_secret } => {
            let token_url =
                crate::onboard::provision::channels::lark::tenant_token_url(effective_use_feishu);
            let probe_body = json!({ "app_id": app_id, "app_secret": app_secret });
            let probe = crate::onboard::provision::validate::http::probe_post(
                token_url,
                &[],
                &serde_json::to_string(&probe_body).unwrap_or_default(),
            )
            .await;
            let verdict = match &probe {
                Ok(r) => {
                    crate::onboard::provision::channels::lark::classify_tenant_token_body(&r.body)
                }
                Err(e) => crate::onboard::provision::validate::verdict::ProbeVerdict::Inconclusive(
                    format!("{e}"),
                ),
            };
            refuse_unless_accepted(verdict, "Lark")?;
            Some((app_id.clone(), app_secret.clone()))
        }
        LarkCredentialPlan::KeepExisting => None,
    };

    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let previous = cfg.channels_config.lark.clone();
    let lc = apply_lark_update(
        previous.clone(),
        new_credentials
            .as_ref()
            .map(|(id, secret)| (id.as_str(), secret.as_str())),
        body.allowed_users.clone(),
        body.encrypt_key.as_deref(),
        body.verification_token.as_deref(),
        effective_use_feishu,
    )?;
    // Decided against the freshly loaded config, before the new one replaces
    // it, so resending unchanged credentials or region does not bounce the
    // daemon (plan 380's correction to the Discord/Slack shape: a real value
    // change, not mere presence in the body).
    let credentials_changed = match &new_credentials {
        Some((id, secret)) => lark_credentials_changed(previous.as_ref(), id, secret),
        None => false,
    };
    let region_before = previous.as_ref().map(|p| p.use_feishu).unwrap_or(false);
    let restarts_runtime =
        lark_restart_needed(credentials_changed, region_before, effective_use_feishu);
    let app_id = lc.app_id.clone();
    cfg.channels_config.lark = Some(lc);
    persist_and_swap(&state, cfg, "channels.lark").await?;

    if restarts_runtime {
        schedule_daemon_reload();
    }

    let warning = allowlist_warning(&body.allowed_users, "Lark")
        .or_else(|| lark_encrypt_key_warning(body.encrypt_key.as_deref()).map(str::to_string));
    Ok(Json(lark_connect_response(
        &app_id,
        body.allowed_users.len(),
        warning.as_deref(),
        restarts_runtime,
    )))
}

async fn disconnect_lark(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let was_configured = cfg.channels_config.lark.is_some();
    cfg.channels_config.lark = None;
    persist_and_swap(&state, cfg, "channels.lark").await?;
    if was_configured {
        schedule_daemon_reload();
    }
    Ok(Json(json!({
        "disconnected": was_configured,
        "channel": "lark",
        "restarts_runtime": was_configured,
    })))
}

// ── POST/DELETE /channels/whatsapp_web + POST /channels/whatsapp_web/pair ────
//
// Plan 367. Three rules bound the surface:
//   * Pair is refused while a `channels_config.whatsapp_web` section exists
//     (D-3: two clients never hold one session).
//   * The pair endpoint mints a fresh session file each call. The listener
//     opens `WhatsAppWebConfig.session_path` exactly as written
//     (`whatsapp_web.rs:923`), so a new pair never collides with an old
//     session, and on `Connected` the gateway persists that new path.
//   * The DELETE handler clears the section and schedules a reload. It
//     moves no file: the runtime's `build_channel_runtime` set-aside step
//     runs on the next start, before any channel listener can open
//     anything (the listener-held SQLite descriptor that stopped the
//     first version of this plan).

#[derive(Deserialize)]
struct WhatsAppWebConnectBody {
    /// Phone numbers allowed to talk to the bot (E.164, or "*"). An empty
    /// list denies everyone — the same convention Telegram and Discord
    /// use, and the warning the response carries names it.
    #[serde(default)]
    allowed_numbers: Vec<String>,
}

/// Held only while a `pair` stream is open. The handler flips it on
/// before yielding the SSE and clears it when the stream drops. A second
/// concurrent `POST /pair` returns 409.
///
/// `pub` because `tests/kb/api_test.rs` builds an `AppState` literal and
/// has to name the field's type; a `pub(crate)` alias is unreachable from
/// an integration test. The alias only reveals an `Arc<AtomicBool>`, the
/// same primitive the `pair` handler exchanges through it.
pub type PairGuard = Arc<std::sync::atomic::AtomicBool>;

/// The allowlist a request asks to save, each entry in the form the runtime
/// compares, or a 400 that names the first entry that cannot be one.
fn allowed_numbers_to_save(entries: &[String]) -> Result<Vec<String>, ApiError> {
    entries
        .iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .map(|entry| crate::config::WhatsAppWebConfig::allowlist_entry(entry).map_err(err_400))
        .collect()
}

async fn whatsapp_web_connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<WhatsAppWebConnectBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let previous = cfg.channels_config.whatsapp_web.clone();

    // `whatsapp_web` is a live-allowlist channel like Telegram: the runtime
    // pushes the new list through `Channel::apply_allowed_senders` on the
    // next message (`whatsapp_web.rs:1291`), so editing it does NOT need
    // a restart — the same rule plan 366 applied to Discord/Slack.
    let normalised = allowed_numbers_to_save(&body.allowed_numbers)?;

    let next = match previous {
        Some(mut existing) => {
            existing.allowed_numbers = normalised;
            existing
        }
        None => {
            // POST without a paired session yet — refuse, the operator
            // must pair through `POST /pair` first. Same shape as the
            // Telegram "token required" error.
            return Err(err_400(
                "whatsapp_web is not linked; pair first via POST /api/v1/channels/whatsapp_web/pair",
            ));
        }
    };

    cfg.channels_config.whatsapp_web = Some(next.clone());
    persist_and_swap(&state, cfg, "channels.whatsapp_web").await?;

    let warning = if next.allowed_numbers.is_empty() {
        Some(
            "allowed_numbers is empty — the bot will deny ALL senders until you add phone numbers."
                .to_string(),
        )
    } else if next.allowed_numbers.iter().any(|n| n.trim() == "*") {
        Some("allowed_numbers contains \"*\" — the bot will respond to ANY sender on WhatsApp Web. Use specific E.164 numbers unless this is intentional.".to_string())
    } else {
        None
    };

    // No restart — the runtime picks up the new allowlist on the next
    // message via `apply_allowed_senders`.
    Ok(Json(json!({
        "connected": true,
        "channel": "whatsapp_web",
        "allowed_numbers": next.allowed_numbers.len(),
        "warning": warning,
        "restarts_runtime": false,
        "note": "Saved. The running channel picks this up on its next message — no restart.",
    })))
}

async fn whatsapp_web_disconnect(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let (_guard, mut cfg) = lock_and_load(&state).await?;
    let was_configured = cfg.channels_config.whatsapp_web.is_some();
    cfg.channels_config.whatsapp_web = None;
    persist_and_swap(&state, cfg, "channels.whatsapp_web").await?;
    if was_configured {
        schedule_daemon_reload();
    }
    Ok(Json(json!({
        "disconnected": was_configured,
        "channel": "whatsapp_web",
        "restarts_runtime": was_configured,
        "note": runtime_restart_note(was_configured),
    })))
}

#[cfg(feature = "whatsapp-web")]
async fn whatsapp_web_pair(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<WhatsAppWebConnectBody>,
) -> Result<Response, ApiError> {
    check_auth(&state, &headers)?;

    // Before the guards below and before a pairing starts: this list is saved
    // on `Connected`, and the phone scan is no time to learn it cannot be.
    // Returning here also leaves the in-flight flag untouched.
    let normalised = allowed_numbers_to_save(&body.allowed_numbers)?;

    // D-3: refuse while a WhatsApp Web section is configured. Two clients
    // holding one session is the problem this guard closes.
    {
        let (_guard, cfg) = lock_and_load(&state).await?;
        if cfg.channels_config.whatsapp_web.is_some() {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "already_linked",
                    "detail": "WhatsApp Web is already linked. Disconnect first via DELETE /api/v1/channels/whatsapp_web, then pair again."
                })),
            ));
        }
    }

    // A single in-flight pairing at a time. The guard is on `AppState` so
    // every concurrent request sees the same flag; the SSE handler clears
    // it on drop.
    let guard = state.whatsapp_pair_guard.clone();
    if guard
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "pairing_in_progress",
                "detail": "Another pairing is already running. Cancel it or wait for it to finish."
            })),
        ));
    }

    // Mint a fresh session path under the workspace so the listener
    // opens a brand new file (`whatsapp_web.rs:923` opens whatever
    // `session_path` says, with no global state to clobber).
    let workspace = state.config.lock().workspace_dir.clone();
    let unix_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let session_path = workspace.join(format!("whatsapp-{unix_now}.db"));

    // Build the pair stream. `pair_once` is only available with the
    // `whatsapp-web` feature, and the entire handler is gated behind it.
    let pair_stream = {
        use crate::channels::whatsapp_web::{pair_once, PairOptions};
        pair_once(PairOptions::new(session_path.clone()))
    };

    let state_for_stream = state.clone();
    let guard_for_drop = guard.clone();
    let stream = async_stream::stream! {
        let _drop_guard = PairDropGuard {
            flag: &guard_for_drop,
        };
        let mut pin = Box::pin(pair_stream);
        use futures::StreamExt;
        while let Some(event) = pin.next().await {
            // Map each PairEvent into an SSE frame. PairCode is dropped
            // (D-2: QR only).
            let frame = match event {
                crate::channels::whatsapp_web::PairEvent::Qr(qr_payload) => {
                    let svg = render_qr_svg(&qr_payload);
                    json!({
                        "type": "qr",
                        "svg": svg,
                    })
                }
                crate::channels::whatsapp_web::PairEvent::PairCode(_) => {
                    // D-2: drop pair codes; the console never gets them.
                    continue;
                }
                crate::channels::whatsapp_web::PairEvent::Connected => {
                    // `Box::pin` so the surrounding async-stream future stays
                    // under the strict lint size budget (clippy::large_futures).
                    Box::pin(finish_pairing(
                        &state_for_stream,
                        session_path.clone(),
                        normalised.clone(),
                        schedule_daemon_reload,
                    ))
                    .await
                }
                crate::channels::whatsapp_web::PairEvent::Timeout => json!({
                    "type": "timeout",
                }),
                crate::channels::whatsapp_web::PairEvent::Failed(reason) => json!({
                    "type": "failed",
                    "reason": reason,
                }),
            };
            yield Ok::<SseEvent, Infallible>(
                SseEvent::default()
                    .event(frame.get("type").and_then(|v| v.as_str()).unwrap_or("message"))
                    .data(frame.to_string()),
            );
            // A terminal frame ends the stream; the drop guard clears
            // the in-flight flag.
            let terminal = matches!(
                frame.get("type").and_then(|v| v.as_str()),
                Some("connected" | "timeout" | "failed")
            );
            if terminal {
                break;
            }
        }
    };

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// RAII guard so the in-flight flag clears even if the stream is dropped
/// mid-flight (browser closes the page, client disconnects).
struct PairDropGuard<'a> {
    flag: &'a AtomicBool,
}
impl Drop for PairDropGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// Render a QR payload string into an SVG string using the `qrcode` crate's
/// `svg` feature. The SVG is inline (no network call, no external assets)
/// and the console renders it as an `<img src="data:image/svg+xml;...">`
/// per plan 369 — never as `innerHTML`. Only compiled with the
/// `whatsapp-web` feature; the connect/disconnect routes that don't render
/// a QR are available in every build.
#[cfg(feature = "whatsapp-web")]
fn render_qr_svg(payload: &str) -> String {
    use qrcode::render::svg;
    use qrcode::{EcLevel, QrCode};
    match QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M) {
        Ok(code) => code
            .render::<svg::Color<'_>>()
            .min_dimensions(200, 200)
            .build(),
        Err(e) => format!("<!-- qr render failed: {e} -->"),
    }
}

/// Save a linked session and its allowlist, then restart the runtime so the
/// channel starts on them.
///
/// The reload is scheduled only once the save succeeded: a restart before it
/// would start without WhatsApp Web, and a failed save must not restart
/// anything. `schedule_reload` is a parameter so a test can count the calls.
#[cfg(feature = "whatsapp-web")]
async fn finish_pairing(
    state: &AppState,
    session_path: PathBuf,
    allowed_numbers: Vec<String>,
    schedule_reload: impl FnOnce(),
) -> serde_json::Value {
    match Box::pin(persist_paired_session(
        state,
        session_path.clone(),
        allowed_numbers,
    ))
    .await
    {
        Ok(()) => {
            schedule_reload();
            json!({
                "type": "connected",
                "session_path": session_path.display().to_string(),
                "restarts_runtime": true,
            })
        }
        Err(err) => json!({
            "type": "failed",
            "reason": err,
        }),
    }
}

async fn persist_paired_session(
    state: &AppState,
    session_path: PathBuf,
    allowed_numbers: Vec<String>,
) -> Result<(), String> {
    let (_guard, mut cfg) = lock_and_load(state)
        .await
        .map_err(|e| format!("could not lock config: {e:?}"))?;
    let session_path_string = session_path.to_string_lossy().to_string();
    let next = crate::config::WhatsAppWebConfig {
        session_path: session_path_string,
        pair_phone: None,
        pair_code: None,
        allowed_numbers,
    };
    cfg.channels_config.whatsapp_web = Some(next);
    persist_and_swap(state, cfg, "channels.whatsapp_web")
        .await
        .map_err(|e| format!("could not persist config: {e:?}"))?;
    Ok(())
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
///
/// `api_url` is withheld when it holds a credential rather than a URL. Rejecting
/// such a value on write (see [`apply_secrets`]) only stops new ones; a config
/// written before that guard existed still carries it, and this view is what the
/// web console renders into a plain-text base-URL field. A malformed-but-harmless
/// value is still returned — the operator needs to see it to correct it, and
/// `doctor` reports that shape.
fn secrets_view(cfg: &crate::config::Config) -> serde_json::Value {
    let api_url = cfg.api_url.as_deref().and_then(sanitize_api_url);
    json!({
        "provider": cfg.default_provider.clone().unwrap_or_default(),
        "api_url": api_url,
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

fn apply_secrets(cfg: &mut crate::config::Config, body: &SecretsBody) -> Result<(), String> {
    if let Some(u) = body.api_url.as_ref() {
        let u = u.trim();
        if !u.is_empty() {
            validate_api_url(u)?;
        }
    }

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
    Ok(())
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
    let (_guard, mut cfg) = lock_and_load(&state).await?;
    apply_secrets(&mut cfg, &body).map_err(err_400)?;
    let present = api_key_present(&cfg);
    persist_and_swap(&state, cfg, "secrets").await?;
    Ok(Json(json!({ "ok": true, "api_key_present": present })))
}

// ── GET/PUT /config/knowledge (Knowledge Base credentials) ───────────────────

#[cfg(feature = "kb")]
#[derive(serde::Deserialize)]
struct KnowledgeBody {
    /// Omitted leaves the current value — same contract as the key fields.
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    embedding_api_key: Option<String>,
    #[serde(default)]
    vision_api_key: Option<String>,
}

/// Probe the configured embedding endpoint with a one-token input so the
/// probe exercises the REAL path (endpoint + model + key) a KB call will
/// take. Returns `Err(message)` only on an explicit 4xx auth/validation
/// rejection — a credential the provider rejects must not be saved (mirrors
/// the `getMe` probe in `connect_telegram`). Transport errors are NOT fatal:
/// an operator configuring the KB while offline must still be able to store
/// a key. The message names the status only; never the key, never the
/// upstream body.
///
/// Endpoint viability verified live (plan 103 open question): OpenRouter's
/// `/api/v1/embeddings` serves the default `qwen/qwen3-embedding-8b`.
#[cfg(feature = "kb")]
async fn probe_embedding_key(kb_cfg: &crate::kb::KbConfig, key: &str) -> Result<(), String> {
    let body = serde_json::json!({
        "model": kb_cfg.embedding_model,
        "input": ["ping"],
    });
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Ok(()), // cannot even build a client — do not fail closed
    };
    match client
        .post(&kb_cfg.embedding_base_url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            if status.is_client_error() {
                Err(format!(
                    "embedding provider rejected the key (http {})",
                    status.as_u16()
                ))
            } else {
                // 2xx = valid; 5xx = provider trouble, not the key's fault.
                Ok(())
            }
        }
        // Offline/proxy/DNS: store the key; the KB call path will surface
        // transport problems where they belong.
        Err(_) => Ok(()),
    }
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
        "enabled": cfg.knowledge.enabled,
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
    let (_guard, mut cfg) = lock_and_load(&state).await?;
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
    if let Some(e) = body.enabled {
        cfg.knowledge.enabled = e;
    }

    // Validate ONLY when this request would leave the KB enabled with a key
    // — deactivating or clearing a key never makes a network call. A typo'd
    // key used to persist happily and fail later as a 502 on every KB call,
    // far from the action that caused it (plan 103).
    if cfg.knowledge.enabled {
        if let Some(key) = cfg
            .knowledge
            .embedding_api_key
            .as_deref()
            .filter(|k| !k.is_empty())
        {
            let kb_cfg = crate::kb::KbConfig::from_env_with_keys(Some(key), None)
                .map_err(|e| err_400(format!("kb config: {e}")))?;
            if let Err(msg) = probe_embedding_key(&kb_cfg, key).await {
                return Err(err_400(msg));
            }
        } else if body.enabled == Some(true) {
            // Turning ON with no key anywhere is an operator mistake worth
            // stopping at the source.
            return Err(err_400(
                "cannot activate the knowledge base without an embedding key",
            ));
        }
    }
    persist_and_swap(&state, cfg, "knowledge").await?;
    // New credentials invalidate any cached KB embedding/extraction context.
    // `clear_kb_ctx` is sufficient: the next KB request rebuilds the context
    // in-process with the new key. Do NOT call `schedule_daemon_reload()` here
    // — the managed service hosts this gateway (`daemon::run` spawns
    // `run_gateway`), so a restart would take the console offline mid-save.
    // Channel connects/disconnects still reload, because the channels
    // supervisor captures its channel set at startup and can't pick up the
    // change any other way; a KB key needs no such bounce.
    crate::kb::axi::clear_kb_ctx().await;
    let cfg = state.config.lock().clone();
    Ok(Json(json!({
        "enabled": cfg.knowledge.enabled,
        "embedding_configured": cfg.knowledge.embedding_api_key.is_some(),
        "vision_configured": cfg.knowledge.vision_api_key.is_some(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// A console built before the token ceiling still sends
    /// `max_cost_per_day_cents` in its autonomy PATCH. The key it wrote is gone,
    /// but rejecting the request over it would break every other autonomy edit
    /// in the same body — an operator changing their allowlist would get a 400
    /// because of a field they never touched.
    #[test]
    fn an_autonomy_body_still_deserialises_with_the_retired_money_key() {
        let body: AutonomyBody = serde_json::from_value(serde_json::json!({
            "level": "full",
            "max_actions_per_hour": 300,
            "max_cost_per_day_cents": 500,
        }))
        .expect("the retired key must not fail the whole request");

        assert_eq!(body.level.as_deref(), Some("full"));
        assert_eq!(
            body.max_actions_per_hour,
            Some(300),
            "the fields that still exist are applied"
        );
        assert_eq!(
            body.max_cost_per_day_cents,
            Some(500),
            "and the retired one is parsed, then ignored by `set_autonomy`"
        );
    }

    /// The gateway is bound to one config file from boot. `load_or_init`
    /// re-resolves the path from the environment and the
    /// `active_workspace.toml` marker on every call, so a marker or env var
    /// that changed after boot made a console write read-modify-save a
    /// DIFFERENT file and swap it into the running state.
    #[tokio::test]
    async fn a_console_write_reads_the_file_the_gateway_booted_with() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp root");

        // The file the gateway is running.
        let running_dir = tmp.path().join("running");
        std::fs::create_dir_all(&running_dir).expect("running dir");
        let running_config = running_dir.join("config.toml");
        std::fs::write(
            &running_config,
            "default_model = \"model-the-gateway-is-running\"\n",
        )
        .expect("write running config");

        // A different file the environment now points at.
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere dir");
        std::fs::write(
            elsewhere.join("config.toml"),
            "default_model = \"model-from-somewhere-else\"\n",
        )
        .expect("write other config");

        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let _g_dir = crate::test_env::EnvGuard::set("RANTAICLAW_CONFIG_DIR", &elsewhere);

        let loaded = load_running_config(&running_config)
            .await
            .expect("load the running config");

        assert_eq!(
            loaded.default_model.as_deref(),
            Some("model-the-gateway-is-running"),
            "a console write must read the file the gateway booted with, not \
             whatever the environment resolves to now"
        );
        assert_eq!(loaded.config_path, running_config);
    }

    /// `load_or_init` applies env overrides and the running config was built
    /// that way. Loading by path alone would drop an env-supplied credential
    /// out of the running process on the first console write — a regression
    /// worse than the split-brain being fixed. (`save()` strips them again on
    /// the way to disk, so they never become permanent.)
    #[tokio::test]
    async fn the_running_config_keeps_env_supplied_values() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp root");
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "default_model = \"m\"\n").expect("write config");

        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let _key = crate::test_env::EnvGuard::set("RANTAICLAW_API_KEY", "key-from-the-environment");

        let loaded = load_running_config(&config_path)
            .await
            .expect("load the running config");

        assert_eq!(
            loaded.api_key.as_deref(),
            Some("key-from-the-environment"),
            "an env-supplied credential must survive into the running config"
        );
    }

    #[test]
    fn err_500_does_not_leak_path() {
        // A load/save failure carries an absolute config path and, in some
        // cases, the gateway host:port. The response `detail` must not echo it —
        // the specific cause belongs in the server log, not the browser.
        let (status, Json(body)) =
            err_500("failed to load /home/rantaiclaw/.rantaiclaw/config.toml at 127.0.0.1:9494");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let detail = body
            .get("detail")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(
            !detail.contains("config.toml"),
            "detail leaked a path: {detail}"
        );
        assert!(!detail.contains("/home"), "detail leaked a path: {detail}");
        assert!(
            !detail.contains("9494"),
            "detail leaked an address: {detail}"
        );
        // The stable error code is still present for the console to map.
        assert_eq!(
            body.get("error").and_then(serde_json::Value::as_str),
            Some("internal_error")
        );
    }

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
    fn redact_config_secrets_leaves_no_marker_in_real_config() {
        // Walk the REAL `Config` struct rather than a hand-written JSON literal, so
        // a newly-added secret field that isn't redacted fails this test instead of
        // silently leaking. A distinctive marker is written into every secret-bearing
        // field this function clears; after the full redaction pass (typed clear +
        // JSON backstop, the same order `get_config` runs) the marker must be gone.
        const MARKER: &str = "MARKER_SECRET_a1b2c3d4";
        let mut cfg = Config::default();
        cfg.api_key = Some(MARKER.into());
        cfg.api_url = Some(format!("https://user:{MARKER}@host.example/v1"));
        cfg.provider_api_keys.insert("openai".into(), MARKER.into());
        cfg.knowledge.embedding_api_key = Some(MARKER.into());
        cfg.knowledge.vision_api_key = Some(MARKER.into());
        cfg.proxy.http_proxy = Some(format!("http://user:{MARKER}@proxy.example:8080"));
        cfg.mcp_servers.insert(
            "srv".into(),
            crate::config::schema::McpServerConfig {
                command: "npx".into(),
                args: vec![
                    "-y".into(),
                    "server".into(),
                    "--api-key".into(),
                    MARKER.into(),
                ],
                env: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("DATABASE_URL".to_string(), MARKER.to_string());
                    m
                },
            },
        );

        // Mirror `get_config`: typed clear, then serialize, then the JSON backstop.
        redact_config_secrets(&mut cfg);
        let mut v = serde_json::to_value(&cfg).unwrap();
        redact_secrets_in_json(&mut v);
        let json = v.to_string();
        assert!(
            !json.contains(MARKER),
            "a secret marker survived redaction in GET /config: {json}"
        );
    }

    #[test]
    fn validate_mcp_command_rejects_shell_expressions() {
        // A plain program name or path is fine (args are a separate field).
        assert!(validate_mcp_command("npx").is_ok());
        assert!(validate_mcp_command("/usr/local/bin/uvx").is_ok());
        // Shell-metacharacter injection in the command field is rejected.
        for bad in [
            "npx; curl evil",
            "a | b",
            "x && y",
            "`id`",
            "$(id)",
            "cat </etc/passwd",
        ] {
            assert!(
                validate_mcp_command(bad).is_err(),
                "shell-expression command should be rejected: {bad}"
            );
        }
    }

    #[test]
    fn is_loader_env_key_flags_loader_vars() {
        for k in [
            "LD_PRELOAD",
            "ld_preload",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
        ] {
            assert!(is_loader_env_key(k), "{k} should be flagged");
        }
        for k in ["PATH", "API_KEY", "NODE_ENV"] {
            assert!(!is_loader_env_key(k), "{k} should be allowed");
        }
    }

    #[test]
    fn config_change_event_records_section_not_values() {
        let event = config_change_event("autonomy");
        let json = serde_json::to_string(&event).expect("event should serialize");
        // Records WHAT section changed and WHO (anonymous console principal)…
        assert!(json.contains("config_change"), "event type: {json}");
        assert!(json.contains("web-console"), "actor: {json}");
        assert!(json.contains("autonomy"), "section: {json}");
        // …but only the section NAME is carried, never a value, so a secret set
        // through the same handler can never reach the audit record.
        assert!(
            !json.contains("MARKER_SECRET"),
            "no value should leak: {json}"
        );
    }

    #[test]
    fn validate_tool_entries_rejects_malformed_and_trims() {
        // Empty / whitespace / multi-token entries can never match the exact-string
        // gate, so they must be rejected rather than silently failing open.
        assert!(validate_tool_entries(vec![String::new()]).is_err());
        assert!(validate_tool_entries(vec!["   ".into()]).is_err());
        assert!(validate_tool_entries(vec!["shell tool".into()]).is_err());
        // Well-formed entries pass and are trimmed; `*` is allowed.
        let ok = validate_tool_entries(vec!["  shell  ".into(), "*".into()])
            .expect("well-formed entries should pass");
        assert_eq!(ok, vec!["shell".to_string(), "*".to_string()]);
    }

    #[test]
    fn merge_mcp_server_keeps_existing_args_and_env_when_omitted() {
        let mut env = std::collections::HashMap::new();
        env.insert("TOKEN".to_string(), "existing-secret".to_string());
        let existing = crate::config::schema::McpServerConfig {
            command: "npx".into(),
            args: vec!["-y".into(), "server".into()],
            env,
        };
        // Re-add with only a new command (args/env omitted) — must not wipe them.
        let merged = merge_mcp_server(Some(&existing), "node".into(), None, None);
        assert_eq!(merged.command, "node");
        assert_eq!(merged.args, vec!["-y".to_string(), "server".to_string()]);
        assert_eq!(
            merged.env.get("TOKEN").map(String::as_str),
            Some("existing-secret")
        );
        // An explicit empty env clears it; a brand-new entry defaults to empty.
        let cleared = merge_mcp_server(
            Some(&existing),
            "node".into(),
            None,
            Some(std::collections::HashMap::new()),
        );
        assert!(cleared.env.is_empty());
        let fresh = merge_mcp_server(None, "npx".into(), None, None);
        assert!(fresh.args.is_empty() && fresh.env.is_empty());
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
    fn config_api_redacts_skill_literal_value() {
        // A `source = "literal"` skill API key is decrypted into memory on
        // load (plan 045) and must never leak back out through the config
        // API response, same as every other credential this redactor clears.
        let mut cfg = Config::default();
        cfg.skills.entries.insert(
            "x".into(),
            crate::config::SkillEntryConfig {
                api_key: Some(crate::config::SkillApiKey {
                    source: "literal".into(),
                    id: None,
                    value: Some("neutral-test-skill-key-value".into()),
                }),
                ..Default::default()
            },
        );
        redact_config_secrets(&mut cfg);
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(
            !json.contains("neutral-test-skill-key-value"),
            "leaked in:\n{json}"
        );
        assert!(cfg
            .skills
            .entries
            .get("x")
            .and_then(|e| e.api_key.as_ref())
            .and_then(|k| k.value.as_ref())
            .is_none());
    }

    #[test]
    fn redact_secrets_in_json_nulls_all_channel_and_gateway_secrets() {
        // Mirrors the shape `get_config` serializes: channel credentials the
        // typed redactor missed (Discord/Slack/WhatsApp/Matrix/webhook/…), the
        // gateway login hash + paired tokens, and tunnel tokens must all be
        // stripped — while a non-secret like `max_tokens` (contains "token")
        // must survive.
        let mut v = serde_json::json!({
            "channels_config": {
                "telegram": { "bot_token": "SEC_TG", "allowed_users": ["u"] },
                "discord": { "bot_token": "SEC_DISCORD" },
                "slack": { "bot_token": "SEC_SLACK", "app_token": "SEC_SLACK_APP" },
                "whatsapp": { "access_token": "SEC_WA", "app_secret": "SEC_WA_SECRET", "verify_token": "SEC_WA_VERIFY" },
                "linq": { "api_token": "SEC_LINQ", "signing_secret": "SEC_LINQ_SIG" },
                "nextcloud_talk": { "app_token": "SEC_NC", "webhook_secret": "SEC_NC_WH" },
                "matrix": { "token": "SEC_MATRIX" },
                "webhook": { "secret": "SEC_WEBHOOK" },
                "irc": { "server_password": "SEC_IRC_SRV", "nickserv_password": "SEC_IRC_NS", "sasl_password": "SEC_IRC_SASL" },
                "lark": { "encrypt_key": "SEC_LARK_ENC", "verification_token": "SEC_LARK_VER" },
            },
            "gateway": {
                "login": { "password_hash": "SEC_PWHASH" },
                "paired_tokens": ["SEC_PAIRED"],
                "rate_limit_max_keys": 10000,
                "idempotency_max_keys": 10000,
            },
            "tunnel": { "auth_token": "SEC_TUNNEL" },
            "api_key": "SEC_API",
            "provider_api_keys": { "openai": "SEC_PROVIDER" },
            "secrets": { "encrypt": true },
            "agent": { "max_tokens": 4096, "max_history_messages": 50 },
            "knowledge": { "chunk_max_tokens": 512 },
        });
        redact_secrets_in_json(&mut v);
        let json = v.to_string();
        for sec in [
            "SEC_TG",
            "SEC_DISCORD",
            "SEC_SLACK",
            "SEC_SLACK_APP",
            "SEC_WA",
            "SEC_WA_SECRET",
            "SEC_WA_VERIFY",
            "SEC_LINQ",
            "SEC_LINQ_SIG",
            "SEC_NC",
            "SEC_NC_WH",
            "SEC_MATRIX",
            "SEC_WEBHOOK",
            "SEC_IRC_SRV",
            "SEC_IRC_NS",
            "SEC_IRC_SASL",
            "SEC_LARK_ENC",
            "SEC_LARK_VER",
            "SEC_PWHASH",
            "SEC_PAIRED",
            "SEC_TUNNEL",
            "SEC_API",
            "SEC_PROVIDER",
        ] {
            assert!(
                !json.contains(sec),
                "secret {sec} leaked in GET /config: {json}"
            );
        }
        // Non-secret look-alikes (counts / a config SECTION named `secrets`) whose
        // keys share a secret stem must NOT be nulled.
        assert_eq!(v["agent"]["max_tokens"], serde_json::json!(4096));
        assert_eq!(v["agent"]["max_history_messages"], serde_json::json!(50));
        assert_eq!(v["knowledge"]["chunk_max_tokens"], serde_json::json!(512));
        assert_eq!(
            v["gateway"]["rate_limit_max_keys"],
            serde_json::json!(10000)
        );
        assert_eq!(
            v["gateway"]["idempotency_max_keys"],
            serde_json::json!(10000)
        );
        assert_eq!(v["secrets"]["encrypt"], serde_json::json!(true));
        // String secrets are redacted to "" (type preserved), not null.
        assert_eq!(
            v["channels_config"]["telegram"]["bot_token"],
            serde_json::json!("")
        );
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
        )
        .unwrap();
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
        )
        .unwrap();
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
    fn allowlist_only_update_does_not_restart_the_runtime() {
        // The operator-reported bug: saving an allowlist restarted the managed
        // service, which hosts this gateway, so the save killed the request that
        // made it. The runtime applies allowlists live now — restarting for one
        // is pure cost, and repeated saves could trip systemd's start limit.
        assert!(
            !needs_runtime_restart(false),
            "an allowlist-only edit must not restart the channels runtime"
        );
    }

    /// The note and the `restarts_runtime` flag are one decision reported two
    /// ways, and a client reads whichever it can. They must not describe
    /// different worlds — the console shipped a hint saying every save reloads
    /// the runtime, printed directly above this reply saying it does not.
    #[test]
    fn the_restart_note_agrees_with_the_flag_it_is_derived_from() {
        let restarting = runtime_restart_note(true);
        let live = runtime_restart_note(false);
        assert!(
            restarting.contains("Reloading the runtime"),
            "a restarting save must say so: {restarting}"
        );
        assert!(
            live.contains("no restart"),
            "a live save must say so: {live}"
        );
        // Control: the two branches are actually different text, so the
        // assertions above cannot both pass against one shared string.
        assert_ne!(restarting, live);
    }

    #[test]
    fn token_change_still_restarts_the_runtime() {
        // A new token creates or replaces the channel itself; the running
        // runtime cannot swap that in place.
        assert!(
            needs_runtime_restart(true),
            "a token change must still restart the channels runtime"
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

    fn discord_config(token: &str) -> DiscordConfig {
        serde_json::from_value(json!({
            "bot_token": token,
            "allowed_users": ["U_KEEP"],
            "listen_to_bots": true,
            "mention_only": true,
            "guild_id": "G1",
        }))
        .expect("a Discord section")
    }

    fn slack_config(token: &str) -> SlackConfig {
        serde_json::from_value(json!({
            "bot_token": token,
            "app_token": "xapp-1-AAA",
            "channel_id": "C1",
            "allowed_users": ["U_KEEP"],
        }))
        .expect("a Slack section")
    }

    /// The three cases the Telegram planner already distinguishes, per channel:
    /// a new token is validated, an omitted token keeps the saved one so an
    /// allowlist edit needs no re-entry, and an omitted token with nothing saved
    /// is refused rather than written as an empty credential.
    #[test]
    fn discord_token_plan_covers_new_kept_and_missing() {
        match plan_discord_token(None, "  discord-token  ").expect("a new token") {
            TokenPlan::Validate(t) => assert_eq!(t, "discord-token", "the token is trimmed"),
            TokenPlan::KeepExisting => panic!("a supplied token must be validated"),
        }

        let existing = discord_config("saved-token");
        assert!(matches!(
            plan_discord_token(Some(&existing), "").expect("keeps the saved token"),
            TokenPlan::KeepExisting
        ));

        let err = plan_discord_token(None, "   ").expect_err("nothing saved, nothing supplied");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn slack_token_plan_covers_new_kept_and_missing() {
        match plan_slack_token(None, " xoxb-abc ").expect("a new token") {
            TokenPlan::Validate(t) => assert_eq!(t, "xoxb-abc"),
            TokenPlan::KeepExisting => panic!("a supplied token must be validated"),
        }

        let existing = slack_config("xoxb-saved");
        assert!(matches!(
            plan_slack_token(Some(&existing), "").expect("keeps the saved token"),
            TokenPlan::KeepExisting
        ));

        assert_eq!(
            plan_slack_token(None, "").expect_err("nothing saved").0,
            StatusCode::BAD_REQUEST
        );
    }

    /// The half that is easy to lose: an allowlist-only edit must not clear the
    /// options the request never mentions.
    #[test]
    fn a_discord_allowlist_edit_keeps_every_other_option() {
        let updated = apply_discord_update(
            Some(discord_config("saved-token")),
            None,
            vec!["U_NEW".into()],
            None,
        )
        .expect("apply");

        assert_eq!(updated.bot_token, "saved-token", "the saved token survives");
        assert_eq!(updated.allowed_users, vec!["U_NEW".to_string()]);
        assert!(updated.listen_to_bots, "listen_to_bots was cleared");
        assert!(updated.mention_only, "mention_only was cleared");
        assert_eq!(
            updated.guild_id.as_deref(),
            Some("G1"),
            "an unmentioned guild_id was cleared"
        );
    }

    #[test]
    fn a_slack_allowlist_edit_keeps_every_other_option() {
        let updated = apply_slack_update(
            Some(slack_config("xoxb-saved")),
            None,
            vec!["U_NEW".into()],
            None,
            None,
        )
        .expect("apply");

        assert_eq!(updated.bot_token, "xoxb-saved");
        assert_eq!(updated.allowed_users, vec!["U_NEW".to_string()]);
        assert_eq!(
            updated.app_token.as_deref(),
            Some("xapp-1-AAA"),
            "an unmentioned app_token was cleared, silently turning off Socket Mode"
        );
        assert_eq!(updated.channel_id.as_deref(), Some("C1"));
    }

    /// The console refuses a malformed app token instead of warning: it can ask
    /// again, which the CLI wizard cannot, so the CLI keeps warn-and-save.
    #[test]
    fn a_malformed_slack_app_token_is_refused() {
        assert!(is_valid_slack_app_token("xapp-1-A0B1C2"));
        assert!(!is_valid_slack_app_token("xoxb-not-an-app-token"));
        assert!(!is_valid_slack_app_token("   "));

        let err = apply_slack_update(None, Some("xoxb-abc"), vec![], Some("nope"), None)
            .expect_err("a malformed app token is a 400");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    /// F-3 from the 2026-09-11 drive: under Socket Mode a `channel_id` filter
    /// drops direct messages. Only when both are set, and in the field the
    /// console already reads for Telegram.
    #[test]
    fn the_socket_mode_dm_caveat_appears_only_with_both_fields() {
        assert!(socket_mode_dm_caveat(Some("xapp-1-A"), Some("C1")).is_some());
        assert!(socket_mode_dm_caveat(Some("xapp-1-A"), None).is_none());
        assert!(socket_mode_dm_caveat(None, Some("C1")).is_none());
        assert!(socket_mode_dm_caveat(None, None).is_none());

        let caveat = socket_mode_dm_caveat(Some("xapp-1-A"), Some("C1")).expect("a caveat");
        assert!(
            caveat.to_lowercase().contains("direct message"),
            "the caveat must name what is dropped: {caveat}"
        );
    }

    fn lark_config(app_id: &str, app_secret: &str, use_feishu: bool) -> LarkConfig {
        serde_json::from_value(json!({
            "app_id": app_id,
            "app_secret": app_secret,
            "allowed_users": ["U_KEEP"],
            "use_feishu": use_feishu,
        }))
        .expect("a Lark section")
    }

    /// The three cases the credential-pair planner distinguishes: a fresh pair
    /// is validated, an omitted pair keeps the saved one so an allowlist/region
    /// edit needs no re-entry, and an omitted pair with nothing saved is
    /// refused rather than written half-empty.
    #[test]
    fn lark_credential_plan_covers_new_kept_and_missing() {
        match plan_lark_credentials(None, "  app-1  ", "  secret-not-real  ").expect("a new pair") {
            LarkCredentialPlan::Validate { app_id, app_secret } => {
                assert_eq!(app_id, "app-1", "the app_id is trimmed");
                assert_eq!(app_secret, "secret-not-real", "the app_secret is trimmed");
            }
            LarkCredentialPlan::KeepExisting => panic!("a supplied pair must be validated"),
        }

        let existing = lark_config("saved-app", "saved-secret-not-real", false);
        assert!(matches!(
            plan_lark_credentials(Some(&existing), "", "").expect("keeps the saved pair"),
            LarkCredentialPlan::KeepExisting
        ));

        let err = plan_lark_credentials(None, "", "").expect_err("nothing saved, nothing supplied");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    /// A credential is two fields that must travel together — one without the
    /// other can never validate, so it is refused as malformed rather than
    /// silently paired with whatever the other field defaults to.
    #[test]
    fn lark_credential_plan_refuses_a_half_supplied_pair() {
        let err = plan_lark_credentials(None, "app-1", "")
            .expect_err("app_secret alone is not a credential");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let err = plan_lark_credentials(None, "", "secret-not-real")
            .expect_err("app_id alone is not a credential");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    /// The half that is easy to lose: an allowlist-only edit must not clear the
    /// credential pair, region, or webhook options the request does not
    /// mention.
    #[test]
    fn a_lark_allowlist_edit_keeps_every_other_option() {
        let mut existing = lark_config("saved-app", "saved-secret-not-real", true);
        existing.encrypt_key = Some("saved-encrypt-key".into());
        existing.verification_token = Some("saved-verification-token".into());

        let updated =
            apply_lark_update(Some(existing), None, vec!["U_NEW".into()], None, None, true)
                .expect("apply");

        assert_eq!(updated.app_id, "saved-app", "the saved app_id survives");
        assert_eq!(
            updated.app_secret, "saved-secret-not-real",
            "the saved app_secret survives"
        );
        assert_eq!(updated.allowed_users, vec!["U_NEW".to_string()]);
        assert!(
            updated.use_feishu,
            "the region survives when re-passed unchanged"
        );
        assert_eq!(
            updated.encrypt_key.as_deref(),
            Some("saved-encrypt-key"),
            "an unmentioned encrypt_key was cleared"
        );
        assert_eq!(
            updated.verification_token.as_deref(),
            Some("saved-verification-token"),
            "an unmentioned verification_token was cleared"
        );
    }

    /// `encrypt_key`/`verification_token` follow the present-even-empty-clears
    /// convention `guild_id` established for Discord: `Some("")` clears,
    /// `Some(value)` sets, `None` (the field omitted) leaves the saved value.
    #[test]
    fn lark_encrypt_key_and_verification_token_can_be_cleared_explicitly() {
        let mut existing = lark_config("saved-app", "saved-secret-not-real", false);
        existing.encrypt_key = Some("saved-encrypt-key".into());
        existing.verification_token = Some("saved-verification-token".into());

        let updated = apply_lark_update(Some(existing), None, vec![], Some(""), Some(""), false)
            .expect("apply");

        assert!(
            updated.encrypt_key.is_none(),
            "an explicitly empty encrypt_key clears it"
        );
        assert!(
            updated.verification_token.is_none(),
            "an explicitly empty verification_token clears it"
        );
    }

    /// Plan 380's correction to the shape Discord/Slack established: the
    /// restart decision compares VALUES, not the mere presence of a field in
    /// the body. Resubmitting the same credentials or region must not restart
    /// the channels runtime, which hosts this gateway.
    #[test]
    fn lark_restart_decision_compares_values_not_presence() {
        assert!(
            !lark_restart_needed(false, false, false),
            "an allowlist-only edit (same region) must not restart"
        );
        assert!(
            !lark_restart_needed(false, true, true),
            "resubmitting the same region must not restart"
        );
        assert!(
            lark_restart_needed(true, false, false),
            "a changed credential must restart even with the region unchanged"
        );
        assert!(
            lark_restart_needed(false, false, true),
            "a changed region must restart even with credentials unchanged"
        );
    }

    /// The half `lark_restart_decision_compares_values_not_presence` cannot
    /// reach: whether a supplied, live-validated pair actually differs from
    /// what is saved, not merely that a pair was supplied. Resubmitting the
    /// unchanged credentials on an already-connected channel must compute
    /// `false` here, or the handler would restart on every re-save.
    #[test]
    fn lark_credentials_changed_compares_values_not_presence() {
        let existing = lark_config("saved-app", "saved-secret-not-real", false);

        assert!(
            !lark_credentials_changed(Some(&existing), "saved-app", "saved-secret-not-real"),
            "resubmitting the identical pair must not count as a change"
        );
        assert!(
            lark_credentials_changed(Some(&existing), "new-app", "saved-secret-not-real"),
            "a changed app_id is a real change"
        );
        assert!(
            lark_credentials_changed(Some(&existing), "saved-app", "new-secret-not-real"),
            "a changed app_secret is a real change"
        );
        assert!(
            lark_credentials_changed(None, "saved-app", "saved-secret-not-real"),
            "a fresh connect (nothing saved yet) is always a change"
        );
    }

    /// The response never carries a credential — `app_id` is a public
    /// identifier, but nothing else from the request reaches the JSON, the
    /// same guarantee Discord/Slack give by never taking a credential
    /// parameter at all.
    #[test]
    fn lark_connect_response_never_echoes_a_credential() {
        let response = lark_connect_response("app-1", 2, None, false);
        let serialized = response.to_string();
        assert!(
            !serialized.contains("secret-not-real"),
            "the response must never contain an app_secret value"
        );
        assert_eq!(response["app_id"], "app-1");
        assert_eq!(response["channel"], "lark");
        assert!(response.get("app_secret").is_none());
        assert!(response.get("encrypt_key").is_none());
        assert!(response.get("verification_token").is_none());
    }

    /// A non-empty `encrypt_key` bricks Lark's own startup guard
    /// (`lark.rs`'s check that this build cannot decrypt event bodies). The
    /// connect response warns about it instead of saving it silently.
    #[test]
    fn lark_encrypt_key_warning_fires_only_when_set() {
        assert!(lark_encrypt_key_warning(None).is_none());
        assert!(lark_encrypt_key_warning(Some("")).is_none());
        assert!(lark_encrypt_key_warning(Some("   ")).is_none());
        let warning = lark_encrypt_key_warning(Some("a-real-key")).expect("a warning");
        assert!(warning.contains("does not decrypt"));
    }

    /// A running Lark channel, for the direct-handler tests below. Nothing on
    /// disk yet, so the first write saves this config to `root`.
    fn running_lark_config(root: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.config_path = root.join("config.toml");
        config.workspace_dir = root.join("workspace");
        config.channels_config.lark =
            Some(lark_config("saved-app", "saved-secret-not-real", false));
        config
    }

    /// The only `connect_lark` path this suite can exercise without a live
    /// Lark/Feishu endpoint: both credential fields omitted, so
    /// `plan_lark_credentials` returns `KeepExisting` and the probe (and any
    /// network call) never runs. Exercises the real handler end to end:
    /// allowlist applied, credentials/region untouched, no restart.
    #[tokio::test]
    async fn lark_allowlist_only_edit_persists_without_touching_credentials() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp root");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let state = console_state(running_lark_config(tmp.path()));

        let response = Box::pin(connect_lark(
            State(state.clone()),
            HeaderMap::new(),
            Json(LarkConnectBody {
                app_id: String::new(),
                app_secret: String::new(),
                encrypt_key: None,
                verification_token: None,
                allowed_users: vec!["U_NEW".into()],
                use_feishu: None,
            }),
        ))
        .await
        .expect("an allowlist-only edit must succeed without a live probe");

        assert_eq!(response.0["restarts_runtime"], false);
        assert_eq!(response.0["app_id"], "saved-app");
        let serialized = response.0.to_string();
        assert!(
            !serialized.contains("saved-secret-not-real"),
            "the response must never contain the app_secret"
        );

        let saved = state
            .config
            .lock()
            .channels_config
            .lark
            .clone()
            .expect("still connected");
        assert_eq!(saved.app_id, "saved-app", "credentials untouched");
        assert_eq!(saved.app_secret, "saved-secret-not-real");
        assert!(!saved.use_feishu, "region untouched");
        assert_eq!(saved.allowed_users, vec!["U_NEW".to_string()]);
    }

    /// A request that supplies only one half of the credential pair is
    /// refused before any config is touched — `connect_lark` end to end, not
    /// just the pure planner.
    #[tokio::test]
    async fn lark_connect_refuses_a_half_supplied_pair_and_writes_nothing() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp root");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let state = console_state(running_lark_config(tmp.path()));

        let refused = Box::pin(connect_lark(
            State(state.clone()),
            HeaderMap::new(),
            Json(LarkConnectBody {
                app_id: "new-app".into(),
                app_secret: String::new(),
                encrypt_key: None,
                verification_token: None,
                allowed_users: vec![],
                use_feishu: None,
            }),
        ))
        .await;

        let Err((status, Json(body))) = refused else {
            panic!("a half-supplied pair must be refused");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let saved = state
            .config
            .lock()
            .channels_config
            .lark
            .clone()
            .expect("still connected");
        assert_eq!(saved.app_id, "saved-app", "nothing was written");
    }

    #[tokio::test]
    async fn disconnect_lark_clears_the_section_and_restarts() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp root");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let state = console_state(running_lark_config(tmp.path()));

        let response = Box::pin(disconnect_lark(State(state.clone()), HeaderMap::new()))
            .await
            .expect("disconnect must succeed");

        assert_eq!(response.0["disconnected"], true);
        assert_eq!(response.0["channel"], "lark");
        assert_eq!(response.0["restarts_runtime"], true);
        assert!(
            state.config.lock().channels_config.lark.is_none(),
            "the section must be cleared"
        );
    }

    #[tokio::test]
    async fn disconnect_lark_on_an_unconfigured_channel_does_not_restart() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp root");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let mut config = Config::default();
        config.config_path = tmp.path().join("config.toml");
        config.workspace_dir = tmp.path().join("workspace");
        let state = console_state(config);

        let response = Box::pin(disconnect_lark(State(state.clone()), HeaderMap::new()))
            .await
            .expect("disconnect must succeed even when nothing was configured");

        assert_eq!(response.0["disconnected"], false);
        assert_eq!(response.0["restarts_runtime"], false);
    }

    /// Structural guarantee for the one path this suite cannot exercise
    /// behaviourally without a live Lark/Feishu endpoint: a fresh connect's
    /// probe-and-refuse must run BEFORE `lock_and_load`/`persist_and_swap`, so
    /// a rejected credential writes nothing. `classify_tenant_token_body` and
    /// `refuse_unless_accepted` already have their own unit coverage; this
    /// pins the ordering between them and the write.
    #[test]
    fn lark_connect_probes_before_it_writes() {
        let src = include_str!("config_api.rs");
        let handler = src
            .split("async fn connect_lark(")
            .nth(1)
            .and_then(|rest| rest.split("\nasync fn disconnect_lark(").next())
            .expect("the connect_lark handler");
        let probe_at = handler
            .find("refuse_unless_accepted(verdict, \"Lark\")?;")
            .expect("the handler must refuse an unaccepted verdict");
        let write_at = handler
            .find("lock_and_load(&state)")
            .expect("the handler must load before writing");
        assert!(
            probe_at < write_at,
            "the probe refusal must happen before the config is loaded for writing"
        );
    }

    /// The one mistake that cannot be walked back once a console has logged the
    /// response. Checked at any depth, not just the top level.
    #[test]
    fn no_connect_response_carries_the_token() {
        let secret = "xoxb-super-secret-value";
        for body in [
            discord_connect_response(Some("BotName"), 2, None, true),
            slack_connect_response(Some("TeamName"), 2, None, false),
        ] {
            let rendered = serde_json::to_string(&body).expect("serialise");
            assert!(
                !rendered.contains(secret),
                "a token reached the response body: {rendered}"
            );
            assert!(
                !rendered.contains("bot_token") && !rendered.contains("app_token"),
                "the response names a credential field: {rendered}"
            );
        }
    }

    /// The console has no prompt, so it cannot ask "save it anyway?" the way the
    /// CLI wizard does. Rule 2 of this effort refuses a credential that cannot be
    /// validated, and the gateway already fails closed for Telegram, so both a
    /// rejection and an inconclusive probe are a 400 here. The two messages
    /// differ, so an operator on an offline host is not told their token is bad.
    #[test]
    fn only_an_accepted_probe_lets_a_token_be_saved() {
        use crate::onboard::provision::validate::verdict::ProbeVerdict;

        assert!(refuse_unless_accepted(ProbeVerdict::Accepted, "Discord").is_ok());

        let rejected = refuse_unless_accepted(ProbeVerdict::Rejected("HTTP 401".into()), "Discord")
            .expect_err("a rejected token must not be saved");
        assert_eq!(rejected.0, StatusCode::BAD_REQUEST);

        let inconclusive =
            refuse_unless_accepted(ProbeVerdict::Inconclusive("dns failure".into()), "Slack")
                .expect_err("an unchecked token must not be saved either");
        assert_eq!(inconclusive.0, StatusCode::BAD_REQUEST);
        let rendered = format!("{:?}", inconclusive.1).to_lowercase();
        // A phrase, not the bare word: the message says "it was not rejected",
        // which contains "rejected" while asserting the opposite. What must not
        // appear is the rejection sentence itself.
        assert!(
            !rendered.contains("rejected the bot token"),
            "an offline probe must not be reported as a rejection: {rendered}"
        );
        assert!(
            rendered.contains("could not check"),
            "an inconclusive probe must say the check could not be made: {rendered}"
        );
    }

    /// VULN-001 from the security review. Presence in the request is not a
    /// change: a console that resends the saved `guild_id` or `channel_id` must
    /// not bounce the daemon, because the daemon hosts this gateway and a
    /// handful of no-op saves trips systemd's start limit and leaves the unit
    /// `failed`, taking the endpoint down with it.
    #[test]
    fn resending_an_unchanged_option_does_not_restart_the_runtime() {
        let before = discord_config("saved-token");
        let unchanged =
            apply_discord_update(Some(before.clone()), None, vec!["U_NEW".into()], Some("G1"))
                .expect("apply");
        assert!(
            !discord_restart_needed(false, Some(&before), &unchanged),
            "the same guild_id came back in the body, so nothing changed"
        );

        let moved =
            apply_discord_update(Some(before.clone()), None, vec![], Some("G2")).expect("apply");
        assert!(
            discord_restart_needed(false, Some(&before), &moved),
            "a different guild_id cannot reach a running channel"
        );

        let slack_before = slack_config("xoxb-saved");
        let slack_unchanged = apply_slack_update(
            Some(slack_before.clone()),
            None,
            vec![],
            Some("xapp-1-AAA"),
            Some("C1"),
        )
        .expect("apply");
        assert!(
            !slack_restart_needed(false, Some(&slack_before), &slack_unchanged),
            "the same app token and channel came back, so nothing changed"
        );

        let slack_moved =
            apply_slack_update(Some(slack_before.clone()), None, vec![], None, Some("C2"))
                .expect("apply");
        assert!(
            slack_restart_needed(false, Some(&slack_before), &slack_moved),
            "a different channel filter needs the listener rebuilt"
        );
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
        )
        .unwrap();
        assert_eq!(cfg.api_key.as_deref(), Some("sk-test"));
        assert_eq!(cfg.api_url.as_deref(), Some("https://api.example.com"));
        assert!(api_key_present(&cfg));

        apply_secrets(
            &mut cfg,
            &SecretsBody {
                api_key: Some(String::new()),
                api_url: Some("   ".into()),
            },
        )
        .unwrap();
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
        )
        .unwrap();
        assert_eq!(
            cfg.api_key.as_deref(),
            Some("keep-me"),
            "an omitted api_key must not wipe the existing key"
        );
        assert_eq!(cfg.api_url.as_deref(), Some("http://override"));
    }

    // ── api_url validation ───────────────────────────────────────
    //
    // `api_url` is persisted to config.toml in plaintext while `api_key` is
    // encrypted, and it is later consumed as a base URL by the model probe. A
    // credential arriving in this field is therefore stored unprotected and
    // interpolated into operator-facing errors. Reject it at the boundary.

    #[test]
    fn apply_secrets_rejects_credential_in_api_url_without_mutating_config() {
        let mut cfg = Config::default();
        cfg.default_provider = Some("openrouter".into());
        cfg.api_key = Some("existing-key".into());
        cfg.api_url = Some("https://openrouter.ai/api/v1".into());

        let err = apply_secrets(
            &mut cfg,
            &SecretsBody {
                api_key: Some("new-key".into()),
                api_url: Some("sk-or-v1-EXAMPLE".into()),
            },
        )
        .expect_err("a credential-shaped api_url must be rejected");
        assert!(err.contains("API key"), "unexpected message: {err}");

        // Validation runs before any mutation, so a rejected body leaves the
        // whole config untouched — including the api_key in the same request.
        assert_eq!(cfg.api_key.as_deref(), Some("existing-key"));
        assert_eq!(cfg.api_url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert!(cfg.provider_api_keys.is_empty());
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

    /// The write guard is not enough on its own: configs written before it
    /// existed still hold a credential here, and the web console renders
    /// `api_url` into a plain-text (non-password) input, so returning it puts
    /// the key on screen.
    #[test]
    fn secrets_view_withholds_a_credential_shaped_api_url() {
        let mut cfg = Config::default();
        cfg.default_provider = Some("openrouter".into());
        cfg.api_url = Some("sk-or-v1-EXAMPLE".into());

        let view = secrets_view(&cfg);

        assert_eq!(view["api_url"], serde_json::Value::Null);
        assert!(
            !view.to_string().contains("sk-or-v1-EXAMPLE"),
            "GET /secrets must not echo a credential stored in api_url"
        );
    }

    /// A typo is not a secret. Withholding it would leave the operator staring
    /// at an empty field with no way to see or correct what is stored.
    #[test]
    fn secrets_view_returns_a_malformed_api_url_so_it_can_be_corrected() {
        let mut cfg = Config::default();
        cfg.api_url = Some("not-a-url".into());

        assert_eq!(secrets_view(&cfg)["api_url"], "not-a-url");
    }

    // ── WhatsApp Web (plan 373) ──

    struct UnusedProvider;

    #[async_trait::async_trait]
    impl crate::providers::Provider for UnusedProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            anyhow::bail!("these tests make no model call")
        }
    }

    /// A gateway running `config`, with pairing off as on a local console.
    fn console_state(config: Config) -> AppState {
        AppState {
            config: Arc::new(parking_lot::Mutex::new(config)),
            config_fingerprint: Arc::new(parking_lot::Mutex::new("test".to_string())),
            provider: Arc::new(UnusedProvider),
            model: "test-model".into(),
            temperature: 0.0,
            mem: Arc::new(crate::memory::NoneMemory::new()),
            auto_save: false,
            tools_factory: Arc::new(|_: &Config| Vec::new()),
            webhook_secret_hash: None,
            pairing: Arc::new(crate::security::pairing::PairingGuard::new(false, &[])),
            trust_forwarded_headers: false,
            rate_limiter: Arc::new(crate::gateway::GatewayRateLimiter::new(100, 100, 100, 100)),
            idempotency_store: Arc::new(crate::gateway::IdempotencyStore::new(
                std::time::Duration::from_mins(5),
                1000,
            )),
            whatsapp: None,
            whatsapp_app_secret: None,
            linq: None,
            linq_signing_secret: None,
            nextcloud_talk: None,
            nextcloud_talk_webhook_secret: None,
            whatsapp_pair_guard: PairGuard::default(),
            observer: Arc::new(crate::observability::NoopObserver),
            webhook_routes: Arc::new(Vec::new()),
            channel_bus: Arc::new(crate::channels::ChannelBus::default()),
            ledger: None,
            web_approvals: Arc::new(crate::security::PendingApprovals::default()),
            mcp: Arc::new(crate::mcp::discover::McpPoolHandle::default()),
        }
    }

    /// A running config under `root` with WhatsApp Web linked. Nothing is on
    /// disk yet, so the first write saves this config to `root`.
    fn linked_config(root: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.config_path = root.join("config.toml");
        config.workspace_dir = root.join("workspace");
        config.channels_config.whatsapp_web = Some(crate::config::WhatsAppWebConfig {
            session_path: root
                .join("workspace")
                .join("whatsapp-1700000000.db")
                .display()
                .to_string(),
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["+15550000000".into()],
        });
        config
    }

    /// F-39. The runtime compares an entry with the sender's `+` form for
    /// exact equality, and the handler only trimmed, so a number typed without
    /// the `+` was saved as an allowlist that matched nobody.
    #[tokio::test]
    async fn whatsapp_web_allowlist_is_saved_in_the_form_the_runtime_compares() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp root");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let state = console_state(linked_config(tmp.path()));

        let saved = Box::pin(whatsapp_web_connect(
            State(state.clone()),
            HeaderMap::new(),
            Json(WhatsAppWebConnectBody {
                allowed_numbers: vec![
                    "15551234567".into(),
                    " +15557654321 ".into(),
                    "*".into(),
                    "lid:200000000000001".into(),
                    "  ".into(),
                ],
            }),
        ))
        .await;

        assert!(saved.is_ok(), "a list of numbers must be saved");
        let running = state
            .config
            .lock()
            .channels_config
            .whatsapp_web
            .clone()
            .expect("still linked");
        assert_eq!(
            running.allowed_numbers,
            vec!["+15551234567", "+15557654321", "*", "lid:200000000000001"],
            "each entry is saved the way the runtime compares it"
        );
    }

    /// F-39. An entry that could never match anyone is refused with the entry
    /// named, and nothing is saved.
    #[tokio::test]
    async fn whatsapp_web_allowlist_refuses_an_entry_that_is_not_a_number() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp root");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let state = console_state(linked_config(tmp.path()));

        let refused = Box::pin(whatsapp_web_connect(
            State(state.clone()),
            HeaderMap::new(),
            Json(WhatsAppWebConnectBody {
                allowed_numbers: vec!["+15551234567".into(), "1555-01x".into()],
            }),
        ))
        .await;

        let Err((status, Json(body))) = refused else {
            panic!("an entry that is not a number must be refused");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("1555-01x"),
            "the refusal names the entry: {body}"
        );
        let running = state
            .config
            .lock()
            .channels_config
            .whatsapp_web
            .clone()
            .expect("still linked");
        assert_eq!(
            running.allowed_numbers,
            vec!["+15550000000"],
            "nothing is saved"
        );
    }

    /// F-39. The pair request's list is saved on `Connected`, so it is checked
    /// by the same rule, and first: before the "already linked" check and
    /// before a pairing starts. This state is linked, so a handler that skips
    /// the check answers 409 and still opens no WhatsApp connection.
    #[cfg(feature = "whatsapp-web")]
    #[tokio::test]
    async fn whatsapp_web_pairing_refuses_an_entry_that_is_not_a_number_before_it_starts() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp root");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let state = console_state(linked_config(tmp.path()));

        let refused = whatsapp_web_pair(
            State(state.clone()),
            HeaderMap::new(),
            Json(WhatsAppWebConnectBody {
                allowed_numbers: vec!["1555-01x".into()],
            }),
        )
        .await;

        let Err((status, Json(body))) = refused else {
            panic!("an entry that is not a number must be refused");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("1555-01x"),
            "the refusal names the entry: {body}"
        );
        assert!(
            !state.whatsapp_pair_guard.load(Ordering::SeqCst),
            "a refused request leaves no pairing marked in progress"
        );
    }

    /// F-37. After a link the gateway saved the session and never scheduled
    /// the reload, so the channel stayed down until someone restarted the
    /// daemon. The reload runs once the save succeeds, and never without it.
    #[cfg(feature = "whatsapp-web")]
    #[tokio::test]
    async fn a_linked_session_restarts_the_runtime_only_once_it_is_saved() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp root");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let session = tmp.path().join("workspace").join("whatsapp-1700000001.db");

        let mut unlinked = linked_config(tmp.path());
        unlinked.channels_config.whatsapp_web = None;
        let state = console_state(unlinked);
        let reloads = std::sync::atomic::AtomicUsize::new(0);
        let frame = finish_pairing(&state, session.clone(), vec!["+15551234567".into()], || {
            reloads.fetch_add(1, Ordering::SeqCst);
        })
        .await;
        assert_eq!(frame["type"], "connected", "{frame}");
        assert_eq!(frame["restarts_runtime"], true, "{frame}");
        assert_eq!(
            reloads.load(Ordering::SeqCst),
            1,
            "a saved session must restart the runtime"
        );
        let saved = state
            .config
            .lock()
            .channels_config
            .whatsapp_web
            .clone()
            .expect("the session is saved");
        assert_eq!(saved.allowed_numbers, vec!["+15551234567"]);

        let broken = tmp.path().join("broken");
        std::fs::create_dir_all(&broken).expect("broken dir");
        std::fs::write(broken.join("config.toml"), "this is not = [ toml").expect("broken config");
        let mut unreadable = Config::default();
        unreadable.config_path = broken.join("config.toml");
        let state = console_state(unreadable);
        let reloads = std::sync::atomic::AtomicUsize::new(0);
        let frame = finish_pairing(&state, session, Vec::new(), || {
            reloads.fetch_add(1, Ordering::SeqCst);
        })
        .await;
        assert_eq!(frame["type"], "failed", "{frame}");
        assert_eq!(
            reloads.load(Ordering::SeqCst),
            0,
            "a session that was not saved must not restart anything"
        );
    }

    /// `finish_pairing` takes the reload as a parameter so the test above can
    /// count it; the pair stream must hand it the real one.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn the_pair_stream_restarts_the_runtime_through_finish_pairing() {
        let src = include_str!("config_api.rs");
        let handler = src
            .split("async fn whatsapp_web_pair(")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("the pair handler");
        assert!(
            handler.contains("finish_pairing(") && handler.contains("schedule_daemon_reload,"),
            "the Connected arm must pass schedule_daemon_reload to finish_pairing"
        );
    }
}
