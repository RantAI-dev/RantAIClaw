//! Slack provisioner — implements [`TuiProvisioner`] for in-TUI Slack bot setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::SlackConfig;
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_post;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const SLACK_NAME: &str = "slack";
pub const SLACK_DESC: &str =
    "Slack bot — bot token (xoxb), app-level token (xapp), channel/user restrictions";

#[derive(Debug, Clone)]
pub struct SlackProvisioner;

impl SlackProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SlackProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for SlackProvisioner {
    fn name(&self) -> &'static str {
        SLACK_NAME
    }

    fn description(&self) -> &'static str {
        SLACK_DESC
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
                text: "Let's configure your Slack bot.".into(),
            },
        )
        .await?;

        // Bot token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "bot_token".into(),
                label: "Bot token (xoxb-...)".into(),
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

        // Optional app-level token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "app_token".into(),
                label: "App-level token for Socket Mode (xapp-..., Enter to skip)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let app_token = recv_text(&mut responses).await?;
        let app_token = if app_token.trim().is_empty() {
            None
        } else {
            Some(app_token.trim().to_string())
        };

        // Validate bot token
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Validating bot token…".into(),
            },
        )
        .await?;

        match probe_post(
            "https://slack.com/api/auth.test",
            &[("Authorization", &format!("Bearer {}", bot_token.trim()))],
            "",
        )
        .await
        {
            Ok(result) if result.body.contains("\"ok\":true") => {
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
                        text: "Bot token may be invalid.".into(),
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
                label: "Channel ID to restrict bot to (Enter to skip)".into(),
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

        // Write config
        config.channels_config.slack = Some(SlackConfig {
            bot_token: bot_token.trim().to_string(),
            app_token,
            channel_id,
            allowed_users,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Slack bot configured.".into(),
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
    fn provisioner_name_is_slack() {
        let p = SlackProvisioner::new();
        assert_eq!(p.name(), "slack");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        let p = SlackProvisioner::new();
        assert!(!p.description().is_empty());
    }
}
