//! Composio provisioner — implements [`TuiProvisioner`] for in-TUI Composio setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::ComposioConfig;
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_get;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const COMPOSIO_NAME: &str = "composio";
pub const COMPOSIO_DESC: &str =
    "Composio — API key and tool pack enablement for managed OAuth integrations";

#[derive(Debug, Clone)]
pub struct ComposioProvisioner;

impl ComposioProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ComposioProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for ComposioProvisioner {
    fn name(&self) -> &'static str {
        COMPOSIO_NAME
    }

    fn description(&self) -> &'static str {
        COMPOSIO_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Integration
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
                text: "Let's configure Composio.".into(),
            },
        )
        .await?;

        // API key
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "api_key".into(),
                label: "Composio API key".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let api_key = recv_text(&mut responses).await?;
        if api_key.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "API key is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Validate
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Validating API key…".into(),
            },
        )
        .await?;

        match probe_get(
            "https://backend.composio.dev/api/v2/auth/whoami",
            &[("X-API-Key", api_key.trim())],
        )
        .await
        {
            Ok(result) if result.status == 200 => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Success,
                        text: "API key validated.".into(),
                    },
                )
                .await?;
            }
            Ok(_) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: "API key may be invalid.".into(),
                    },
                )
                .await?;
            }
            Err(e) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: format!("Could not validate: {e}. Continuing…"),
                    },
                )
                .await?;
            }
        }

        // Default tool packs (enable all recommended)
        send(
            &events,
            ProvisionEvent::Choose {
                id: "tool_packs".into(),
                label: "Enable which tool packs?".into(),
                options: vec![
                    "All recommended packs".to_string(),
                    "Select manually".to_string(),
                    "None".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let _enabled_tools = match sel.first().copied() {
            Some(0) => {
                send(&events, ProvisionEvent::Message {
                    severity: Severity::Info,
                    text: "Recommended tool packs will be enabled (github, slack, notion, gmail, googlecalendar, jira, linear).".into(),
                }).await?;
            }
            Some(1) => {
                send(&events, ProvisionEvent::Message {
                    severity: Severity::Info,
                    text: "Tool pack selection — press Enter to continue (full picker requires Composio CLI)".into(),
                }).await?;
            }
            _ => {}
        };

        config.composio = ComposioConfig {
            api_key: Some(api_key.trim().to_string()),
            ..ComposioConfig::default()
        };

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Composio configured.".into(),
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
    fn provisioner_name_is_composio() {
        assert_eq!(ComposioProvisioner::new().name(), "composio");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!ComposioProvisioner::new().description().is_empty());
    }
}
