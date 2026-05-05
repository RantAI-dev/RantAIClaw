//! Nextcloud Talk provisioner — implements [`TuiProvisioner`] for in-TUI Nextcloud Talk setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::NextcloudTalkConfig;
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_get;
use crate::profile::Profile;
use anyhow::{self, Result};
use async_trait::async_trait;

pub const NEXTCLOUD_TALK_NAME: &str = "nextcloud-talk";
pub const NEXTCLOUD_TALK_DESC: &str =
    "Nextcloud Talk — server URL, app token, webhook secret, allowed users";

#[derive(Debug, Clone)]
pub struct NextcloudTalkProvisioner;

impl NextcloudTalkProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NextcloudTalkProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for NextcloudTalkProvisioner {
    fn name(&self) -> &'static str {
        NEXTCLOUD_TALK_NAME
    }

    fn description(&self) -> &'static str {
        NEXTCLOUD_TALK_DESC
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
                text: "Let's configure Nextcloud Talk.".into(),
            },
        )
        .await?;

        // Base URL
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "base_url".into(),
                label: "Nextcloud server URL (e.g. https://cloud.example.com)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let base_url = recv_text(&mut responses).await?;
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Server URL is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // App token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "app_token".into(),
                label: "App token (bot user access token)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let app_token = recv_text(&mut responses).await?;
        if app_token.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "App token is required.".into(),
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

        let ocs_url = format!("{}/ocs/v2.php/cloud/user", base_url);
        let encoded = base64::encode(format!("{}:{}", "", app_token.trim())); // user is empty for app token
        match probe_get(
            &ocs_url,
            &[
                ("OCS-APIRequest", "true"),
                ("Authorization", &format!("Basic {}", encoded)),
            ],
        )
        .await
        {
            Ok(result) if result.status == 200 => {
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
                        text: "Token may be invalid.".into(),
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

        // Optional webhook secret
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "webhook_secret".into(),
                label: "Webhook secret for signature verification (Enter to skip)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let webhook_secret = recv_text(&mut responses).await?;
        let webhook_secret = if webhook_secret.trim().is_empty() {
            None
        } else {
            Some(webhook_secret.trim().to_string())
        };

        // Allowed users
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_users".into(),
                label: "Allowed actor IDs (comma-separated, empty = deny all, * = allow all)"
                    .into(),
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
        config.channels_config.nextcloud_talk = Some(NextcloudTalkConfig {
            base_url,
            app_token: app_token.trim().to_string(),
            webhook_secret,
            allowed_users,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Nextcloud Talk configured.".into(),
            },
        )
        .await?;

        Ok(())
    }
}

use crate::onboard::provision::ProvisionerCategory;

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
    fn provisioner_name_is_nextcloud_talk() {
        assert_eq!(NextcloudTalkProvisioner::new().name(), "nextcloud-talk");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!NextcloudTalkProvisioner::new().description().is_empty());
    }
}
