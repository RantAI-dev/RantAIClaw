//! Discord provisioner — implements [`TuiProvisioner`] for in-TUI Discord bot setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::DiscordConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::validate::verdict;
use crate::onboard::provision::ProvisionerCategory;
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

        if !verdict::resolve(
            &events,
            &mut responses,
            crate::channels::discord::validate_bot_token(bot_token.trim()).await,
            "bot token",
        )
        .await?
        .should_persist()
        {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "The bot token was not saved — Discord is not configured.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "bot token failed validation and was not saved".into(),
            ));
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
                    "Respond only when @-mentioned or replied to (DMs always)".to_string(),
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

        Ok(ProvisionOutcome::Configured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::smoke::OfflineProbes;
    use crate::onboard::provision::test_support::{drive, scratch_profile, Answer, Transcript};

    /// Walk Discord setup with the bot-mode picker answered by `pick`.
    ///
    /// Answers before the picker: token, the probe's "save anyway" confirmation,
    /// the optional guild id, and the allowed-users prompt. The probe has no
    /// egress here, so it resolves `Inconclusive` and option 0 keeps the
    /// credential.
    async fn drive_bot_mode(pick: usize) -> (Transcript, Config) {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _offline = OfflineProbes::engage();
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &DiscordProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Text("placeholder-discord-bot-token"),
                Answer::Pick(0),  // probe inconclusive -> save anyway
                Answer::Text(""), // guild id optional
                Answer::Text(""),
                Answer::Pick(pick),
            ],
        )
        .await;
        (t, config)
    }

    fn bot_mode_options(t: &Transcript) -> Vec<String> {
        t.events
            .iter()
            .find_map(|e| match e {
                ProvisionEvent::Choose { id, options, .. } if id == "bot_mode" => {
                    Some(options.clone())
                }
                _ => None,
            })
            .expect("the Discord flow must offer a bot-mode choice")
    }

    fn modes(config: &Config) -> (bool, bool) {
        let discord = config
            .channels_config
            .discord
            .as_ref()
            .expect("discord config written");
        (discord.mention_only, discord.listen_to_bots)
    }

    /// Each option carries a distinct (mention_only, listen_to_bots) pair, so a
    /// label or order change that does not move the mapping with it silently
    /// rewrites the operator's intent. Pin all three.
    #[tokio::test]
    async fn first_bot_mode_option_writes_mention_only_true() {
        let (t, config) = drive_bot_mode(0).await;
        assert!(t.configured(), "expected configured, got {:?}", t.outcome);
        assert_eq!(modes(&config), (true, false));
    }

    #[tokio::test]
    async fn second_bot_mode_option_writes_mention_only_false() {
        let (t, config) = drive_bot_mode(1).await;
        assert!(t.configured(), "expected configured, got {:?}", t.outcome);
        assert_eq!(modes(&config), (false, false));
    }

    #[tokio::test]
    async fn third_bot_mode_option_also_listens_to_bots() {
        let (t, config) = drive_bot_mode(2).await;
        assert!(t.configured(), "expected configured, got {:?}", t.outcome);
        assert_eq!(modes(&config), (false, true));
    }

    #[tokio::test]
    async fn bot_mode_options_name_the_reply_rule() {
        let (t, _config) = drive_bot_mode(0).await;
        assert_eq!(
            bot_mode_options(&t),
            vec![
                "Respond only when @-mentioned or replied to (DMs always)".to_string(),
                "Respond to all messages".to_string(),
                "Respond to all (including other bots)".to_string(),
            ]
        );
    }
}
