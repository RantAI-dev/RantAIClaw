//! Integration tests for the gateway's provider-auth answers and its router.
//!
//! Two properties the console depends on, and no test covered them:
//!
//! - A route that calls a provider answers a rejected credential with
//!   `502 provider_auth_failed` — the same code, the same body and the same
//!   sentence as the api_v1 chat routes — and never with the upstream body or
//!   the key.
//! - A provider id that arrives percent-encoded reaches the handler as ONE
//!   decoded value. The console's provider routes address a `custom:` endpoint
//!   by URL, and that URL contains slashes; axum routes on the raw path and
//!   decodes each captured value once, so `%2F` stays inside the segment
//!   instead of splitting it.

use crate::config_api::{spawn_test_gateway, test_config, EnvGuard, TEST_TOKEN};

/// The memory store's embedding provider answers 401 to the key it was sent.
///
/// The route is `POST /api/v1/memory`: it stores a note and, when the
/// operator configured semantic search, embeds it. A rejected embedding
/// credential is the operator's to fix, so the route says which provider
/// refused it — with the code the console already knows.
#[tokio::test]
async fn a_memory_write_answers_502_when_the_embedding_provider_rejects_the_key() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let embed = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string("the upstream wrote a refusal, and it echoes the key"),
        )
        .mount(&embed)
        .await;

    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    let mut config = test_config(workspace.path());
    // The embedding endpoint the store talks to, and a key for it.
    config.memory.embedding_provider = format!("custom:{}", embed.uri());
    config.api_key = Some("rantaiclaw_test_key".to_string());
    let base_url = spawn_test_gateway(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/api/v1/memory"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "content": "a note about the lantern" }))
        .send()
        .await
        .expect("request");
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("json");

    assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"], "provider_auth_failed", "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("401"), "the status is named: {body}");
    assert!(
        !detail.contains("rantaiclaw_test_key"),
        "the key must never surface: {body}"
    );
    assert!(
        !detail.contains("the upstream wrote a refusal"),
        "the upstream body must never surface: {body}"
    );
}

/// A provider id that carries an encoded slash reaches the handler as ONE
/// value, decoded.
///
/// The request goes THROUGH the router — the handler is never called
/// directly — because the property is about the router: it routes on the raw
/// path and decodes each captured value once. The route refuses the
/// caller-chosen endpoint before any request, and the refusal names the id it
/// received, so the answer shows what the handler got.
#[tokio::test]
async fn a_provider_id_with_an_encoded_slash_reaches_the_handler_as_one_value() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;

    let resp = reqwest::Client::new()
        .post(format!(
            "{base_url}/api/v1/providers/custom:https%3A%2F%2Fhost%2Fv1/models/refresh"
        ))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("request");
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("json");

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("custom:https://host/v1"),
        "the handler got the decoded id as one value: {body}"
    );
    assert!(
        !detail.contains("%2F") && !detail.contains("%3A"),
        "the id was decoded, not left encoded: {body}"
    );
}

/// The memory control: an embedding provider that fails for a reason of its
/// own is not a rejected credential, so the write is not refused. The store
/// keeps the note without a vector, as it always did, and the route answers
/// `201`.
#[tokio::test]
async fn a_memory_write_keeps_storing_when_the_embedding_provider_fails_outright() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let embed = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(500).set_body_string("the embedding service is down"))
        .mount(&embed)
        .await;

    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    let mut config = test_config(workspace.path());
    config.memory.embedding_provider = format!("custom:{}", embed.uri());
    config.api_key = Some("rantaiclaw_test_key".to_string());
    let base_url = spawn_test_gateway(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/api/v1/memory"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "content": "a note about the lantern" }))
        .send()
        .await
        .expect("request");
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("json");

    assert_eq!(status, reqwest::StatusCode::CREATED, "{body}");
    assert!(
        !body.to_string().contains("provider_auth_failed"),
        "a non-auth failure is not a credential refusal: {body}"
    );
}

