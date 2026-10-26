//! MCP provisioner — implements [`TuiProvisioner`] for in-TUI MCP server setup.
//!
//! Mirrors the legacy flow in [`crate::onboard::section::mcp`]:
//!   1. Multi-select from curated MCP servers
//!   2. Per-auth-required server: prompt for token
//!   3. Option to install all zero-auth servers
//!   4. Custom MCP server command (optional)
//!
//! Config writes: `config.mcp_servers`

use std::io::IsTerminal;

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner};
use crate::config::Config;
use crate::mcp::curated::{AuthMethod, CuratedMcpServer, OAuthProvider, AUTHED, NO_AUTH};
use crate::mcp::{oauth, setup};
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const MCP_NAME: &str = "mcp";
pub const MCP_DESC: &str = "Add curated MCP servers (Notion, Slack, GitHub, …)";

#[derive(Debug, Clone)]
pub struct McpProvisioner;

impl McpProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for McpProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for McpProvisioner {
    fn name(&self) -> &'static str {
        MCP_NAME
    }

    fn description(&self) -> &'static str {
        MCP_DESC
    }

    async fn run(
        &self,
        config: &mut Config,
        _profile: &Profile,
        io: ProvisionIo,
    ) -> Result<ProvisionOutcome> {
        Self::run_with(
            &oauth::OAuthEndpoints::google(),
            std::io::stdin().is_terminal(),
            config,
            io,
        )
        .await
    }
}

impl McpProvisioner {
    /// [`TuiProvisioner::run`] with the two seams tests must set explicitly:
    /// where the OAuth endpoints point, and whether this session can show a
    /// URL and wait for the browser to come back (an interactive terminal
    /// can; a headless run cannot). Production callers go through `run`,
    /// which passes Google's endpoints and the terminal's own answer.
    async fn run_with(
        endpoints: &oauth::OAuthEndpoints,
        can_wait_for_browser: bool,
        config: &mut Config,
        io: ProvisionIo,
    ) -> Result<ProvisionOutcome> {
        let ProvisionIo {
            events,
            mut responses,
        } = io;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Let's add MCP servers to extend your agent's capabilities.".into(),
            },
        )
        .await?;

        // Build the full option list: zero-auth first, then authed
        let mut all_options: Vec<String> = Vec::new();
        // Map global index -> server
        let mut server_by_idx: Vec<(&CuratedMcpServer, bool)> = Vec::new(); // (server, is_authed)

        for s in NO_AUTH {
            all_options.push(format!("{} (no auth)", s.display_name));
            server_by_idx.push((s, false));
        }
        for s in AUTHED {
            all_options.push(format!("{} (auth required)", s.display_name));
            server_by_idx.push((s, true));
        }

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: format!(
                    "Available: {} curated servers ({} require auth)",
                    server_by_idx.len(),
                    AUTHED.len()
                ),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Choose {
                id: "servers".into(),
                label: "Select MCP servers to install (multi-select)".into(),
                options: all_options,
                multi: true,
                filterable: false,
            },
        )
        .await?;

        let selections = recv_selection(&mut responses).await?;

        // Collect each selected authed server's credential and register it —
        // in that order, per server. A server whose credential was not
        // actually obtained is NOT registered; the summary names it with the
        // exact step that finishes the setup instead of counting it authed.
        let mut authed_added = 0;
        let mut not_set_up: Vec<String> = Vec::new();

        for (global_idx, (server, is_authed)) in server_by_idx.iter().enumerate() {
            if !*is_authed || !selections.contains(&global_idx) {
                continue;
            }
            let server: &CuratedMcpServer = server;
            let credential = match server.auth {
                AuthMethod::None => None,
                AuthMethod::Token { hint, .. } => {
                    send(
                        &events,
                        ProvisionEvent::Prompt {
                            id: format!("token_{}", server.slug),
                            label: format!("{} — {}", server.display_name, hint),
                            default: None,
                            secret: true,
                        },
                    )
                    .await?;
                    Some(setup::CollectedCredential::Typed(
                        recv_text(&mut responses).await?,
                    ))
                }
                AuthMethod::OAuth { provider, scopes } => {
                    if !can_wait_for_browser {
                        not_set_up.push(not_set_up_note(
                            server,
                            "run `rantaiclaw setup mcp` in a terminal to finish the login \
                             (it opens a browser on this machine)",
                        ));
                        continue;
                    }
                    match oauth_credential(endpoints, &events, provider, scopes).await {
                        Ok(token) => Some(setup::CollectedCredential::OAuthToken(token)),
                        Err(e) => {
                            not_set_up.push(not_set_up_note(server, &e.to_string()));
                            continue;
                        }
                    }
                }
            };

            // No credential, no registration: an empty answer or a flow that
            // never finished must not turn into a server entry that only
            // looks authenticated.
            let registered = match credential {
                Some(c) => setup::credential_env_pairs(server, c)
                    .and_then(|env| setup::register_mcp(config, server, &env)),
                None => setup::register_mcp(config, server, &[]),
            };
            match registered {
                Ok(()) => authed_added += 1,
                Err(e) => not_set_up.push(not_set_up_note(server, &e.to_string())),
            }
        }

        // Ask about zero-auth servers
        send(
            &events,
            ProvisionEvent::Choose {
                id: "install_zero_auth".into(),
                label: "Install all zero-auth servers?".into(),
                // The inert option is FIRST on purpose: an unattended run
                // takes option 0, and "install every zero-auth server" is not
                // something a no-op headless setup should do.
                options: vec![
                    "No — only selected".to_string(),
                    "Yes — install all zero-auth servers".to_string(),
                ],
                multi: false,
                filterable: false,
            },
        )
        .await?;

        let install_all_zero_auth = {
            let sel = recv_selection(&mut responses).await?;
            sel.first().copied() == Some(1)
        };

        // Custom MCP server
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "custom_command".into(),
                label: "Custom MCP server command (Enter to skip)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let custom_cmd = recv_text(&mut responses).await?;

        // Register zero-auth servers
        let mut zero_auth_added = 0;

        for (zero_idx, server) in NO_AUTH.iter().enumerate() {
            let global_idx = zero_idx;
            if selections.contains(&global_idx) || install_all_zero_auth {
                match setup::register_mcp(config, server, &[]) {
                    Ok(()) => zero_auth_added += 1,
                    Err(e) => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Warn,
                                text: format!("{} not registered: {e}", server.slug),
                            },
                        )
                        .await?;
                    }
                }
            }
        }

        // Custom server
        let mut custom_added = false;
        if !custom_cmd.trim().is_empty() {
            let (cmd, args) = split_custom_command(custom_cmd.trim());
            let cfg = crate::config::schema::McpServerConfig {
                command: cmd,
                args,
                env: std::collections::HashMap::new(),
            };
            let key = format!("custom_{}", authed_added);
            config.mcp_servers.insert(key, cfg);
            custom_added = true;
        }

        let total = authed_added + zero_auth_added + if custom_added { 1 } else { 0 };
        let mut summary = format!(
            "MCP servers registered: {} total ({} authed, {} zero-auth{}{})",
            total,
            authed_added,
            zero_auth_added,
            if custom_added { ", 1 custom" } else { "" },
            if install_all_zero_auth && !selections.is_empty() {
                " (all zero-auth)"
            } else {
                ""
            }
        );
        if !not_set_up.is_empty() {
            summary.push_str(" — not set up: ");
            summary.push_str(&not_set_up.join("; "));
        }
        send(&events, ProvisionEvent::Done { summary }).await?;

        Ok(ProvisionOutcome::Configured)
    }
}

