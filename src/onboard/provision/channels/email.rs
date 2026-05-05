//! Email provisioner — implements [`TuiProvisioner`] for in-TUI Email setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::channels::email_channel::EmailConfig;
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const EMAIL_NAME: &str = "email";
pub const EMAIL_DESC: &str = "Email — IMAP/SMTP server, credentials, from address, IDLE timeout";

#[derive(Debug, Clone)]
pub struct EmailProvisioner;

impl EmailProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EmailProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for EmailProvisioner {
    fn name(&self) -> &'static str {
        EMAIL_NAME
    }

    fn description(&self) -> &'static str {
        EMAIL_DESC
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
                text: "Let's configure Email.".into(),
            },
        )
        .await?;

        // IMAP host
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "imap_host".into(),
                label: "IMAP host (e.g. imap.gmail.com)".into(),
                default: Some("imap.gmail.com".into()),
                secret: false,
            },
        )
        .await?;

        let imap_host = recv_text(&mut responses).await?;
        let imap_host = imap_host.trim().to_string();
        if imap_host.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "IMAP host is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // IMAP port
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "imap_port".into(),
                label: "IMAP port (Enter for default 993)".into(),
                default: Some("993".into()),
                secret: false,
            },
        )
        .await?;

        let imap_port: u16 = recv_text(&mut responses)
            .await?
            .trim()
            .parse()
            .unwrap_or(993);

        // IMAP folder
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "imap_folder".into(),
                label: "IMAP folder to poll (Enter for default INBOX)".into(),
                default: Some("INBOX".into()),
                secret: false,
            },
        )
        .await?;

        let imap_folder = recv_text(&mut responses).await?;
        let imap_folder = if imap_folder.trim().is_empty() {
            "INBOX".to_string()
        } else {
            imap_folder.trim().to_string()
        };

        // SMTP host
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "smtp_host".into(),
                label: "SMTP host (e.g. smtp.gmail.com)".into(),
                default: Some("smtp.gmail.com".into()),
                secret: false,
            },
        )
        .await?;

        let smtp_host = recv_text(&mut responses).await?;
        let smtp_host = smtp_host.trim().to_string();
        if smtp_host.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "SMTP host is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // SMTP port
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "smtp_port".into(),
                label: "SMTP port (Enter for default 587)".into(),
                default: Some("587".into()),
                secret: false,
            },
        )
        .await?;

        let smtp_port: u16 = recv_text(&mut responses)
            .await?
            .trim()
            .parse()
            .unwrap_or(587);

        // From address
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "from_address".into(),
                label: "From address for outgoing emails".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let from_address = recv_text(&mut responses).await?;
        let from_address = from_address.trim().to_string();
        if from_address.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "From address is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Username
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "username".into(),
                label: "Email username (usually same as from address)".into(),
                default: Some(from_address.clone()),
                secret: false,
            },
        )
        .await?;

        let username = recv_text(&mut responses).await?;
        let username = if username.trim().is_empty() {
            from_address.clone()
        } else {
            username.trim().to_string()
        };

        // Password
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "password".into(),
                label: "Email password or app password".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let password = recv_text(&mut responses).await?;
        if password.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Password is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Allowed senders
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_senders".into(),
                label:
                    "Allowed sender addresses (comma-separated, empty = deny all, * = allow all)"
                        .into(),
                default: Some("*".into()),
                secret: false,
            },
        )
        .await?;

        let allowed_raw = recv_text(&mut responses).await?;
        let allowed_senders: Vec<String> =
            if allowed_raw.trim().is_empty() || allowed_raw.trim() == "*" {
                vec!["*".to_string()]
            } else {
                allowed_raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };

        // IDLE timeout
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "idle_timeout".into(),
                label: "IDLE timeout in seconds (Enter for default 1740 = 29 min)".into(),
                default: Some("1740".into()),
                secret: false,
            },
        )
        .await?;

        let idle_timeout_secs: u64 = recv_text(&mut responses)
            .await?
            .trim()
            .parse()
            .unwrap_or(1740);

        // Write config
        config.channels_config.email = Some(EmailConfig {
            imap_host,
            imap_port,
            imap_folder,
            smtp_host,
            smtp_port,
            smtp_tls: true,
            username,
            password: password.trim().to_string(),
            from_address,
            idle_timeout_secs,
            allowed_senders,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Email configured.".into(),
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
    fn provisioner_name_is_email() {
        assert_eq!(EmailProvisioner::new().name(), "email");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!EmailProvisioner::new().description().is_empty());
    }
}
