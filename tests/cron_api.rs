//! Integration tests for the Cron API (`/api/v1/cron*`).
//!
//! Uses the `build_gateway_router` seam (`src/gateway/mod.rs`) to run a real
//! gateway on an OS-assigned port (`127.0.0.1:0`) against a hermetic,
//! temp-workspace `Config`. Cron jobs live in the per-profile sqlite store
//! under `workspace_dir`, NOT in `config.toml`, so — unlike `config_api` — these
//! tests do NOT set the process-global `RANTAICLAW_CONFIG_DIR` and can run
//! multi-threaded. Covers: `check_auth` gating, a create/list/update/delete
//! roundtrip, and not-found mapping.

use rantaiclaw::config::Config;
use rantaiclaw::gateway::build_gateway_router;

/// Deterministic bearer token paired into every test gateway. Not a real
/// credential — used only to exercise the auth-gated/hermetic code paths.
const TEST_TOKEN: &str = "test-not-a-real-token";

/// A minimal, hermetic `Config` rooted at a temp workspace: pairing required
/// with a single deterministic token, so `build_gateway_router` builds fully
/// offline.
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
/// (OS-assigned port). Returns the base URL clients should hit.
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
async fn cron_requires_auth() {
    let ws = tempfile::tempdir().unwrap();
    let base = spawn_test_gateway(test_config(ws.path())).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/api/v1/cron"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cron_list_create_delete_roundtrip() {
    let ws = tempfile::tempdir().unwrap();
    let base = spawn_test_gateway(test_config(ws.path())).await;
    let client = reqwest::Client::new();

    // Empty to start.
    let list: serde_json::Value = client
        .get(format!("{base}/api/v1/cron"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["count"], 0);

    // Create an agent job.
    let created: serde_json::Value = client
        .post(format!("{base}/api/v1/cron"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "prompt": "Good morning"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["job_type"], "agent");

    // It shows up in the list.
    let list: serde_json::Value = client
        .get(format!("{base}/api/v1/cron"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["count"], 1);

    // Disable via PUT.
    let updated: serde_json::Value = client
        .put(format!("{base}/api/v1/cron/{id}"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "enabled": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated["enabled"], false);

    // Delete.
    let del: serde_json::Value = client
        .delete(format!("{base}/api/v1/cron/{id}"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(del["deleted"], true);
}

#[tokio::test]
async fn cron_get_missing_job_runs_returns_empty() {
    let ws = tempfile::tempdir().unwrap();
    let base = spawn_test_gateway(test_config(ws.path())).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/api/v1/cron/does-not-exist/runs"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["count"], 0);
}

#[tokio::test]
async fn cron_update_missing_job_returns_404() {
    let ws = tempfile::tempdir().unwrap();
    let base = spawn_test_gateway(test_config(ws.path())).await;
    let resp = reqwest::Client::new()
        .put(format!("{base}/api/v1/cron/nope"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "enabled": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}
