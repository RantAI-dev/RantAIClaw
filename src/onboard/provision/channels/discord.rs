//! Discord provisioner — implements [`TuiProvisioner`] for in-TUI Discord bot setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::DiscordConfig;
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_get;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const DISCORD_NAME: &str = "discord";
pub const DISCORD_DESC: &str = "Discord bot — bot token, guild, allowed users, mention mode";

#[derive(Debug, Clone)]
pub struct DiscordProvisioner;

impl DiscordProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DiscordProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for DiscordProvisioner {
    fn name(&self) -> &'static str {
        DISCORD_NAME
    }

    fn description(&self) -> &'static str {
        DISCORD_DESC
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
                text: "Let's configure your Discord bot.".into(),
            },
        )
        .await?;

        // Bot token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "bot_token".into(),
                label: "Bot token (from Discord Developer Portal)".into(),
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
            "https://discord.com/api/v10/users/@me",
            &[("Authorization", &format!("Bot {}", bot_token.trim()))],
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

        // Optional guild ID
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "guild_id".into(),
                label: "Guild (server) ID to restrict to (Enter to skip)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let guild_id = recv_text(&mut responses).await?;
        let guild_id = if guild_id.trim().is_empty() {
            None
        } else {
            Some(guild_id.trim().to_string())
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

        // Bot mode
        send(
            &events,
            ProvisionEvent::Choose {
                id: "bot_mode".into(),
                label: "Bot mode".into(),
                options: vec![
                    "Respond to @-mention only".to_string(),
                    "Respond to all messages".to_string(),
                    "Respond to all (including other bots)".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let (mention_only, listen_to_bots) = {
            let sel = recv_selection(&mut responses).await?;
            match sel.first().copied() {
                Some(0) => (true, false),
                Some(1) => (false, false),
                Some(2) => (false, true),
                _ => (false, false),
            }
        };

        // Write config
        config.channels_config.discord = Some(DiscordConfig {
            bot_token: bot_token.trim().to_string(),
            guild_id,
            allowed_users,
            listen_to_bots,
            mention_only,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Discord bot configured.".into(),
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
    fn provisioner_name_is_discord() {
        let p = DiscordProvisioner::new();
        assert_eq!(p.name(), "discord");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        let p = DiscordProvisioner::new();
        assert!(!p.description().is_empty());
    }
}