/// The provider id names an endpoint by URL, and the URL here carries
/// userinfo and a query string with secrets. The endpoint answers 401, and
/// neither the 502 body nor the log output may carry either secret.
///
/// The log check reads the output of the whole request, so `abc` alone is not
/// searched for there: a log line of hex ids contains it by chance. The
/// query is searched as `key=abc`.
#[tokio::test]
async fn provider_auth_error_never_echoes_custom_endpoint_userinfo_or_query() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let endpoint = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string("the upstream echoes the credential"),
        )
        .mount(&endpoint)
        .await;

    // The same endpoint, with userinfo and a query string the operator typed.
    let authority = endpoint.uri().trim_start_matches("http://").to_string();
    let provider_id = format!("custom:http://user:secret@{authority}/v1?key=abc");

    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _log_guard = tracing::subscriber::set_default(subscriber);

    let mut config = test_config(workspace.path());
    config.default_provider = Some(provider_id);
    config.default_model = Some("test-model".to_string());
    config.api_key = Some("rantaiclaw_test_key".to_string());
    let base_url = spawn_test_gateway(config).await;

    let resp = reqwest::Client::new()
        .post(format!("{base_url}/api/v1/agent/chat"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "message": "a question" }))
        .send()
        .await
        .expect("request");
    let status = resp.status();
    let body = resp.text().await.expect("body");

    assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("provider_auth_failed"), "{body}");
    assert!(body.contains("127.0.0.1"), "the endpoint is named: {body}");
    assert!(!body.contains("secret"), "the userinfo leaked: {body}");
    assert!(!body.contains("abc"), "the query leaked: {body}");

    let log_text = logs.text();
    assert!(
        log_text.contains("rejected the credential"),
        "the capture saw the refusal, so the check below is not vacuous: {log_text}"
    );
    assert!(
        !log_text.contains("secret"),
        "the userinfo leaked to the log: {log_text}"
    );
    assert!(
        !log_text.contains("key=abc"),
        "the query leaked to the log: {log_text}"
    );
}

/// A cron run of an agent job whose turn the provider rejects answers
/// `502 provider_auth_failed`, the same body the chat routes answer, and the
/// run history keeps the failed run.
///
/// The job retries once (`scheduler_retries = 1`), so the history holds one
/// row per attempt: two rows, both `error`. The retry policy is unchanged on
/// the manual path, and the count shows it.
#[tokio::test]
async fn a_cron_run_of_an_agent_job_answers_502_when_the_provider_rejects_the_key() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string("the upstream echoes the credential"),
        )
        .mount(&provider)
        .await;

    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    let mut config = test_config(workspace.path());
    config.default_provider = Some(format!("custom:{}", provider.uri()));
    config.default_model = Some("test-model".to_string());
    config.api_key = Some("rantaiclaw_test_key".to_string());
    config.reliability.scheduler_retries = 1;
    let base_url = spawn_test_gateway(config).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = client
        .post(format!("{base_url}/api/v1/cron"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "prompt": "summarise the lantern notes"
        }))
        .send()
        .await
        .expect("create")
        .json()
        .await
        .expect("json");
    let id = created["id"].as_str().expect("job id").to_string();

    let resp = client
        .post(format!("{base_url}/api/v1/cron/{id}/run"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("run");
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("json");

    assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"], "provider_auth_failed", "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("401"), "the status is named: {body}");
    assert!(
        !detail.contains("the upstream echoes"),
        "the upstream body must never surface: {body}"
    );
    assert!(
        !detail.contains("rantaiclaw_test_key"),
        "the key must never surface: {body}"
    );

    let runs: serde_json::Value = client
        .get(format!("{base_url}/api/v1/cron/{id}/runs"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("runs")
        .json()
        .await
        .expect("json");
    let runs = runs["runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 2, "one history row per attempt: {runs:?}");
    assert!(
        runs.iter().all(|run| run["status"] == "error"),
        "the failed run stays in the history as an error: {runs:?}"
    );
}

/// The control for the test above: an agent job that fails for a reason that
/// is not a rejected credential keeps the `200 {success:false, output}` answer.
/// No provider is configured, so the turn fails before any request is sent.
#[tokio::test]
async fn a_cron_run_of_an_agent_job_without_a_provider_key_answers_200_not_502() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let config_dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvGuard::acquire().await;
    std::env::set_var("RANTAICLAW_CONFIG_DIR", config_dir.path());
    let base_url = spawn_test_gateway(test_config(workspace.path())).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = client
        .post(format!("{base_url}/api/v1/cron"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "prompt": "summarise the lantern notes"
        }))
        .send()
        .await
        .expect("create")
        .json()
        .await
        .expect("json");
    let id = created["id"].as_str().expect("job id").to_string();

    let resp = client
        .post(format!("{base_url}/api/v1/cron/{id}/run"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("run");
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("json");

    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    assert_eq!(body["success"], false, "{body}");
    assert!(
        body["output"]
            .as_str()
            .unwrap_or_default()
            .contains("agent job failed:"),
        "the failure text is unchanged: {body}"
    );
    assert!(
        body.get("error").is_none(),
        "a non-auth failure has no error code: {body}"
    );
}

/// Output the gateway writes during a test, so a test can read the log lines.
#[derive(Clone, Default)]
struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl CapturedLogs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log lock")).into_owned()
    }
}

impl std::io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogs;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
