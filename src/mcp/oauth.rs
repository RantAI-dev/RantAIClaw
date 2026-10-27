//! Minimal local-listener OAuth flow for the curated MCP picker.
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`
//! §"Section 5 — mcp (NEW)" — bullet 3.
//!
//! The callback is bound to its login: the authorization URL carries a
//! one-time `state` and a PKCE `code_challenge` (S256), the token exchange
//! sends the matching `code_verifier`, and the loopback listener accepts a
//! `code` only from the callback whose `state` matches. Any other request —
//! a stray port scan, a favicon fetch, a different provider slug, a replayed
//! or forged callback — is answered with an error page and **ignored**: it
//! neither ends the login nor supplies the code. Only an `error=` parameter
//! carrying the right `state` ends the flow, because that is the identity
//! provider reporting that *this* login was refused.
//!
//! The endpoints are constants in production (`OAuthEndpoints::google()`).
//! Tests pass their own through `OAuthSession::with_client`; **no
//! environment variable or config key may redirect the authorization or
//! token request** — a redirectable exchange would turn any local env
//! change into a token thief.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use axum::extract::{Path as AxumPath, Query, State};
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tokio::sync::{oneshot, Mutex};

use super::curated::OAuthProvider;
use crate::auth::openai_oauth::{generate_pkce_state, PkceState};

#[cfg(test)]
use sha2::{Digest, Sha256};

/// Fixed listener port. Must match what's registered in upstream OAuth
/// app config (`http://localhost:11500/oauth/<provider-slug>`).
pub const OAUTH_PORT: u16 = 11500;

/// Hard cap on how long we wait for the user to complete the flow.
pub const OAUTH_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Every flow test binds the fixed loopback port, and `cargo test --lib` runs
/// unit tests in parallel threads of one process — so the flow tests
/// serialize on this lock. The provisioner tests that drive a full OAuth
/// flow take it too.
#[cfg(test)]
pub(crate) static OAUTH_PORT_LOCK: Mutex<()> = Mutex::const_new(());

/// Where the authorization and token requests go. A constant in production —
/// Google's real endpoints for every curated OAuth provider. Tests build their
/// own pointing at a local fake server; there is deliberately no runtime
/// override of any kind.
#[derive(Clone, Debug)]
pub(crate) struct OAuthEndpoints {
    pub(crate) auth_url: String,
    pub(crate) token_url: String,
}

impl OAuthEndpoints {
    pub(crate) fn google() -> Self {
        Self {
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
            token_url: "https://oauth2.googleapis.com/token".to_string(),
        }
    }
}

/// The names of the environment variables holding the OAuth client id and
/// secret for `provider`, so error messages can name the exact step that
/// finishes the setup.
pub(crate) fn client_env_names(provider: OAuthProvider) -> (&'static str, &'static str) {
    match provider {
        OAuthProvider::GoogleDrive | OAuthProvider::GoogleCalendar | OAuthProvider::Gmail => (
            "RANTAICLAW_GOOGLE_CLIENT_ID",
            "RANTAICLAW_GOOGLE_CLIENT_SECRET",
        ),
    }
}

/// One login: the PKCE pair and state this flow generated, the client
/// credentials it runs under, and the endpoints it talks to. Every check the
/// listener makes is against the values held here, which is what binds a
/// callback to its session.
pub(crate) struct OAuthSession {
    endpoints: OAuthEndpoints,
    provider: OAuthProvider,
    scopes: Vec<String>,
    redirect_uri: String,
    client_id: String,
    client_secret: String,
    pkce: PkceState,
}

