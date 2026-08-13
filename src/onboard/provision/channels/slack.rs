//! Slack provisioner — implements [`TuiProvisioner`] for in-TUI Slack bot setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::SlackConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_text, send};
use crate::onboard::provision::validate::http::probe_post;
use crate::onboard::provision::validate::verdict;
use crate::onboard::provision::ProvisionerCategory;
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
            return Ok(ProvisionOutcome::Aborted("Bot token is required.".into()));
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

        let probe = probe_post(
            "https://slack.com/api/auth.test",
            &[("Authorization", &format!("Bearer {}", bot_token.trim()))],
            "",
        )
        .await;
        // Slack answers 200 even when it rejects the token, so the status says
        // nothing and `classify_status` cannot be used here — `ok` in the body
        // is the only signal. Anything that is neither `ok:true` nor `ok:false`
        // is an unrecognised response, not evidence against the token.
        let verdict = match &probe {
            Ok(r) if r.body.contains("\"ok\":true") => verdict::ProbeVerdict::Accepted,
            Ok(r) if r.body.contains("\"ok\":false") => {
                verdict::ProbeVerdict::Rejected(slack_error(&r.body))
            }
            Ok(_) => verdict::ProbeVerdict::Inconclusive("unrecognised response".into()),
            Err(e) => verdict::ProbeVerdict::Inconclusive(format!("{e}")),
        };
        if !verdict::resolve(&events, &mut responses, verdict, "bot token")
            .await?
            .should_persist()
        {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "The bot token was not saved — Slack is not configured.".into(),
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

        Ok(ProvisionOutcome::Configured)
    }
}

/// Slack reports the reason in `error` alongside `"ok": false`. Surfacing it
/// turns "may be invalid" into something the operator can act on —
/// `invalid_auth` and `account_inactive` need different fixes.
fn slack_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str().map(|s| s.to_string()))
        })
        .map_or_else(
            || "Slack rejected it".to_string(),
            |e| format!("Slack returned `{e}`"),
        )
}
