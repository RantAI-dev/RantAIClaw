//! DingTalk provisioner — implements [`TuiProvisioner`] for in-TUI DingTalk setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::DingTalkConfig;
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_get;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const DINGTALK_NAME: &str = "dingtalk";
pub const DINGTALK_DESC: &str = "DingTalk — client ID (AppKey), client secret, allowed users";

#[derive(Debug, Clone)]
pub struct DingTalkProvisioner;

impl DingTalkProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DingTalkProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for DingTalkProvisioner {
    fn name(&self) -> &'static str {
        DINGTALK_NAME
    }

    fn description(&self) -> &'static str {
        DINGTALK_DESC
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
                text: "Let's configure DingTalk.".into(),
            },
        )
        .await?;

        // Client ID (AppKey)
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "client_id".into(),
                label: "Client ID (AppKey) from DingTalk developer console".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let client_id = recv_text(&mut responses).await?;
        if client_id.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Client ID is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Client Secret
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "client_secret".into(),
                label: "Client Secret (AppSecret)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let client_secret = recv_text(&mut responses).await?;
        if client_secret.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Client Secret is required.".into(),
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

        let token_url = format!(
            "https://api.dingtalk.com/v1.0/oauth2/accessToken?appkey={}&appsecret={}",
            client_id.trim(),
            client_secret.trim()
        );

        match probe_get(&token_url, &[]).await {
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
                label:
                    "Allowed user IDs (comma-separated staff IDs, empty = deny all, * = allow all)"
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
        config.channels_config.dingtalk = Some(DingTalkConfig {
            client_id: client_id.trim().to_string(),
            client_secret: client_secret.trim().to_string(),
            allowed_users,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "DingTalk configured.".into(),
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
    fn provisioner_name_is_dingtalk() {
        assert_eq!(DingTalkProvisioner::new().name(), "dingtalk");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!DingTalkProvisioner::new().description().is_empty());
    }
}