impl OAuthSession {
    /// Start a login from the operator's environment against `endpoints`.
    /// The client id and secret must already be set; without them the flow
    /// cannot start, and the error names the exact missing variables and the
    /// command that finishes the setup.
    pub(crate) fn start_from_env(
        endpoints: OAuthEndpoints,
        provider: OAuthProvider,
        scopes: &[&str],
    ) -> Result<Self> {
        let (id_env, secret_env) = client_env_names(provider);
        let client_id = std::env::var(id_env).map_err(|_| {
            anyhow!(
                "missing {id_env}; set it (and {secret_env}) before running OAuth, \
                 then run `rantaiclaw setup mcp` again to finish the login \
                 — see docs/start/install.md"
            )
        })?;
        let client_secret = std::env::var(secret_env).map_err(|_| {
            anyhow!(
                "missing {secret_env}; set it (and {id_env}) before running OAuth, \
                 then run `rantaiclaw setup mcp` again to finish the login"
            )
        })?;
        Ok(Self::start_with(
            endpoints,
            provider,
            scopes,
            client_id,
            client_secret,
        ))
    }

    /// Start a login against `endpoints` with the given client credentials.
    pub(crate) fn start_with(
        endpoints: OAuthEndpoints,
        provider: OAuthProvider,
        scopes: &[&str],
        client_id: String,
        client_secret: String,
    ) -> Self {
        Self {
            endpoints,
            provider,
            scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            redirect_uri: format!("http://localhost:{}/oauth/{}", OAUTH_PORT, provider.slug()),
            client_id,
            client_secret,
            pkce: generate_pkce_state(),
        }
    }

    /// Start a login with explicit client credentials and endpoints.
    /// **Test-only** — a second production caller must not appear: the real
    /// flow reads the client credentials from the operator's environment
    /// ([`OAuthSession::start_from_env`]) and the endpoints from the constants, so
    /// nothing in a deployed binary can point the token exchange elsewhere.
    #[cfg(test)]
    pub(crate) fn with_client(
        endpoints: OAuthEndpoints,
        provider: OAuthProvider,
        scopes: &[&str],
        client_id: &str,
        client_secret: &str,
    ) -> Self {
        Self::start_with(
            endpoints,
            provider,
            scopes,
            client_id.to_string(),
            client_secret.to_string(),
        )
    }

    /// The URL the operator's browser opens. It carries this login's `state`
    /// and the S256 `code_challenge` derived from the verifier that only the
    /// token exchange will later see.
    pub(crate) fn authorization_url(&self) -> String {
        let scope = self.scopes.join(" ");
        format!(
            "{}?response_type=code&access_type=offline&prompt=consent&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
            self.endpoints.auth_url,
            urlencoding::encode(&self.client_id),
            urlencoding::encode(&self.redirect_uri),
            urlencoding::encode(&scope),
            urlencoding::encode(&self.pkce.state),
            urlencoding::encode(&self.pkce.code_challenge),
        )
    }

    /// Bind the loopback listener, wait for the one callback that belongs to
    /// this login (right path, right `state`), then exchange its code with
    /// the PKCE verifier. Everything else the listener receives is answered
    /// and ignored; the whole wait is capped at `timeout`.
    pub(crate) async fn finish(&self, timeout: Duration) -> Result<String> {
        let (code_tx, code_rx) = oneshot::channel::<Result<String>>();
        let server_handle = self.spawn_listener(code_tx).await?;

        let received = match tokio::time::timeout(timeout, code_rx).await {
            Ok(received) => received,
            Err(_) => {
                server_handle.shutdown().await;
                bail!(
                    "OAuth flow timed out after {}s — re-run `rantaiclaw setup mcp` to retry",
                    timeout.as_secs()
                );
            }
        };

        server_handle.shutdown().await;

        match received {
            Ok(Ok(code)) => self.exchange_code(&code).await,
            Ok(Err(e)) => Err(e),
            Err(_recv_err) => bail!("OAuth listener died before receiving redirect"),
        }
    }

