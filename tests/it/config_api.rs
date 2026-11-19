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
//! restores them on drop so a panicking test cannot leak an override into the
//! next one. `crate::test_env::ENV_LOCK` is not reachable from an integration
//! binary, and does not need to be: the race here is between siblings in this
//! process — and the binary's one shared `crate::common::ENV_LOCK` is what
//! serialises them against every other file's env mutation too.

// The binary's one process-wide env-mutation lock. Shared with every other
// module of this binary, because `std::env::set_var` is process-global: a
// per-file lock would not stop a sibling in another file from clobbering the
// variable mid-test.
use crate::common::ENV_LOCK;

/// Serialises the tests that set process-global config-resolution env vars,
/// and restores them on drop — including on unwind.
pub(crate) struct EnvGuard {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    pub(crate) async fn acquire() -> Self {
        let guard = ENV_LOCK.lock().await;
        // Start from a known state: a previous panicking test may have leaked.
        let prev = CONFIG_ENV_VARS
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        clear_config_env();
        Self { _lock: guard, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.prev.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// The process-global variables these tests own.
pub(crate) const CONFIG_ENV_VARS: [&str; 5] = [
    "RANTAICLAW_CONFIG_DIR",
    "KB_EMBEDDING_BASE_URL",
    "KB_EMBEDDING_API_KEY",
    "KB_EMBEDDING_DIM",
    "OPENROUTER_API_KEY",
];

fn clear_config_env() {
    for key in CONFIG_ENV_VARS {
        std::env::remove_var(key);
    }
}

use rantaiclaw::config::Config;
use rantaiclaw::gateway::build_gateway_router;

/// Deterministic bearer token paired into every test gateway. Not a real
/// credential — used only to exercise the auth-gated/hermetic code paths.
pub(crate) const TEST_TOKEN: &str = "test-not-a-real-token";

/// A minimal, hermetic `Config` rooted at a temp workspace: pairing required
/// with a single deterministic token, default (sqlite) memory backend under
/// the temp dir, and a provider that resolves no credential in this test
/// environment — so `build_gateway_router` builds fully offline.
pub(crate) fn test_config(workspace: &std::path::Path) -> Config {
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
pub(crate) async fn spawn_test_gateway(config: Config) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding 127.0.0.1:0 should succeed");
    let port = listener
        .local_addr()
        .expect("bound listener should have a local addr")
        .port();
    let (_state, router) = build_gateway_router(config, None, None)
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

/// Plan 319: the endpoint publishes the whole catalog, with a tier per row.
///
/// A client previously could not learn that a channel *existed* — only that one
/// was configured — which is why the web console shipped a second, hand-typed
/// copy of the catalog. This asserts the whole surface a client sees: every row,
/// every field, the tier vocabulary, and that the pre-existing `configured` and
/// `count` fields still mean what they meant.
#[tokio::test]
async fn get_channels_publishes_the_catalog_with_both_axes() {
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

    let channels = body["channels"]
        .as_array()
        .unwrap_or_else(|| panic!("`channels` must be an array, got: {body}"));
    // Pinned, like the counts in `mod_tests.rs` and `check_channel_maturity.sh`:
    // `CHANNEL_CATALOG` is the source and this number moves with it. 17 since
    // schema v32 split WhatsApp Cloud API and WhatsApp Web onto their own rows.
    assert_eq!(
        channels.len(),
        17,
        "every catalog channel must be published, got: {body}"
    );
    assert!(
        channels
            .iter()
            .any(|c| c["key"] == "whatsapp_web" && c["label"] == "WhatsApp Web"),
        "the split gave WhatsApp Web its own row; the API must publish it: {body}"
    );

    let mut supported = 0;
    let mut driven = 0;
    for entry in channels {
        for field in [
            "key",
            "label",
            "support",
            "maturity",
            "verification",
            "configured",
        ] {
            assert!(
                entry.get(field).is_some(),
                "a channel entry is missing `{field}`: {entry}"
            );
        }
        match entry["support"].as_str() {
            Some("supported") => supported += 1,
            Some("under_development") => {}
            // The one row the gateway serves itself. It sits on neither rung,
            // so no other key may carry it.
            Some("gateway_served") => assert_eq!(
                entry["key"], "webhook",
                "only the webhook row is served by the gateway: {entry}"
            ),
            other => panic!("unknown support value {other:?} in {entry}"),
        }
        match entry["verification"].as_str() {
            Some("driven") => driven += 1,
            Some("not_driven") => {}
            other => panic!("unknown verification value {other:?} in {entry}"),
        }
        assert_eq!(
            entry["maturity"], entry["support"],
            "`maturity` is the deprecated alias for `support`; they must never \
             diverge on the wire: {entry}"
        );
        assert!(entry["configured"].is_boolean(), "in {entry}");
    }
    // Counted separately, because they are separate facts. One assertion over
    // both would pass while a channel moved from one axis to the other.
    assert_eq!(
        supported, 6,
        "the support axis is a product commitment, not a refactor: {body}"
    );
    assert_eq!(
        driven, 5,
        "the verification axis moves only when a round trip was run: {body}"
    );

    // The combination the split exists for: committed to, never driven. Three
    // channels left this set on 2026-09-12, the only way a row may leave it:
    // somebody drove them. If it is ever empty, check whether somebody drove the
    // last one or somebody demoted it.
    let committed_undriven: Vec<&str> = channels
        .iter()
        .filter(|e| e["support"] == "supported" && e["verification"] == "not_driven")
        .map(|e| e["key"].as_str().expect("key is a string"))
        .collect();
    assert_eq!(
        committed_undriven,
        vec!["whatsapp"],
        "supported-but-undriven is a legitimate state and must survive: {body}"
    );

    // The compatibility half: the old fields still mean what they meant.
    let from_catalog: Vec<&str> = channels
        .iter()
        .filter(|e| e["configured"] == serde_json::Value::Bool(true))
        .map(|e| e["key"].as_str().expect("key is a string"))
        .collect();
    let configured: Vec<&str> = body["configured"]
        .as_array()
        .expect("`configured` must still be an array")
        .iter()
        .map(|k| k.as_str().expect("key is a string"))
        .collect();
    assert_eq!(
        configured, from_catalog,
        "`configured` and `channels[].configured` must not disagree: {body}"
    );
    assert_eq!(
        body["count"]
            .as_u64()
            .expect("`count` must still be a number"),
        configured.len() as u64
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

/// The gateway resolves the embedding key the way `kb status` does, so a key
/// that only `OPENROUTER_API_KEY` provides is reported as configured.
#[tokio::test]
async fn knowledge_get_reports_openrouter_env_key_as_embedding_source() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    std::env::set_var("OPENROUTER_API_KEY", "rantaiclaw_test_key");
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;

    let got: serde_json::Value = reqwest::Client::new()
        .get(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(got["source"], "openrouter_env", "{got}");
    assert_eq!(got["embedding_configured"], true, "{got}");
}

/// A key that only `OPENROUTER_API_KEY` provides is a key: PUT activates with
/// it, probes with it, and answers `embedding_configured` the way GET does.
#[tokio::test]
async fn knowledge_put_activates_with_an_openrouter_env_key_and_agrees_with_get() {
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let embed = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer rantaiclaw_openrouter_key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "embedding": [0.1, 0.2] } ]
        })))
        .expect(1)
        .mount(&embed)
        .await;

    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    std::env::set_var("KB_EMBEDDING_BASE_URL", embed.uri());
    // The probe checks the vector size the knowledge base expects, so the
    // fake endpoint answers with a vector of that size.
    std::env::set_var("KB_EMBEDDING_DIM", "2");
    std::env::set_var("OPENROUTER_API_KEY", "rantaiclaw_openrouter_key");
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;
    let client = reqwest::Client::new();

