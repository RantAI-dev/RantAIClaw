//! Integration tests for the Live Config API (`/api/v1/config*`).
//!
//! Uses the `build_gateway_router` seam (`src/gateway/mod.rs`) to run a real
//! gateway on an OS-assigned port (`127.0.0.1:0`) against a hermetic,
//! temp-workspace `Config` — no real channels/MCP servers are started, and
//! provider/memory construction is offline (synchronous factories, no
//! network at construction time). Covers the two properties a regression
//! could silently break without any test noticing: `check_auth` gating on
//! every route, and secret redaction on `GET /api/v1/config`.
//!
//! Mutation tests (PUT/POST) additionally set the process-global
//! `RANTAICLAW_CONFIG_DIR` env var. That USED to be load-bearing: the handlers
//! persisted via `Config::load_or_init()`, which re-resolves the path from the
//! environment rather than using the one the gateway booted with. They now read
//! `state.config`'s own `config_path` (see `load_running_config` in
//! `src/gateway/config_api.rs`), so the env var no longer selects which file a
//! write lands in. It is still set here because `cfg.save()` and the session
//! store resolve other per-profile paths from the environment.
//!
//! That race is now **enforced, not requested**. This header used to ask for
//! `--test-threads=1`; CI runs plain `cargo test`, so the instruction was
//! documentation that nothing applied, and
//! `knowledge_accepted_key_activates_and_deactivate_keeps_it` failed
//! intermittently when a sibling replaced `RANTAICLAW_CONFIG_DIR` mid-test.
//! Every test that touches those variables takes [`EnvGuard`], which also
//! clears them on drop so a panicking test cannot leak an override into the
//! next one. `crate::test_env::ENV_LOCK` is not reachable from an integration
//! binary, and does not need to be: the race here is between siblings in this
//! process.

/// Serialises the tests that set process-global config-resolution env vars,
/// and clears them on drop — including on unwind.
static CONFIG_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct EnvGuard(#[allow(dead_code)] tokio::sync::MutexGuard<'static, ()>);

impl EnvGuard {
    async fn acquire() -> Self {
        let guard = CONFIG_ENV_LOCK.lock().await;
        // Start from a known state: a previous panicking test may have leaked.
        clear_config_env();
        Self(guard)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        clear_config_env();
    }
}

fn clear_config_env() {
    std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    std::env::remove_var("KB_EMBEDDING_BASE_URL");
}

use rantaiclaw::config::Config;
use rantaiclaw::gateway::build_gateway_router;

/// Deterministic bearer token paired into every test gateway. Not a real
/// credential — used only to exercise the auth-gated/hermetic code paths.
const TEST_TOKEN: &str = "test-not-a-real-token";

/// A minimal, hermetic `Config` rooted at a temp workspace: pairing required
/// with a single deterministic token, default (sqlite) memory backend under
/// the temp dir, and a provider that resolves no credential in this test
/// environment — so `build_gateway_router` builds fully offline.
fn test_config(workspace: &std::path::Path) -> Config {
    let mut cfg = Config {
        workspace_dir: workspace.to_path_buf(),
        config_path: workspace.join("config.toml"),
        ..Config::default()
    };
    cfg.gateway.require_pairing = true;
    cfg.gateway.paired_tokens = vec![TEST_TOKEN.to_string()];
    cfg
}

/// Build the gateway router from `config` and serve it on `127.0.0.1:0`
/// (OS-assigned port). Returns the base URL clients should hit. The server
/// task lives for the remainder of the current tokio runtime, which
/// `#[tokio::test]` tears down at the end of each test.
async fn spawn_test_gateway(config: Config) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding 127.0.0.1:0 should succeed");
    let port = listener
        .local_addr()
        .expect("bound listener should have a local addr")
        .port();
    let (_state, router) = build_gateway_router(config, None)
        .expect("build_gateway_router should build offline from a temp-workspace config");
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("test gateway should serve without error");
    });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn get_config_without_auth_returns_401() {
    let workspace = tempfile::tempdir().expect("tempdir creation should succeed");
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;

    let resp = reqwest::Client::new()
        .get(format!("{base_url}/api/v1/config"))
        .send()
        .await
        .expect("request should complete");

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "GET /api/v1/config without a bearer token must be rejected when require_pairing=true"
    );
}

