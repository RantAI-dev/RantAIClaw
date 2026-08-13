//! Nextcloud Talk provisioner — implements [`TuiProvisioner`] for in-TUI Nextcloud Talk setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::NextcloudTalkConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_text, send};
use crate::onboard::provision::validate::http::probe_get;
use crate::onboard::provision::validate::verdict;
use crate::onboard::provision::ProvisionerCategory;
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
            return Ok(ProvisionOutcome::Aborted("Server URL is required.".into()));
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
            return Ok(ProvisionOutcome::Aborted("App token is required.".into()));
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

        // Authenticate the way the channel does. The probe used to send Basic
        // auth with an empty username, which Nextcloud rejects — so a valid app
        // token still produced a warning, and operators learned to ignore it.
        let ocs_url = format!("{}/ocs/v2.php/cloud/user", base_url);
        let probe = probe_get(
            &ocs_url,
            &[
                ("OCS-APIRequest", "true"),
                ("Authorization", &format!("Bearer {}", app_token.trim())),
            ],
        )
        .await;
        if !verdict::resolve(
            &events,
            &mut responses,
            verdict::classify_status(&probe),
            "app token",
        )
        .await?
        .should_persist()
        {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "The app token was not saved — Nextcloud Talk is not configured.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "app token failed validation and was not saved".into(),
            ));
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

        Ok(ProvisionOutcome::Configured)
    }
}