    let resp = client
        .put(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "enabled": true }))
        .send()
        .await
        .expect("request");
    let status = resp.status();
    let put: serde_json::Value = resp.json().await.expect("json");
    assert_eq!(status, reqwest::StatusCode::OK, "{put}");
    assert_eq!(put["enabled"], true, "{put}");
    assert_eq!(put["embedding_configured"], true, "{put}");

    let got: serde_json::Value = client
        .get(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(
        got["embedding_configured"], put["embedding_configured"],
        "GET and PUT must agree: get={got} put={put}"
    );
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

/// The strict probe refuses an embedding endpoint that answers with vectors of
/// a size the knowledge base does not expect: `kb enable` refuses it, so the
/// console must refuse it too. A key saved here would fail every later
/// knowledge-base call with a dimension mismatch.
#[tokio::test]
async fn knowledge_put_refuses_an_embedding_endpoint_with_the_wrong_vector_size() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let embed = MockServer::start().await;
    // Two dimensions, while the knowledge base expects `KB_EMBEDDING_DIM`.
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
    std::env::set_var("KB_EMBEDDING_DIM", "16");
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;

    let resp = reqwest::Client::new()
        .put(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({
            "embedding_api_key": "rantaiclaw_good_key",
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
    assert!(
        detail.contains(
            "the embedding endpoint returns 2-dimension vectors but KB_EMBEDDING_DIM is 16"
        ),
        "the refusal names both sizes: {body}"
    );
    assert!(
        !detail.contains("rantaiclaw_good_key"),
        "key must never surface: {body}"
    );
    assert_eq!(
        got["embedding_configured"], false,
        "a refused key must not persist: {got}"
    );
    assert_eq!(got["enabled"], false, "{got}");
}

/// An embedding endpoint that cannot be reached is refused too. The strict
/// probe cannot tell a working endpoint from a dead one, so the console does
/// not write a key it cannot use — the same answer `kb enable` gives.
#[tokio::test]
async fn knowledge_put_refuses_an_unreachable_embedding_endpoint() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    // A port nothing listens on: the listener is bound, its port is taken and
    // the listener is dropped again, so the port is closed.
    let closed_port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    };
    std::env::set_var(
        "KB_EMBEDDING_BASE_URL",
        format!("http://127.0.0.1:{closed_port}/v1/embeddings"),
    );
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;

    let resp = reqwest::Client::new()
        .put(format!("{base_url}/api/v1/config/knowledge"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({
            "embedding_api_key": "rantaiclaw_good_key",
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
    assert!(
        detail.contains("could not reach the embedding endpoint"),
        "the refusal says why: {body}"
    );
    assert!(
        !detail.contains("rantaiclaw_good_key"),
        "key must never surface: {body}"
    );
    assert_eq!(
        got["embedding_configured"], false,
        "an unreachable endpoint must not persist the key: {got}"
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
    // The probe checks the vector size the knowledge base expects, so the
    // fake endpoint answers with a vector of that size.
    std::env::set_var("KB_EMBEDDING_DIM", "2");
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