/// The summary note for a selected server whose setup did not finish: what
/// was picked, and — as the caller's `reason` — the exact step that finishes
/// it.
fn not_set_up_note(server: &CuratedMcpServer, reason: &str) -> String {
    format!("{} ({}) — {}", server.display_name, server.slug, reason)
}

/// Run the OAuth flow for a curated server and return the access token:
/// build the session from the operator's environment, show the authorization
/// URL as a TUI message (the terminal surface prints and opens it its own
/// way, which is why this cannot live behind a shared function), open the
/// browser, and wait for the bound callback. Mapping the token to env pairs
/// is the shared gate's job, not this helper's.
async fn oauth_credential(
    endpoints: &oauth::OAuthEndpoints,
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    provider: OAuthProvider,
    scopes: &[&str],
) -> Result<String> {
    let session = oauth::OAuthSession::start_from_env(endpoints.clone(), provider, scopes)?;
    let url = session.authorization_url();
    send(
        events,
        ProvisionEvent::Message {
            severity: Severity::Info,
            text: format!("Open this URL to authorize {}:\n  {url}", provider.slug()),
        },
    )
    .await?;
    let _ = webbrowser::open(&url);
    session.finish(oauth::OAUTH_TIMEOUT).await
}

fn split_custom_command(cmd: &str) -> (String, Vec<String>) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    match parts.split_first() {
        Some((head, tail)) => (
            head.to_string(),
            tail.iter().map(|s| s.to_string()).collect(),
        ),
        None => (String::new(), Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::mcp::oauth::OAUTH_PORT_LOCK;
    use crate::onboard::provision::test_support::{
        drive, drive_entrypoint, scratch_profile, Answer,
    };
    use crate::test_env::{EnvGuard, ENV_LOCK};

    /// The `Done` summary the provisioner sent, if any.
    fn done_summary(events: &[ProvisionEvent]) -> String {
        events
            .iter()
            .find_map(|e| match e {
                ProvisionEvent::Done { summary } => Some(summary.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// The URL the provisioner asked the operator to open, if any.
    fn shown_url(events: &[ProvisionEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                ProvisionEvent::Message { text, .. } => Some(text.clone()),
                _ => None,
            })
            .flat_map(|text| {
                text.lines()
                    .map(|line| line.trim().to_string())
                    .collect::<Vec<_>>()
            })
            .find(|line| line.starts_with("http://"))
            .unwrap_or_default()
    }

    /// Every form body the fake token endpoint received, in order.
    type RecordedForms = Arc<std::sync::Mutex<Vec<Vec<(String, String)>>>>;

    /// A fake OAuth server whose `/authorize` mirrors the `state` it was
    /// given into the loopback redirect — so a "browser" that only knows the
    /// shown URL can complete the flow — and whose `/token` records every
    /// form body it receives.
    async fn spawn_auto_completing_fake() -> (oauth::OAuthEndpoints, RecordedForms) {
        let forms: RecordedForms = Arc::default();
        let token_forms = forms.clone();
        let app = axum::Router::new()
            .route(
                "/authorize",
                axum::routing::get(
                    |axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>| async move {
                        let state = q.get("state").cloned().unwrap_or_default();
                        axum::response::Redirect::to(&format!(
                            "http://127.0.0.1:{}/oauth/google-drive?code=fake-auth-code&state={state}",
                            oauth::OAUTH_PORT,
                        ))
                    },
                ),
            )
            .route(
                "/token",
                axum::routing::post(move |body: axum::body::Bytes| async move {
                    token_forms
                        .lock()
                        .expect("fake token recorder")
                        .push(parse_urlencoded(&body));
                    axum::Json(serde_json::json!({ "access_token": "fake-access-token" }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake OAuth endpoints");
        let addr = listener.local_addr().expect("fake endpoint addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (
            oauth::OAuthEndpoints {
                auth_url: format!("http://{addr}/authorize"),
                token_url: format!("http://{addr}/token"),
            },
            forms,
        )
    }

    /// `axum`'s `Form` extractor is not compiled into this crate, so the
    /// fake token endpoint reads the urlencoded body itself.
    fn parse_urlencoded(body: &[u8]) -> Vec<(String, String)> {
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

    /// A headless session cannot show a URL and wait: picking Google Drive
    /// must register nothing and count no authed server, and the summary
    /// must name the step that finishes the setup. (Fails on b1d43ce4,
    /// where the credential-less entry was registered and counted.)
    #[tokio::test]
    async fn a_headless_oauth_pick_is_not_registered_and_not_counted_authed() {
        let mut config = Config::default();
        let transcript = drive_entrypoint(
            &mut config,
            vec![Answer::Pick(5), Answer::Pick(0), Answer::Text("")],
            async |io, config| {
                McpProvisioner::run_with(&oauth::OAuthEndpoints::google(), false, config, io).await
            },
        )
        .await;

        assert!(transcript.configured());
        assert!(
            !config.mcp_servers.contains_key("google-drive"),
            "a headless OAuth pick must not register the server"
        );
        let summary = done_summary(&transcript.events);
        assert!(summary.contains("0 authed"), "{summary}");
        assert!(summary.contains("not set up"), "{summary}");
        assert!(summary.contains("google-drive"), "{summary}");
        assert!(
            summary.contains("`rantaiclaw setup mcp`"),
            "the summary must name the command that finishes the setup: {summary}"
        );
    }

    /// With the browser seam available but the client id and secret missing
    /// from the environment, the flow cannot even start: nothing is
    /// registered, and the summary names the missing variable and the step.
    #[tokio::test]
    async fn a_missing_client_env_is_reported_and_not_registered() {
        let _env = ENV_LOCK.lock().await;
        let _id = EnvGuard::unset("RANTAICLAW_GOOGLE_CLIENT_ID");
        let _secret = EnvGuard::unset("RANTAICLAW_GOOGLE_CLIENT_SECRET");

        let mut config = Config::default();
        let transcript = drive_entrypoint(
            &mut config,
            vec![Answer::Pick(5), Answer::Pick(0), Answer::Text("")],
            async |io, config| {
                McpProvisioner::run_with(&oauth::OAuthEndpoints::google(), true, config, io).await
            },
        )
        .await;

        assert!(transcript.configured());
        assert!(
            config.mcp_servers.is_empty(),
            "a flow that cannot start must not register anything"
        );
        let summary = done_summary(&transcript.events);
        assert!(summary.contains("0 authed"), "{summary}");
        assert!(summary.contains("not set up"), "{summary}");
        assert!(summary.contains("RANTAICLAW_GOOGLE_CLIENT_ID"), "{summary}");
        assert!(summary.contains("`rantaiclaw setup mcp`"), "{summary}");
    }

    /// The full flow in-process: fake authorization and token servers on
    /// loopback, and the test plays the browser by following the URL the
    /// provisioner shows. The server registers with the returned token and
    /// counts 1 authed.
    #[tokio::test]
    async fn a_finished_oauth_flow_registers_the_server_and_counts_one_authed() {
        let _port = OAUTH_PORT_LOCK.lock().await;
        let _env = ENV_LOCK.lock().await;
        let _browser = EnvGuard::set("BROWSER", "/bin/true");
        let _id = EnvGuard::set("RANTAICLAW_GOOGLE_CLIENT_ID", "test-client-id");
        let _secret = EnvGuard::set("RANTAICLAW_GOOGLE_CLIENT_SECRET", "test-client-secret");

        let (endpoints, forms) = spawn_auto_completing_fake().await;

        // Drive the provisioner with its event stream watched live: the fake
        // `/authorize` mirrors the `state` out of the shown URL, so the
        // "browser" can only be played once that message is on the wire.
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
        let (resp_tx, resp_rx) = tokio::sync::mpsc::channel(64);
        for response in [
            crate::onboard::provision::traits::ProvisionResponse::Selection(vec![5]),
            crate::onboard::provision::traits::ProvisionResponse::Selection(vec![0]),
            crate::onboard::provision::traits::ProvisionResponse::Text(String::new()),
        ] {
            resp_tx.send(response).await.expect("queue answer");
        }
        drop(resp_tx);

        let collector = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(ev) = events_rx.recv().await {
                if let ProvisionEvent::Message { text, .. } = &ev {
                    for line in text.lines() {
                        let url = line.trim();
                        if !url.starts_with("http://127.0.0.1") {
                            continue;
                        }
                        // The loopback listener binds right after this
                        // message is sent; retry while it is still coming up.
                        for _ in 0..200 {
                            match reqwest::get(url).await {
                                Ok(resp) => {
                                    let _ = resp;
                                    break;
                                }
                                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
                            }
                        }
                    }
                }
                seen.push(ev);
            }
            seen
        });

        let mut config = Config::default();
        let outcome = McpProvisioner::run_with(
            &endpoints,
            true,
            &mut config,
            ProvisionIo {
                events: events_tx,
                responses: resp_rx,
            },
        )
        .await;
        let events = collector.await.expect("event collector");

        outcome.expect("provisioner run");
        let entry = config
            .mcp_servers
            .get("google-drive")
            .expect("the finished flow registers the server");
        assert_eq!(
            entry
                .env
                .get("GOOGLE_DRIVE_OAUTH_TOKEN")
                .map(String::as_str),
            Some("fake-access-token"),
            "the server registers with the token the flow returned"
        );
        let summary = done_summary(&events);
        assert!(summary.contains("1 authed"), "{summary}");
        assert!(!summary.contains("not set up"), "{summary}");

        // The shown URL bound the login to its session.
        let url = shown_url(&events);
        assert!(url.contains("state="), "{url}");
        assert!(url.contains("code_challenge="), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");

        // ...and exactly one exchange happened, with the code the fake
        // authorization minted — not one a bystander injected.
        let forms = forms.lock().expect("fake token recorder");
        assert_eq!(forms.len(), 1, "exactly one token exchange: {forms:?}");
        assert_eq!(
            forms[0]
                .iter()
                .find(|(k, _)| *k == "code")
                .map(|(_, v)| v.as_str()),
            Some("fake-auth-code")
        );
    }

    /// A token server whose answer is empty is not registered — an empty
    /// answer must not become a server entry that looks configured.
    /// (Fails on b1d43ce4, where it was registered and counted authed.)
    #[tokio::test]
    async fn an_empty_token_answer_is_not_registered() {
        let mut config = Config::default();
        let profile = scratch_profile(Path::new("/tmp"));
        let transcript = drive(
            &McpProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Pick(2),  // notion — a token server
                Answer::Text(""), // the token prompt: empty answer
                Answer::Pick(0),  // zero-auth bundle: no
                Answer::Text(""), // custom command: skip
            ],
        )
        .await;

        assert!(transcript.configured());
        assert!(
            !config.mcp_servers.contains_key("notion"),
            "an empty token answer must not register the server"
        );
        let summary = done_summary(&transcript.events);
        assert!(summary.contains("0 authed"), "{summary}");
        assert!(summary.contains("not set up"), "{summary}");
        assert!(summary.contains("notion"), "{summary}");
    }
}