#[tokio::test]
async fn get_config_with_auth_returns_200_json() {
    let workspace = tempfile::tempdir().expect("tempdir creation should succeed");
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;

    let resp = reqwest::Client::new()
        .get(format!("{base_url}/api/v1/config"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("request should complete");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("body should parse as JSON");
    assert!(
        body.is_object(),
        "GET /api/v1/config should return a JSON object, got: {body}"
    );
}

#[tokio::test]
async fn get_config_redacts_secrets() {
    let workspace = tempfile::tempdir().expect("tempdir creation should succeed");
    let mut cfg = test_config(workspace.path());
    // Seed a neutral placeholder secret the way a real deploy would: a
    // configured Telegram bot token. Never a real credential — see
    // CLAUDE.md §9.1.
    cfg.channels_config.telegram = Some(
        serde_json::from_value(serde_json::json!({
            "bot_token": "0000000000:PLACEHOLDER_NOT_A_REAL_TOKEN",
            "allowed_users": ["rantaiclaw_user"],
        }))
        .expect("TelegramConfig should deserialize from a minimal JSON object"),
    );
    let base_url = spawn_test_gateway(cfg).await;

    let resp = reqwest::Client::new()
        .get(format!("{base_url}/api/v1/config"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("request should complete");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let text = resp.text().await.expect("body should be readable");
    assert!(
        !text.contains("PLACEHOLDER_NOT_A_REAL_TOKEN"),
        "GET /api/v1/config must redact the Telegram bot token, got: {text}"
    );
}

#[tokio::test]
async fn put_model_with_auth_returns_200() {
    let workspace = tempfile::tempdir().expect("tempdir creation should succeed");
    let config_dir = tempfile::tempdir().expect("tempdir creation should succeed");
    // `set_model` persists via `Config::load_or_init()` / `cfg.save()`,
    // resolved from `RANTAICLAW_CONFIG_DIR` — not from `state.config` (see
    // module doc comment above). Point it at a scratch dir so this test
    // never touches a real config.toml. This binary MUST run
    // single-threaded (`--test-threads=1`): the env var is process-global.
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());

    let base_url = spawn_test_gateway(test_config(workspace.path())).await;

    let resp = reqwest::Client::new()
        .put(format!("{base_url}/api/v1/config/model"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "model": "anthropic/claude-sonnet-4.6" }))
        .send()
        .await
        .expect("request should complete");
    let status = resp.status();

    assert_eq!(status, reqwest::StatusCode::OK);
}

#[tokio::test]
async fn put_model_without_auth_returns_401() {
    let workspace = tempfile::tempdir().expect("tempdir creation should succeed");
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;

    let resp = reqwest::Client::new()
        .put(format!("{base_url}/api/v1/config/model"))
        .json(&serde_json::json!({ "model": "anthropic/claude-sonnet-4.6" }))
        .send()
        .await
        .expect("request should complete");

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "PUT /api/v1/config/model without a bearer token must be rejected"
    );
}

/// H1: the file's doc claims `check_auth` gates every route, but only two routes
/// had a 401 test. Cover every (method, path) at once. Each body-carrying route
/// gets a body its `Json<_>` extractor accepts (axum runs extractors before the
/// handler, so a bodyless PUT/POST is rejected by the extractor, not the auth
/// gate); the body only exists so `check_auth` is the thing that fails. The
/// high-consequence writes (`PUT /secrets`, `POST /mcp_servers`) are the point.
#[tokio::test]
async fn every_config_api_route_requires_auth() {
    use serde_json::json;
    let workspace = tempfile::tempdir().expect("tempdir creation should succeed");
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;
    let client = reqwest::Client::new();

    let mut routes: Vec<(reqwest::Method, &str, serde_json::Value)> = vec![
        (reqwest::Method::GET, "/api/v1/config", json!({})),
        (reqwest::Method::PUT, "/api/v1/config/model", json!({})),
        (reqwest::Method::PUT, "/api/v1/config/autonomy", json!({})),
        (reqwest::Method::GET, "/api/v1/secrets", json!({})),
        (reqwest::Method::PUT, "/api/v1/secrets", json!({})),
        // McpServerBody requires `command`; everything else is all-optional.
        (
            reqwest::Method::POST,
            "/api/v1/config/mcp_servers/probe",
            json!({ "command": "echo" }),
        ),
        (
            reqwest::Method::DELETE,
            "/api/v1/config/mcp_servers/probe",
            json!({}),
        ),
        (
            reqwest::Method::POST,
            "/api/v1/channels/telegram",
            json!({}),
        ),
        (
            reqwest::Method::DELETE,
            "/api/v1/channels/telegram",
            json!({}),
        ),
    ];
    #[cfg(feature = "kb")]
    {
        routes.push((reqwest::Method::GET, "/api/v1/config/knowledge", json!({})));
        routes.push((reqwest::Method::PUT, "/api/v1/config/knowledge", json!({})));
    }

    for (method, path, body) in routes {
        let resp = client
            .request(method.clone(), format!("{base_url}{path}"))
            .json(&body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{method} {path} request failed: {e}"));
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{method} {path} without a bearer token must be rejected"
        );
    }
}

/// The bypass side of the gate: when `require_pairing = false`, `check_auth`
/// returns Ok, so an unauthenticated request is NOT rejected with 401 (it runs
/// the handler and fails, if at all, for a different reason — a bad body, etc.).
#[tokio::test]
async fn require_pairing_false_bypasses_auth() {
    let workspace = tempfile::tempdir().expect("tempdir creation should succeed");
    let mut config = test_config(workspace.path());
    config.gateway.require_pairing = false;
    let base_url = spawn_test_gateway(config).await;

    let resp = reqwest::Client::new()
        .get(format!("{base_url}/api/v1/config"))
        .send()
        .await
        .expect("request should complete");

    assert_ne!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "with require_pairing=false, an unauthenticated GET must NOT be 401"
    );
}

