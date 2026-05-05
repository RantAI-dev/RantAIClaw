//! Mattermost provisioner — implements [`TuiProvisioner`] for in-TUI Mattermost setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::MattermostConfig;
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_get;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const MATTERMOST_NAME: &str = "mattermost";
pub const MATTERMOST_DESC: &str = "Mattermost — server URL, bot token, channel/user restrictions";

#[derive(Debug, Clone)]
pub struct MattermostProvisioner;

impl MattermostProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MattermostProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for MattermostProvisioner {
    fn name(&self) -> &'static str {
        MATTERMOST_NAME
    }

    fn description(&self) -> &'static str {
        MATTERMOST_DESC
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
                text: "Let's configure Mattermost.".into(),
            },
        )
        .await?;

        // Server URL
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "url".into(),
                label: "Mattermost server URL (e.g. https://mattermost.example.com)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let url = recv_text(&mut responses).await?;
        let url = url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Server URL is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Bot token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "bot_token".into(),
                label: "Bot access token".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let bot_token = recv_text(&mut responses).await?;
        if bot_token.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Bot token is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Validate token
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Validating bot token…".into(),
            },
        )
        .await?;

        match probe_get(
            &format!("{}/api/v4/users/me", url),
            &[("Authorization", &format!("Bearer {}", bot_token.trim()))],
        )
        .await
        {
            Ok(result) if result.status == 200 => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Success,
                        text: "Bot token validated.".into(),
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
                        text: format!("Could not validate token: {e}. Continuing…"),
                    },
                )
                .await?;
            }
        }

        // Optional channel ID
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "channel_id".into(),
                label: "Channel ID to restrict to (Enter to skip)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let channel_id = recv_text(&mut responses).await?;
        let channel_id = if channel_id.trim().is_empty() {
            None
        } else {
            Some(channel_id.trim().to_string())
        };

        // Allowed users
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_users".into(),
                label: "Allowed user IDs (comma-separated, empty = deny all)".into(),
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

        // Thread replies
        send(
            &events,
            ProvisionEvent::Choose {
                id: "thread_replies".into(),
                label: "Reply mode".into(),
                options: vec![
                    "Thread replies (recommended)".to_string(),
                    "Channel root".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let thread_replies = {
            let sel = recv_selection(&mut responses).await?;
            sel.first().copied() != Some(1)
        };

        // Write config
        config.channels_config.mattermost = Some(MattermostConfig {
            url,
            bot_token: bot_token.trim().to_string(),
            channel_id,
            allowed_users,
            thread_replies: Some(thread_replies),
            mention_only: Some(false),
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Mattermost configured.".into(),
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
    fn provisioner_name_is_mattermost() {
        assert_eq!(MattermostProvisioner::new().name(), "mattermost");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!MattermostProvisioner::new().description().is_empty());
    }
}