    async fn spawn_listener(
        &self,
        code_tx: oneshot::Sender<Result<String>>,
    ) -> Result<ListenerHandle> {
        let state = Arc::new(ListenerState {
            expected_slug: self.provider.slug().to_string(),
            expected_state: self.pkce.state.clone(),
            sender: Mutex::new(Some(code_tx)),
        });

        let app = Router::new()
            .route("/oauth/{provider}", get(handle_redirect))
            .with_state(state);

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), OAUTH_PORT);
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind {addr} for OAuth listener"))?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let join = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        Ok(ListenerHandle {
            shutdown_tx: Some(shutdown_tx),
            join,
        })
    }

    async fn exchange_code(&self, code: &str) -> Result<String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .context("build OAuth token-exchange client")?;

        let params = [
            ("grant_type", "authorization_code"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("code", code),
            ("code_verifier", self.pkce.code_verifier.as_str()),
        ];

        let resp = client
            .post(&self.endpoints.token_url)
            .form(&params)
            .send()
            .await
            .context("POST OAuth token endpoint")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("token endpoint returned {status}: {body}");
        }

        let parsed: TokenResponse = resp
            .json()
            .await
            .context("parse token endpoint JSON response")?;
        Ok(parsed.access_token)
    }
}

/// Run the full OAuth flow: print the URL → open the browser → wait for the
/// bound callback → exchange the code with the PKCE verifier → return the
/// access token.
pub async fn run_oauth(provider: OAuthProvider, scopes: &[&str]) -> Result<String> {
    run_oauth_with(&OAuthEndpoints::google(), provider, scopes).await
}

/// [`run_oauth`] with the endpoints made a parameter — the seam tests use to
/// prove the flow against a local fake server. Production callers pass
/// [`OAuthEndpoints::google`] only.
pub(crate) async fn run_oauth_with(
    endpoints: &OAuthEndpoints,
    provider: OAuthProvider,
    scopes: &[&str],
) -> Result<String> {
    let session = OAuthSession::start_from_env(endpoints.clone(), provider, scopes)?;

    let url = session.authorization_url();
    eprintln!("Open this URL to authorize {}:\n  {url}", provider.slug());
    let _ = webbrowser::open(&url);

    session.finish(OAUTH_TIMEOUT).await
}

struct ListenerHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl ListenerHandle {
    /// Stop the listener and wait for it to release the port, so the next
    /// login (or the next test) can bind it.
    async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let join = self.join;
        if tokio::time::timeout(Duration::from_secs(2), join)
            .await
            .is_err()
        {
            // Server didn't honour graceful shutdown — the port may still be
            // held, and the next bind will say so.
        }
    }
}

struct ListenerState {
    expected_slug: String,
    expected_state: String,
    sender: Mutex<Option<oneshot::Sender<Result<String>>>>,
}

#[derive(Deserialize)]
struct OAuthCallback {
    code: Option<String>,
    error: Option<String>,
    state: Option<String>,
}

async fn handle_redirect(
    State(state): State<Arc<ListenerState>>,
    AxumPath(provider): AxumPath<String>,
    Query(params): Query<OAuthCallback>,
) -> Html<&'static str> {
    // A request for another provider's slug is a wrong door, not a refusal:
    // answering it and keeping the listener alive means a stray local process
    // (or page) cannot cancel a login it did not start.
    if provider != state.expected_slug {
        return Html(MISMATCH_PAGE);
    }

    // The `state` binds this callback to THIS login. Without a matching one,
    // nothing here may be delivered — no code, and no `error` either, or any
    // local page could cancel the login by hitting the listener first.
    let state_matches = params
        .state
        .as_deref()
        .is_some_and(|got| constant_time_eq(got, &state.expected_state));
    if !state_matches {
        return Html(MISMATCH_PAGE);
    }

    let result = match (params.code, params.error) {
        (Some(code), _) => Ok(code),
        (None, Some(err)) => Err(anyhow!("provider returned error: {err}")),
        // The right door with the right state but nothing in hand is a stray
        // request (a preconnect, a manual reload) — ignore it and keep waiting.
        (None, None) => return Html(MISMATCH_PAGE),
    };

    let page = if result.is_ok() {
        SUCCESS_PAGE
    } else {
        FAILURE_PAGE
    };
    if let Some(sender) = state.sender.lock().await.take() {
        let _ = sender.send(result);
    }

    Html(page)
}

