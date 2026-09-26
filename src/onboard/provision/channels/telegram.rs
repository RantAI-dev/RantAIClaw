//! Telegram provisioner — implements [`TuiProvisioner`] for in-TUI Telegram bot setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::TelegramConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::validate::http::probe_get;
use crate::onboard::provision::validate::verdict;
use crate::onboard::provision::ProvisionerCategory;
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

        let validate_url = format!("https://api.telegram.org/bot{}/getMe", bot_token.trim());
        let probe = probe_get(&validate_url, &[]).await;
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
                    error: "Bot token not saved — Telegram is not configured.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "bot token failed validation and was not saved".into(),
            ));
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
                    "Respond in groups only when @-mentioned or replied to (DMs always)"
                        .to_string(),
                    "Respond to every group message (DMs always)".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let mention_only = {
            let sel = recv_selection(&mut responses).await?;
            sel.first().copied() == Some(0)
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

        Ok(ProvisionOutcome::Configured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::smoke::OfflineProbes;
    use crate::onboard::provision::test_support::{drive, scratch_profile, Answer, Transcript};

    /// Walk Telegram setup with the bot-mode picker answered by `pick`.
    ///
    /// Answers before the picker: token, the probe's "save anyway" confirmation,
    /// and the allowed-users prompt. The probe has no egress here, so it
    /// resolves `Inconclusive` and option 0 keeps the credential.
    async fn drive_bot_mode(pick: usize) -> (Transcript, Config) {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _offline = OfflineProbes::engage();
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &TelegramProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Text("00000000:placeholder-bot-token"),
                Answer::Pick(0), // probe inconclusive -> save anyway
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
                ProvisionEvent::Choose { id, options, .. } if id == "mention_only" => {
                    Some(options.clone())
                }
                _ => None,
            })
            .expect("the Telegram flow must offer a bot-mode choice")
    }

    fn mention_only(config: &Config) -> bool {
        config
            .channels_config
            .telegram
            .as_ref()
            .expect("telegram config written")
            .mention_only
    }

    /// Enter on the first option must write the addressed default: the picker
    /// starts at index 0, and the schema 33 default the gateway and the legacy
    /// wizard write is `true`.
    #[tokio::test]
    async fn first_bot_mode_option_writes_mention_only_true() {
        let (t, config) = drive_bot_mode(0).await;
        assert!(t.configured(), "expected configured, got {:?}", t.outcome);
        assert!(
            mention_only(&config),
            "the first option must be the addressed default"
        );
    }

    #[tokio::test]
    async fn second_bot_mode_option_writes_mention_only_false() {
        let (t, config) = drive_bot_mode(1).await;
        assert!(t.configured(), "expected configured, got {:?}", t.outcome);
        assert!(!mention_only(&config));
    }

    /// The old first label ("Direct messages only") described the opposite of
    /// what `false` does, so pin the offered wording and its order.
    #[tokio::test]
    async fn bot_mode_options_name_the_reply_rule() {
        let (t, _config) = drive_bot_mode(0).await;
        assert_eq!(
            bot_mode_options(&t),
            vec![
                "Respond in groups only when @-mentioned or replied to (DMs always)".to_string(),
                "Respond to every group message (DMs always)".to_string(),
            ]
        );
    }

    /// The plan's primary case. A provisioner that stops on a missing required
    /// field used to emit `Failed` and then return `Ok(())`, which both drivers
    /// read as success — so they installed the core skill and saved the config,
    /// producing the false "channel is set up" signal the skill-install
    /// ordering exists to prevent.
    ///
    /// The skill-install half is gated in the drivers' match arms; what is
    /// unit-testable here is that the outcome says `Aborted` and that nothing
    /// was written. Without the first, the second cannot be enforced.
    #[tokio::test]
    async fn aborted_provisioner_writes_no_config() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &TelegramProvisioner::new(),
            &mut config,
            &profile,
            vec![Answer::Text("   ")],
        )
        .await;

        assert!(
            t.aborted(),
            "an empty bot token must abort, got {:?}",
            t.outcome
        );
        assert!(
            config.channels_config.telegram.is_none(),
            "an aborted provisioner must not write a channel config"
        );
    }
}
