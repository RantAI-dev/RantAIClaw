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
//! `RANTAICLAW_CONFIG_DIR` env var, because the config-api handlers persist
//! via `Config::load_or_init()` / `cfg.save()` — resolved from that env var,
//! not from `state.config` (see `lock_and_load`/`persist_and_swap` in
//! `src/gateway/config_api.rs`). Run this binary single-threaded:
//! `cargo test --test config_api -- --test-threads=1` (env var is
//! process-global; concurrent test threads would race on it).

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
    let (_state, router) = build_gateway_router(config)
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

    std::env::remove_var("RANTAICLAW_CONFIG_DIR");
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
