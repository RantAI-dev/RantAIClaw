//! QQ provisioner — implements [`TuiProvisioner`] for in-TUI QQ Official Bot setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::QQConfig;
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_post;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const QQ_NAME: &str = "qq";
pub const QQ_DESC: &str = "QQ Official Bot — app ID, app secret, allowed users";

#[derive(Debug, Clone)]
pub struct QqProvisioner;

impl QqProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for QqProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for QqProvisioner {
    fn name(&self) -> &'static str {
        QQ_NAME
    }

    fn description(&self) -> &'static str {
        QQ_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Channel
    }

    async fn run(&self, config: &mut Config, _profile: &Profile, io: ProvisionIo) -> Result<()> {
        let ProvisionIo {
            events,
            mut responses,
        } = io;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Let's configure QQ Official Bot.".into(),
            },
        )
        .await?;

        // App ID
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "app_id".into(),
                label: "App ID (from QQ Bot developer console)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let app_id = recv_text(&mut responses).await?;
        if app_id.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "App ID is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // App Secret
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "app_secret".into(),
                label: "App Secret".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let app_secret = recv_text(&mut responses).await?;
        if app_secret.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "App Secret is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Validate credentials
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Validating credentials…".into(),
            },
        )
        .await?;

        // Exchange the credentials the operator actually typed. The old probe sent
        // the App *ID* as a bearer token to the gateway endpoint and accepted 401
        // as success, so every input — including no secret at all — was reported
        // as "Credentials validated."
        let body = qq_probe_body(app_id.trim(), app_secret.trim());

        match probe_post(
            crate::channels::qq::QQ_AUTH_URL,
            &[("Content-Type", "application/json")],
            &body,
        )
        .await
        {
            Ok(result) if qq_probe_succeeded(result.status, &result.body) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Success,
                        text: "Credentials validated.".into(),
                    },
                )
                .await?;
            }
            Ok(_) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: "Credentials may be invalid.".into(),
                    },
                )
                .await?;
            }
            Err(e) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: format!("Could not validate: {e}. Continuing…"),
                    },
                )
                .await?;
            }
        }

        // Allowed users
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_users".into(),
                label: "Allowed user IDs (comma-separated, empty = deny all, * = allow all)".into(),
                default: Some(String::new()),
                secret: false,
            },
        )
        .await?;

        let allowed_users: Vec<String> = recv_text(&mut responses)
            .await?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Write config
        config.channels_config.qq = Some(QQConfig {
            app_id: app_id.trim().to_string(),
            app_secret: app_secret.trim().to_string(),
            allowed_users,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "QQ Official Bot configured.".into(),
            },
        )
        .await?;

        Ok(())
    }
}

use crate::onboard::provision::ProvisionerCategory;

/// The token-exchange request body, mirroring what `QqChannel` sends.
fn qq_probe_body(app_id: &str, app_secret: &str) -> String {
    serde_json::json!({
        "appId": app_id,
        "clientSecret": app_secret,
    })
    .to_string()
}

/// A probe succeeds only when the endpoint hands back a token. A 401 means the
/// credentials were rejected, which is exactly the case the old probe reported
/// as success.
fn qq_probe_succeeded(status: u16, body: &str) -> bool {
    if status != 200 {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("access_token")
                .and_then(|t| t.as_str().map(|s| !s.is_empty()))
        })
        .unwrap_or(false)
}

async fn send(
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    ev: ProvisionEvent,
) -> Result<()> {
    events
        .send(ev)
        .await
        .map_err(|e| anyhow::anyhow!("send failed: {e}"))
}

async fn recv_selection(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<Vec<usize>> {
    match responses.recv().await {
        Some(ProvisionResponse::Selection(indices)) => Ok(indices),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

async fn recv_text(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<String> {
    match responses.recv().await {
        Some(ProvisionResponse::Text(t)) => Ok(t),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_qq() {
        assert_eq!(QqProvisioner::new().name(), "qq");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!QqProvisioner::new().description().is_empty());
    }

    /// The old probe accepted 401 as success, so an unauthenticated request —
    /// which is what it sent, since it never included the secret — reported
    /// "Credentials validated." for any input at all.
    #[test]
    fn qq_probe_rejects_a_401() {
        assert!(!qq_probe_succeeded(401, ""));
        assert!(!qq_probe_succeeded(401, r#"{"message":"unauthorized"}"#));
    }

    #[test]
    fn qq_probe_requires_a_token_in_the_body() {
        assert!(!qq_probe_succeeded(200, r#"{"message":"ok"}"#));
        assert!(!qq_probe_succeeded(200, r#"{"access_token":""}"#));
        assert!(!qq_probe_succeeded(200, "not json"));
        assert!(qq_probe_succeeded(
            200,
            r#"{"access_token":"placeholder-token"}"#
        ));
    }

    /// The credential the operator typed must be the credential that gets tested.
    #[test]
    fn qq_probe_body_carries_the_secret() {
        let body = qq_probe_body("placeholder-app-id", "placeholder-app-secret");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("body is json");
        assert_eq!(parsed["appId"], "placeholder-app-id");
        assert_eq!(parsed["clientSecret"], "placeholder-app-secret");
    }

    #[test]
    fn qq_probe_url_has_no_query_string() {
        let url = reqwest::Url::parse(crate::channels::qq::QQ_AUTH_URL).expect("probe url parses");
        assert!(url.query().is_none());
    }
}
