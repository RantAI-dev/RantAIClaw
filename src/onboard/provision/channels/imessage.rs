//! iMessage provisioner — implements [`TuiProvisioner`] for in-TUI iMessage setup.
//!
//! macOS only. Checks for Full Disk Access before proceeding.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::IMessageConfig;
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const IMESSAGE_NAME: &str = "imessage";
pub const IMESSAGE_DESC: &str = "iMessage — macOS only, requires Full Disk Access for Terminal";

#[derive(Debug, Clone)]
pub struct IMessageProvisioner;

impl IMessageProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for IMessageProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for IMessageProvisioner {
    fn name(&self) -> &'static str {
        IMESSAGE_NAME
    }

    fn description(&self) -> &'static str {
        IMESSAGE_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Channel
    }

    async fn run(&self, config: &mut Config, _profile: &Profile, io: ProvisionIo) -> Result<()> {
        let ProvisionIo {
            events,
            mut responses,
        } = io;

        // macOS check
        if !cfg!(target_os = "macos") {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "iMessage is macOS-only.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Let's configure iMessage.".into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Warn,
                text: "iMessage requires macOS with Full Disk Access for Terminal.".into(),
            },
        )
        .await?;

        send(&events, ProvisionEvent::Message {
            severity: Severity::Info,
            text: "System Settings → Privacy & Security → Full Disk Access → add Terminal (or iTerm).".into(),
        }).await?;

        // Confirm prerequisites
        send(
            &events,
            ProvisionEvent::Choose {
                id: "prereq_confirm".into(),
                label: "Have you granted Full Disk Access?".into(),
                options: vec!["Yes — continue".to_string(), "No — cancel".to_string()],
                multi: false,
            },
        )
        .await?;

        let confirmed = {
            let sel = recv_selection(&mut responses).await?;
            sel.first().copied() == Some(0)
        };

        if !confirmed {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "iMessage setup cancelled — prerequisites not met.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Check if chat.db is accessible
        let chat_db = std::path::Path::new("/Users/Library/Messages/chat.db");
        if chat_db.exists() {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Success,
                    text: "chat.db is accessible — Full Disk Access is working.".into(),
                },
            )
            .await?;
        } else {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Warn,
                    text:
                        "chat.db not found at expected path. Full Disk Access may not be granted."
                            .into(),
                },
            )
            .await?;
        }

        // Allowed contacts
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_contacts".into(),
                label:
                    "Allowed contacts (comma-separated phone numbers or emails, empty = deny all)"
                        .into(),
                default: Some(String::new()),
                secret: false,
            },
        )
        .await?;

        let allowed_contacts: Vec<String> = recv_text(&mut responses)
            .await?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Write config
        config.channels_config.imessage = Some(IMessageConfig { allowed_contacts });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "iMessage configured.".into(),
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
    fn provisioner_name_is_imessage() {
        assert_eq!(IMessageProvisioner::new().name(), "imessage");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!IMessageProvisioner::new().description().is_empty());
    }
}
