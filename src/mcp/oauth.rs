//! Minimal local-listener OAuth flow for the curated MCP picker.
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`
//! §"Section 5 — mcp (NEW)" — bullet 3.

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

/// Fixed listener port. Must match what's registered in upstream OAuth
/// app config (`http://localhost:11500/oauth/<provider-slug>`).
pub const OAUTH_PORT: u16 = 11500;

/// Hard cap on how long we wait for the user to complete the flow.
pub const OAUTH_TIMEOUT: Duration = Duration::from_mins(5);

struct ProviderConfig {
    auth_url: &'static str,
    token_url: &'static str,
    client_id_env: &'static str,
    client_secret_env: &'static str,
}

impl ProviderConfig {
    fn for_provider(provider: OAuthProvider) -> Self {
        match provider {
            OAuthProvider::GoogleDrive | OAuthProvider::GoogleCalendar | OAuthProvider::Gmail => {
                Self {
                    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
                    token_url: "https://oauth2.googleapis.com/token",
                    client_id_env: "RANTAICLAW_GOOGLE_CLIENT_ID",
                    client_secret_env: "RANTAICLAW_GOOGLE_CLIENT_SECRET",
                }
            }
        }
    }
}

/// Run the full OAuth flow: open browser → wait for redirect →
/// exchange code → return access token.
pub async fn run_oauth(provider: OAuthProvider, scopes: &[&str]) -> Result<String> {
    let cfg = ProviderConfig::for_provider(provider);
    let client_id = std::env::var(cfg.client_id_env).map_err(|_| {
        anyhow!(
            "missing {}; set it (and {}) before running OAuth — see docs/install.md",
            cfg.client_id_env,
            cfg.client_secret_env
        )
    })?;
    let client_secret = std::env::var(cfg.client_secret_env).map_err(|_| {
        anyhow!(
            "missing {}; set it (and {}) before running OAuth",
            cfg.client_secret_env,
            cfg.client_id_env
        )
    })?;
    let redirect_uri = format!("http://localhost:{}/oauth/{}", OAUTH_PORT, provider.slug());

    let scope = scopes.join(" ");
    let auth_url = format!(
        "{}?response_type=code&access_type=offline&prompt=consent&client_id={}&redirect_uri={}&scope={}",
        cfg.auth_url,
        urlencoding::encode(&client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&scope),
    );

    let (code_tx, code_rx) = oneshot::channel::<Result<String>>();
    let server_handle = spawn_listener(provider, code_tx).await?;

    if webbrowser::open(&auth_url).is_err() {
        eprintln!(
            "Could not open browser automatically. Open this URL manually:\n  {}",
            auth_url
        );
    }

    let code = match tokio::time::timeout(OAUTH_TIMEOUT, code_rx).await {
        Ok(Ok(Ok(code))) => code,
        Ok(Ok(Err(e))) => {
            server_handle.shutdown();
            return Err(e);
        }
        Ok(Err(_recv_err)) => {
            server_handle.shutdown();
            bail!("OAuth listener died before receiving redirect");
        }
        Err(_) => {
            server_handle.shutdown();
            bail!(
                "OAuth flow timed out after {}s — re-run setup mcp to retry",
                OAUTH_TIMEOUT.as_secs()
            );
        }
    };

    server_handle.shutdown();

    exchange_code(&cfg, &client_id, &client_secret, &redirect_uri, &code).await
}

struct ListenerHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl ListenerHandle {
    fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let join = self.join;
        tokio::spawn(async move {
            if tokio::time::timeout(Duration::from_secs(2), join)
                .await
                .is_err()
            {
                // Server didn't honour graceful shutdown — bail.
            }
        });
    }
}

async fn spawn_listener(
    provider: OAuthProvider,
    code_tx: oneshot::Sender<Result<String>>,
) -> Result<ListenerHandle> {
    let expected_slug = provider.slug();
    let state = Arc::new(ListenerState {
        expected_slug: expected_slug.to_string(),
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

struct ListenerState {
    expected_slug: String,
    sender: Mutex<Option<oneshot::Sender<Result<String>>>>,
}

#[derive(Deserialize)]
struct OAuthCallback {
    code: Option<String>,
    error: Option<String>,
}

async fn handle_redirect(
    State(state): State<Arc<ListenerState>>,
    AxumPath(provider): AxumPath<String>,
    Query(params): Query<OAuthCallback>,
) -> Html<&'static str> {
    if provider != state.expected_slug {
        if let Some(sender) = state.sender.lock().await.take() {
            let _ = sender.send(Err(anyhow!(
                "redirect for unexpected provider: got {provider}, expected {}",
                state.expected_slug
            )));
        }
        return Html("<h3>Wrong provider</h3>");
    }

    let result = match (params.code, params.error) {
        (Some(code), _) => Ok(code),
        (None, Some(err)) => Err(anyhow!("provider returned error: {err}")),
        (None, None) => Err(anyhow!("redirect missing both `code` and `error`")),
    };

    if let Some(sender) = state.sender.lock().await.take() {
        let _ = sender.send(result);
    }

    Html(SUCCESS_PAGE)
}

const SUCCESS_PAGE: &str = "<!doctype html>\
<html><head><title>RantaiClaw OAuth</title></head>\
<body style=\"font-family: system-ui; padding: 3rem; text-align: center;\">\
<h2>You're authenticated.</h2>\
<p>You can close this tab and return to your terminal.</p>\
</body></html>";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

async fn exchange_code(
    cfg: &ProviderConfig,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code: &str,
) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("build OAuth token-exchange client")?;

    let params = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("redirect_uri", redirect_uri),
        ("code", code),
    ];

    let resp = client
        .post(cfg.token_url)
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

#[cfg(test)]
mod tests {
    use super::*;

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
