//! Approvals provisioner — implements [`TuiProvisioner`] for in-TUI
//! preset selection (Manual / Smart / Strict / Off).
//!
//! Mirrors the legacy flow in [`crate::onboard::section::approvals`]:
//!   1. Choose preset (Manual / Smart / Strict / Off)
//!   2. Write policy files via `crate::approval::policy_writer::write_policy_files`
//!
//! Config writes: `<profile>/policy/autonomy.toml`, `command_allowlist.toml`, `forbidden_paths.toml`

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner};
use crate::approval::policy_writer::{self, PolicyPreset};
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const APPROVALS_NAME: &str = "approvals";
pub const APPROVALS_DESC: &str = "Approval policy preset — Manual / Smart / Strict / Off";

#[derive(Debug, Clone)]
pub struct ApprovalsProvisioner;

impl ApprovalsProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ApprovalsProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for ApprovalsProvisioner {
    fn name(&self) -> &'static str {
        APPROVALS_NAME
    }

    fn description(&self) -> &'static str {
        APPROVALS_DESC
    }

    async fn run(&self, _config: &mut Config, profile: &Profile, io: ProvisionIo) -> Result<()> {
        let ProvisionIo {
            events,
            mut responses,
        } = io;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Let's configure the approval policy for this agent.".into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Choose an approval policy preset".into(),
            },
        )
        .await?;

        let options = vec![
            "Manual — prompt for every tool call (safest)".to_string(),
            "Smart — prompt only for writes and system changes (recommended)".to_string(),
            "Strict — deny-by-default, allow read-only".to_string(),
            "Off — autonomous execution, no prompts".to_string(),
        ];

        send(
            &events,
            ProvisionEvent::Choose {
                id: "preset".into(),
                label: "Approval tier".into(),
                options,
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let idx = sel.first().copied().unwrap_or(1);

        let preset = match idx {
            0 => PolicyPreset::Manual,
            1 => PolicyPreset::Smart,
            2 => PolicyPreset::Strict,
            3 => PolicyPreset::Off,
            _ => PolicyPreset::Smart,
        };

        let label = preset.label();

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: format!("Selected: {label}"),
            },
        )
        .await?;

        policy_writer::write_policy_files(profile, preset, false)?;

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!("Approval policy set: {label}"),
            },
        )
        .await?;

        Ok(())
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_approvals() {
        let p = ApprovalsProvisioner::new();
        assert_eq!(p.name(), "approvals");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        let p = ApprovalsProvisioner::new();
        assert!(!p.description().is_empty());
    }
}
