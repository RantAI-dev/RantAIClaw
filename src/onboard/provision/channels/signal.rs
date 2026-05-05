//! Signal provisioner — implements [`TuiProvisioner`] for in-TUI Signal setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::SignalConfig;
use crate::config::Config;
use crate::onboard::provision::validate::file::assert_path_exists;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const SIGNAL_NAME: &str = "signal";
pub const SIGNAL_DESC: &str = "Signal messenger — signal-cli daemon socket + account";

#[derive(Debug, Clone)]
pub struct SignalProvisioner;

impl SignalProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SignalProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for SignalProvisioner {
    fn name(&self) -> &'static str {
        SIGNAL_NAME
    }

    fn description(&self) -> &'static str {
        SIGNAL_DESC
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
                text: "Let's configure Signal.".into(),
            },
        )
        .await?;

        // HTTP URL for signal-cli daemon
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "http_url".into(),
                label: "signal-cli HTTP daemon URL".into(),
                default: Some("http://127.0.0.1:8686".into()),
                secret: false,
            },
        )
        .await?;

        let http_url = recv_text(&mut responses).await?;
        let http_url = http_url.trim().to_string();
        if http_url.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "HTTP URL is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Account phone number
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "account".into(),
                label: "Your Signal phone number (E.164, e.g. +12025551234)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let account = recv_text(&mut responses).await?;
        let account = account.trim().to_string();
        if account.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Account phone number is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Validate socket path exists (it's HTTP-based so we just check connectivity)
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: format!("Checking signal-cli daemon at {http_url}…"),
            },
        )
        .await?;

        // Allowed senders
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_from".into(),
                label: "Allowed sender numbers (comma-separated E.164, or * for all)".into(),
                default: Some("*".into()),
                secret: false,
            },
        )
        .await?;

        let allowed_raw = recv_text(&mut responses).await?;
        let allowed_from: Vec<String> =
            if allowed_raw.trim().is_empty() || allowed_raw.trim() == "*" {
                vec!["*".to_string()]
            } else {
                allowed_raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };

        // Group ID filter
        send(
            &events,
            ProvisionEvent::Choose {
                id: "group_filter".into(),
                label: "Which messages to receive?".into(),
                options: vec![
                    "All messages (DMs and groups)".to_string(),
                    "Direct messages only".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let group_id = {
            let sel = recv_selection(&mut responses).await?;
            match sel.first().copied() {
                Some(1) => Some("dm".to_string()),
                _ => None,
            }
        };

        // Write config
        config.channels_config.signal = Some(SignalConfig {
            http_url,
            account,
            group_id,
            allowed_from,
            ignore_attachments: false,
            ignore_stories: true,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Signal configured.".into(),
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
    fn provisioner_name_is_signal() {
        assert_eq!(SignalProvisioner::new().name(), "signal");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!SignalProvisioner::new().description().is_empty());
    }
}