/// Byte-for-byte equality whose running time does not depend on *where* the
/// first difference is. The listener runs on a local port any local process
/// can probe; a comparison that early-returns on the first differing byte
/// would let a local attacker recover the expected `state` one guess at a
/// time by timing responses.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        // Only the length leaks, which is not secret: every value this
        // compares is a fixed-length base64url string from the same generator.
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const SUCCESS_PAGE: &str = "<!doctype html>\
<html><head><title>RantaiClaw OAuth</title></head>\
<body style=\"font-family: system-ui; padding: 3rem; text-align: center;\">\
<h2>You're authenticated.</h2>\
<p>You can close this tab and return to your terminal.</p>\
</body></html>";

const FAILURE_PAGE: &str = "<!doctype html>\
<html><head><title>RantaiClaw OAuth</title></head>\
<body style=\"font-family: system-ui; padding: 3rem; text-align: center;\">\
<h2>Authorization failed.</h2>\
<p>Return to the terminal for the reason, and start the login again.</p>\
</body></html>";

const MISMATCH_PAGE: &str = "<!doctype html>\
<html><head><title>RantaiClaw OAuth</title></head>\
<body style=\"font-family: system-ui; padding: 3rem; text-align: center;\">\
<h2>This callback does not match an active login.</h2>\
<p>If you started a login, open the authorization URL from the terminal again. \
This request was ignored and the login is still waiting.</p>\
</body></html>";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::Json;
    use base64::Engine as _;
    use serde_json::json;

    /// Every form body POSTed to the fake token endpoint, in order.
    type RecordedForms = Arc<std::sync::Mutex<Vec<Vec<(String, String)>>>>;

    /// Records every form body POSTed to the fake token endpoint.
    #[derive(Clone, Default)]
    struct FakeTokenRecorder(RecordedForms);

    impl FakeTokenRecorder {
        fn forms(&self) -> Vec<Vec<(String, String)>> {
            self.0.lock().expect("fake token recorder").clone()
        }
    }

    /// `axum`'s `Form` extractor is not compiled into this crate, so the fake
    /// endpoint reads the urlencoded body itself.
    fn parse_form(body: &[u8]) -> Vec<(String, String)> {
        String::from_utf8_lossy(body)
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (
                    urlencoding::decode(key).unwrap_or_default().into_owned(),
                    urlencoding::decode(value).unwrap_or_default().into_owned(),
                )
            })
            .collect()
    }

    async fn spawn_fake_endpoints() -> (OAuthEndpoints, FakeTokenRecorder) {
        let recorder = FakeTokenRecorder::default();
        let app = Router::new()
            .route(
                "/token",
                post(
                    |State(rec): State<FakeTokenRecorder>, body: axum::body::Bytes| async move {
                        rec.0
                            .lock()
                            .expect("fake token recorder")
                            .push(parse_form(&body));
                        Json(json!({ "access_token": "fake-access-token" }))
                    },
                ),
            )
            .route("/authorize", get(|| async { "fake authorization page" }))
            .with_state(recorder.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake OAuth endpoints");
        let addr = listener.local_addr().expect("fake endpoint addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (
            OAuthEndpoints {
                auth_url: format!("http://{addr}/authorize"),
                token_url: format!("http://{addr}/token"),
            },
            recorder,
        )
    }

    fn fake_session(endpoints: OAuthEndpoints) -> OAuthSession {
        OAuthSession::with_client(
            endpoints,
            OAuthProvider::GoogleDrive,
            &["https://www.googleapis.com/auth/drive.readonly"],
            "fake-client-id",
            "fake-client-secret",
        )
    }

    /// Poll until the loopback listener accepts, so a test never races the
    /// bind that `finish` performs.
    async fn wait_for_listener() {
        for _ in 0..400 {
            if tokio::net::TcpStream::connect(("127.0.0.1", OAUTH_PORT))
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("OAuth listener never came up on port {OAUTH_PORT}");
    }

    fn query_param(url: &str, key: &str) -> String {
        url.split(['?', '&'])
            .find_map(|pair| pair.split_once('=').filter(|(k, _)| *k == key))
            .map(|(_, v)| v.to_string())
            .unwrap_or_else(|| panic!("no {key} in {url}"))
    }

    /// Start `finish` in the background and wait for the listener, so the
    /// test can play the browser against it.
    async fn run_flow_against_listener(
        session: OAuthSession,
        timeout: Duration,
    ) -> tokio::task::JoinHandle<Result<String>> {
        let handle = tokio::spawn(async move { session.finish(timeout).await });
        wait_for_listener().await;
        handle
    }

    async fn get_callback(path_and_query: &str) -> reqwest::Result<reqwest::Response> {
        reqwest::get(format!("http://127.0.0.1:{OAUTH_PORT}{path_and_query}")).await
    }

    #[test]
    fn provider_slugs_are_kebab_case() {
        for p in [
            OAuthProvider::GoogleDrive,
            OAuthProvider::GoogleCalendar,
            OAuthProvider::Gmail,
        ] {
            let slug = p.slug();
            assert!(!slug.contains('_'), "{slug} should not contain underscore");
            assert!(slug.chars().all(|c| c.is_ascii_lowercase() || c == '-'));
        }
    }

    /// The authorization URL must bind the login to its session: a one-time
    /// `state` and the S256 challenge of the verifier the exchange will send.
    /// (Before the fix this URL carried neither, so any loopback request with
    /// a `code` could complete somebody else's login.)
    #[tokio::test]
    async fn authorization_url_carries_state_pkce_and_s256() {
        let _port = OAUTH_PORT_LOCK.lock().await;
        let (endpoints, _recorder) = spawn_fake_endpoints().await;
        let session = fake_session(endpoints);
        let url = session.authorization_url();

        assert!(url.starts_with("http://127.0.0.1"), "test endpoints: {url}");
        assert!(url.contains("response_type=code"), "{url}");
        assert_eq!(query_param(&url, "code_challenge_method"), "S256", "{url}");
        let challenge = query_param(&url, "code_challenge");
        let state = query_param(&url, "state");

        // The challenge in the URL is the S256 digest of the verifier the
        // session kept — the pair the token exchange is checked against.
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(session.pkce.code_verifier.as_bytes()));
        assert_eq!(challenge, expected);
        assert_eq!(state, session.pkce.state);
    }

    /// A callback carrying a `state` this login did not generate is answered
    /// and ignored — and the real callback that arrives after it still
    /// completes the login. (Before the fix the first request won, so this
    /// test failed: the injected code was exchanged.)
    #[tokio::test]
    async fn a_callback_with_a_wrong_state_is_ignored_then_the_right_one_succeeds() {
        let _port = OAUTH_PORT_LOCK.lock().await;
        let (endpoints, recorder) = spawn_fake_endpoints().await;
        let session = fake_session(endpoints);
        let url = session.authorization_url();
        let state = query_param(&url, "state");

        let flow = run_flow_against_listener(session, Duration::from_secs(10)).await;

        let injected = get_callback("/oauth/google-drive?code=attacker-code&state=wrong")
            .await
            .expect("injected callback answered");
        assert!(
            injected.status().is_success(),
            "a stray callback still gets an answer, not a connection error"
        );

        let good = get_callback(&format!("/oauth/google-drive?code=good-code&state={state}"))
            .await
            .expect("real callback answered");

        let token = flow.await.expect("flow task").expect("flow succeeds");
        assert_eq!(token, "fake-access-token");
        assert!(
            good.text().await.expect("page").contains("authenticated"),
            "the real callback gets the success page"
        );
        let forms = recorder.forms();
        assert_eq!(forms.len(), 1, "exactly one token exchange: {forms:?}");
        assert_eq!(
            forms[0]
                .iter()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.as_str()),
            Some("good-code"),
            "the injected code never reaches the exchange: {forms:?}"
        );
    }

    /// A callback with no `state` at all is ignored the same way — a bare
    /// `?code=` hand-off (which the old flow accepted) must not complete a
    /// login.
    #[tokio::test]
    async fn a_callback_with_no_state_is_ignored() {
        let _port = OAUTH_PORT_LOCK.lock().await;
        let (endpoints, recorder) = spawn_fake_endpoints().await;
        let session = fake_session(endpoints);
        let state = query_param(&session.authorization_url(), "state");

        let flow = run_flow_against_listener(session, Duration::from_secs(10)).await;

        let bare = get_callback("/oauth/google-drive?code=no-state-code")
            .await
            .expect("bare callback answered");
        let _ = bare.text().await;

        let good = get_callback(&format!("/oauth/google-drive?code=good-code&state={state}"))
            .await
            .expect("real callback answered");
        let _ = good.text().await;

        flow.await.expect("flow task").expect("flow succeeds");
        let forms = recorder.forms();
        assert_eq!(forms.len(), 1, "{forms:?}");
        assert_eq!(
            forms[0]
                .iter()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.as_str()),
            Some("good-code"),
        );
    }

    /// A request for another provider's slug is ignored — before the fix it
    /// *ended* the login with an error, so any local process could cancel it.
    #[tokio::test]
    async fn a_wrong_slug_does_not_end_the_flow() {
        let _port = OAUTH_PORT_LOCK.lock().await;
        let (endpoints, recorder) = spawn_fake_endpoints().await;
        let session = fake_session(endpoints);
        let state = query_param(&session.authorization_url(), "state");

        let flow = run_flow_against_listener(session, Duration::from_secs(10)).await;

        let wrong_slug = get_callback("/oauth/some-other-provider?code=x&state=whatever").await;
        assert!(
            wrong_slug
                .expect("wrong slug answered")
                .status()
                .is_success(),
            "a wrong slug still gets an answer"
        );

        let good = get_callback(&format!("/oauth/google-drive?code=good-code&state={state}"))
            .await
            .expect("real callback answered");
        let _ = good.text().await;

        flow.await.expect("flow task").expect("flow succeeds");
        assert_eq!(recorder.forms().len(), 1);
    }

    /// The exchange must carry the `code_verifier` whose S256 digest went out
    /// in the authorization URL — before the fix the exchange sent no
    /// verifier, so a stolen `code` alone was spendable.
    #[tokio::test]
    async fn the_token_exchange_sends_the_verifier_matching_the_challenge() {
        let _port = OAUTH_PORT_LOCK.lock().await;
        let (endpoints, recorder) = spawn_fake_endpoints().await;
        let session = fake_session(endpoints);
        let url = session.authorization_url();
        let challenge = query_param(&url, "code_challenge");
        let state = query_param(&url, "state");

        let flow = run_flow_against_listener(session, Duration::from_secs(10)).await;
        let _ = get_callback(&format!("/oauth/google-drive?code=good-code&state={state}"))
            .await
            .expect("callback answered")
            .text()
            .await;

        flow.await.expect("flow task").expect("flow succeeds");

        let forms = recorder.forms();
        assert_eq!(forms.len(), 1, "{forms:?}");
        let verifier = forms[0]
            .iter()
            .find(|(k, _)| k == "code_verifier")
            .map(|(_, v)| v.clone())
            .expect("the exchange carries a code_verifier");
        let digest = Sha256::digest(verifier.as_bytes());
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        assert_eq!(
            expected, challenge,
            "the verifier's S256 digest must equal the challenge from the URL"
        );
    }

    /// An `error=` parameter with the RIGHT state is the identity provider
    /// refusing THIS login — the flow ends and says so.
    #[tokio::test]
    async fn an_error_with_the_right_state_ends_the_flow_with_that_error() {
        let _port = OAUTH_PORT_LOCK.lock().await;
        let (endpoints, _recorder) = spawn_fake_endpoints().await;
        let session = fake_session(endpoints);
        let state = query_param(&session.authorization_url(), "state");

        let flow = run_flow_against_listener(session, Duration::from_secs(10)).await;
        let page = get_callback(&format!(
            "/oauth/google-drive?error=access_denied&state={state}"
        ))
        .await
        .expect("error callback answered")
        .text()
        .await
        .expect("page");

        let err = flow
            .await
            .expect("flow task")
            .expect_err("the flow ends with the provider's error");
        assert!(err.to_string().contains("access_denied"), "{err}");
        assert!(
            page.contains("failed"),
            "the browser sees the failure: {page}"
        );
    }

    /// The same `error=` with a wrong or missing state is ignored: a forged
    /// denial cannot cancel a login it did not start.
    #[tokio::test]
    async fn an_error_with_a_wrong_state_is_ignored() {
        let _port = OAUTH_PORT_LOCK.lock().await;
        let (endpoints, recorder) = spawn_fake_endpoints().await;
        let session = fake_session(endpoints);
        let state = query_param(&session.authorization_url(), "state");

        let flow = run_flow_against_listener(session, Duration::from_secs(10)).await;

        let forged = get_callback("/oauth/google-drive?error=access_denied&state=forged")
            .await
            .expect("forged denial answered");
        let _ = forged.text().await;
        let bare = get_callback("/oauth/google-drive?error=access_denied")
            .await
            .expect("stateless denial answered");
        let _ = bare.text().await;

        let good = get_callback(&format!("/oauth/google-drive?code=good-code&state={state}"))
            .await
            .expect("real callback answered");
        let _ = good.text().await;

        flow.await.expect("flow task").expect("flow survives");
        assert_eq!(recorder.forms().len(), 1);
    }

    /// The whole wait is capped: no valid callback inside the budget ends
    /// the flow with a clear error (and releases the port).
    #[tokio::test]
    async fn timeout_still_ends_the_flow_with_a_clear_error() {
        let _port = OAUTH_PORT_LOCK.lock().await;
        let (endpoints, _recorder) = spawn_fake_endpoints().await;
        let session = fake_session(endpoints);

        let flow = tokio::spawn(async move { session.finish(Duration::from_millis(300)).await });
        // Somebody knocks, with nothing valid, to prove the timeout is not
        // only for a silent port.
        wait_for_listener().await;
        let _ = get_callback("/oauth/google-drive?code=stray&state=stray")
            .await
            .expect("stray answered")
            .text()
            .await;

        let err = flow
            .await
            .expect("flow task")
            .expect_err("the flow times out");
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    /// Equality of the state check must not depend on where a string
    /// differs: a mismatch deep in the string takes the same time as one at
    /// the front. This is a coarse shared-prefix probe, not a crypto-grade
    /// timing proof — it pins the behaviour the implementation must keep.
    #[test]
    fn constant_time_eq_agrees_with_equality() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcdef"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
        let long_a = "a".repeat(64);
        let mut long_b = long_a.clone();
        long_b.replace_range(60..61, "b");
        assert!(!constant_time_eq(&long_a, &long_b));
        assert!(constant_time_eq(&long_a, &long_a.clone()));
    }

    #[tokio::test]
    #[ignore = "interactive — requires browser + Google OAuth client creds"]
    async fn run_oauth_smoke_google_drive() {
        let token = run_oauth(
            OAuthProvider::GoogleDrive,
            &["https://www.googleapis.com/auth/drive.readonly"],
        )
        .await
        .unwrap();
        assert!(!token.is_empty());
    }
}
