//! Mattermost provisioner — implements [`TuiProvisioner`] for in-TUI Mattermost setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::MattermostConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::validate::http::probe_get;
use crate::onboard::provision::validate::verdict;
use crate::onboard::provision::ProvisionerCategory;
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

    async fn run(
        &self,
        config: &mut Config,
        _profile: &Profile,
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
            return Ok(ProvisionOutcome::Aborted("Server URL is required.".into()));
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
            return Ok(ProvisionOutcome::Aborted("Bot token is required.".into()));
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

        let probe = probe_get(
            &format!("{}/api/v4/users/me", url),
            &[("Authorization", &format!("Bearer {}", bot_token.trim()))],
        )
        .await;
        if !verdict::resolve(
            &events,
            &mut responses,
            verdict::classify_status(&probe),
            "bot token",
        )
        .await?
        .should_persist()
        {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "The bot token was not saved — Mattermost is not configured.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "bot token failed validation and was not saved".into(),
            ));
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

        Ok(ProvisionOutcome::Configured)
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
