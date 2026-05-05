//! Telegram provisioner — implements [`TuiProvisioner`] for in-TUI Telegram bot setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::TelegramConfig;
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_get;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const TELEGRAM_NAME: &str = "telegram";
pub const TELEGRAM_DESC: &str = "Telegram bot — bot token, allowed users, mention mode";

#[derive(Debug, Clone)]
pub struct TelegramProvisioner;

impl TelegramProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TelegramProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for TelegramProvisioner {
    fn name(&self) -> &'static str {
        TELEGRAM_NAME
    }

    fn description(&self) -> &'static str {
        TELEGRAM_DESC
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
                text: "Let's configure your Telegram bot.".into(),
            },
        )
        .await?;

        // Bot token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "bot_token".into(),
                label: "Bot token (from @BotFather)".into(),
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

        let validate_url = format!("https://api.telegram.org/bot{}/getMe", bot_token.trim());
        match probe_get(&validate_url, &[]).await {
            Ok(result) if result.status == 200 => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Success,
                        text: "Bot token validated successfully.".into(),
                    },
                )
                .await?;
            }
            Ok(_) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: "Bot token may be invalid (non-200 response).".into(),
                    },
                )
                .await?;
            }
            Err(e) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: format!("Could not validate token (network error): {e}. Continuing…"),
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
                label: "Allowed user IDs/usernames (comma-separated, empty = deny all)".into(),
                default: Some(String::new()),
                secret: false,
            },
        )
        .await?;

        let allowed_raw = recv_text(&mut responses).await?;
        let allowed_users: Vec<String> = allowed_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Mention-only mode
        send(
            &events,
            ProvisionEvent::Choose {
                id: "mention_only".into(),
                label: "Bot mode".into(),
                options: vec![
                    "Direct messages only".to_string(),
                    "Respond to @-mention in groups (DMs always)".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let mention_only = {
            let sel = recv_selection(&mut responses).await?;
            sel.first().copied() == Some(1)
        };

        // Write config
        config.channels_config.telegram = Some(TelegramConfig {
            bot_token: bot_token.trim().to_string(),
            allowed_users,
            stream_mode: crate::config::schema::StreamMode::default(),
            draft_update_interval_ms: 1500,
            interrupt_on_new_message: false,
            mention_only,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Telegram bot configured.".into(),
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
    fn provisioner_name_is_telegram() {
        let p = TelegramProvisioner::new();
        assert_eq!(p.name(), "telegram");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        let p = TelegramProvisioner::new();
        assert!(!p.description().is_empty());
    }
}