#[tokio::test]
async fn get_channels_returns_200() {
    let workspace = tempfile::tempdir().expect("tempdir creation should succeed");
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;

    let resp = reqwest::Client::new()
        .get(format!("{base_url}/api/v1/channels"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("request should complete");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("body should parse as JSON");
    assert!(
        body.get("configured").is_some(),
        "GET /api/v1/channels should return a channel status map, got: {body}"
    );
}

// ── Plan 103: /config/knowledge carries `enabled`; key probed on activation ──

#[tokio::test]
async fn knowledge_activate_without_key_returns_400() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;

    let resp = reqwest::Client::new()
        .put(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "enabled": true }))
        .send()
        .await
        .expect("request");
    let status = resp.status();

    // Nothing persisted: GET still reports disabled.
    let got: serde_json::Value = reqwest::Client::new()
        .get(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(got["enabled"], false, "{got}");
}

#[tokio::test]
async fn knowledge_rejected_key_is_never_persisted() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let embed = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&embed)
        .await;

    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    std::env::set_var("KB_EMBEDDING_BASE_URL", embed.uri());
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;

    let resp = reqwest::Client::new()
        .put(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({
            "embedding_api_key": "rantaiclaw_wrong_key",
            "enabled": true
        }))
        .send()
        .await
        .expect("request");
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("json");

    let got: serde_json::Value = reqwest::Client::new()
        .get(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("401"), "status named: {body}");
    assert!(
        !detail.contains("rantaiclaw_wrong_key"),
        "key must never surface: {body}"
    );
    assert_eq!(
        got["embedding_configured"], false,
        "rejected key must not persist: {got}"
    );
    assert_eq!(got["enabled"], false, "{got}");
}

#[tokio::test]
async fn knowledge_accepted_key_activates_and_deactivate_keeps_it() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let embed = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "embedding": [0.1, 0.2] } ]
        })))
        .mount(&embed)
        .await;

    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    std::env::set_var("KB_EMBEDDING_BASE_URL", embed.uri());
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;
    let client = reqwest::Client::new();

    // Accepted key + enabled -> 200 with enabled true.
    let resp = client
        .put(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({
            "embedding_api_key": "rantaiclaw_good_key",
            "enabled": true
        }))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["enabled"], true, "{body}");
    assert_eq!(body["embedding_configured"], true, "{body}");

    // Deactivate: 200, and the key SURVIVES — deactivate is not delete;
    // this is the whole point of the feature (plan 102/103).
    let resp = client
        .put(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "enabled": false }))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(body["enabled"], false, "{body}");
    assert_eq!(
        body["embedding_configured"], true,
        "deactivating must keep the credential: {body}"
    );

    // GET reflects the deactivated-but-configured state.
    let got: serde_json::Value = client
        .get(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(got["enabled"], false, "{got}");
    assert_eq!(got["embedding_configured"], true, "{got}");
}
