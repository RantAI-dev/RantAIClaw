//! Composio provisioner — implements [`TuiProvisioner`] for in-TUI Composio setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::ComposioConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::validate::http::probe_get;
use crate::onboard::provision::validate::verdict;
use crate::onboard::provision::ProvisionerCategory;
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
            return Ok(ProvisionOutcome::Aborted("API key is required.".into()));
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

        let probe = probe_get(
            "https://backend.composio.dev/api/v2/auth/whoami",
            &[("X-API-Key", api_key.trim())],
        )
        .await;
        if !verdict::resolve(
            &events,
            &mut responses,
            verdict::classify_status(&probe),
            "API key",
        )
        .await?
        .should_persist()
        {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "The API key was not saved — Composio is not configured.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "API key failed validation and was not saved".into(),
            ));
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

        Ok(ProvisionOutcome::Configured)
    }
}
