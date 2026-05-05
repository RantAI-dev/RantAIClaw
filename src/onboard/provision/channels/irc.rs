//! IRC provisioner — implements [`TuiProvisioner`] for in-TUI IRC setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::IrcConfig;
use crate::config::Config;
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

    async fn run(&self, config: &mut Config, _profile: &Profile, io: ProvisionIo) -> Result<()> {
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
            return Ok(());
        }

        // Port
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "port".into(),
                label: "Port (Enter for default 6697 = TLS)".into(),
                default: Some("6697".into()),
                secret: false,
            },
        )
        .await?;

        let port: u16 = recv_text(&mut responses)
            .await?
            .trim()
            .parse()
            .unwrap_or(6697);

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
            return Ok(());
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
                default: Some("*".into()),
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
        config.channels_config.irc = Some(IrcConfig {
            server,
            port,
            nickname,
            username,
            channels,
            allowed_users,
            server_password,
            nickserv_password,
            sasl_password: None,
            verify_tls: Some(verify_tls),
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "IRC configured.".into(),
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
    fn provisioner_name_is_irc() {
        assert_eq!(IrcProvisioner::new().name(), "irc");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!IrcProvisioner::new().description().is_empty());
    }
}
