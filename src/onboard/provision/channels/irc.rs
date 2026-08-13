//! IRC provisioner — implements [`TuiProvisioner`] for in-TUI IRC setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::IrcConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::validate::allowlist;
use crate::onboard::provision::validate::numeric;
use crate::onboard::provision::ProvisionerCategory;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const IRC_NAME: &str = "irc";
pub const IRC_DESC: &str = "IRC — server, port, nickname, channels, TLS, NickServ/SASL passwords";

#[derive(Debug, Clone)]
pub struct IrcProvisioner;

impl IrcProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for IrcProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for IrcProvisioner {
    fn name(&self) -> &'static str {
        IRC_NAME
    }

    fn description(&self) -> &'static str {
        IRC_DESC
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
                text: "Let's configure IRC.".into(),
            },
        )
        .await?;

        // Server
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "server".into(),
                label: "IRC server hostname (e.g. irc.libera.chat)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let server = recv_text(&mut responses).await?;
        let server = server.trim().to_string();
        if server.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Server is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted("Server is required.".into()));
        }

        let port: u16 = numeric::prompt_number(
            &events,
            &mut responses,
            "port",
            "Port (Enter for default 6697 = TLS)",
            6697u16,
        )
        .await?;

        // Nickname
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "nickname".into(),
                label: "Nickname".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let nickname = recv_text(&mut responses).await?;
        let nickname = nickname.trim().to_string();
        if nickname.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Nickname is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted("Nickname is required.".into()));
        }

        // Optional username
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "username".into(),
                label: "Username (Enter to use nickname)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let username = recv_text(&mut responses).await?;
        let username = if username.trim().is_empty() {
            None
        } else {
            Some(username.trim().to_string())
        };

        // TLS
        send(
            &events,
            ProvisionEvent::Choose {
                id: "verify_tls".into(),
                label: "Use TLS?".into(),
                options: vec!["Yes — TLS (recommended)".to_string(), "No".to_string()],
                multi: false,
            },
        )
        .await?;

        let verify_tls = {
            let sel = recv_selection(&mut responses).await?;
            sel.first().copied() != Some(1)
        };

        // Server password
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "server_password".into(),
                label: "Server password (Enter to skip)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let server_password = recv_text(&mut responses).await?;
        let server_password = if server_password.trim().is_empty() {
            None
        } else {
            Some(server_password.trim().to_string())
        };

        // NickServ password
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "nickserv_password".into(),
                label: "NickServ IDENTIFY password (Enter to skip)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let nickserv_password = recv_text(&mut responses).await?;
        let nickserv_password = if nickserv_password.trim().is_empty() {
            None
        } else {
            Some(nickserv_password.trim().to_string())
        };

        // SASL. `IRC_DESC` advertises "NickServ/SASL passwords" but this path
        // never asked and wrote `sasl_password: None`, so a network that
        // requires SASL could not be configured from the TUI at all.
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "sasl_password".into(),
                label: "SASL PLAIN password (Enter to skip)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let sasl_password = recv_text(&mut responses).await?;
        let sasl_password = if sasl_password.trim().is_empty() {
            None
        } else {
            Some(sasl_password.trim().to_string())
        };

        // Channels
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "channels".into(),
                label: "Channels to join (comma-separated, e.g. #RantaiClaw,#bots)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let channels_raw = recv_text(&mut responses).await?;
        let channels: Vec<String> = channels_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Allowed users
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_users".into(),
                label: "Allowed nicknames (comma-separated, empty = deny all, * = allow all)"
                    .into(),
                default: None,
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
        allowlist::warn_on_reach(&events, &allowed_users, "Allowed nicknames").await?;

        // Write config
        config.channels_config.irc = Some(IrcConfig {
            server,
            port,
            nickname,
            username,
            channels,
            allowed_users,
            server_password,
            nickserv_password,
            sasl_password,
            verify_tls: Some(verify_tls),
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "IRC configured.".into(),
            },
        )
        .await?;

        Ok(ProvisionOutcome::Configured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::test_support::{drive, scratch_profile, Answer};

    /// `IRC_DESC` advertises "NickServ/SASL passwords", but the TUI path never
    /// asked and hardcoded `sasl_password: None`, so a network requiring SASL
    /// could not be configured from here at all.
    #[tokio::test]
    async fn irc_sasl_prompt_is_reachable() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &IrcProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Text("irc.example.com"),
                Answer::Text(""), // port -> default
                Answer::Text("rantaiclaw_bot"),
                Answer::Text(""), // username -> nickname
                Answer::Pick(0),  // TLS yes
                Answer::Text(""), // server password -> skip
                Answer::Text(""), // NickServ -> skip
                Answer::Text("placeholder-sasl-secret"),
                Answer::Text("#rantaiclaw"),
                Answer::Text("rantaiclaw_user"),
            ],
        )
        .await;

        assert!(t.configured(), "expected configured, got {:?}", t.outcome);
        assert!(
            t.prompts().iter().any(|p| p.contains("SASL")),
            "a SASL prompt must be offered: {:?}",
            t.prompts()
        );
        assert_eq!(
            config
                .channels_config
                .irc
                .as_ref()
                .expect("irc config written")
                .sasl_password
                .as_deref(),
            Some("placeholder-sasl-secret"),
            "the answer must reach the config, not be dropped for a hardcoded None"
        );
    }
}
