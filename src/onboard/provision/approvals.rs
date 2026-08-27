//! Approvals provisioner — implements [`TuiProvisioner`] for in-TUI
//! preset selection (Manual / Smart / Strict / Off).
//!
//! Mirrors the legacy flow in [`crate::onboard::section::approvals`]:
//!   1. Choose preset (Manual / Smart / Strict / Off)
//!   2. Write policy files via `crate::approval::policy_writer::write_policy_files`
//!
//! Config writes: `<profile>/policy/autonomy.toml`, `command_allowlist.toml`, `forbidden_paths.toml`

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner};
use crate::approval::policy_writer::{self, PolicyPreset};
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, send};
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

    async fn run(
        &self,
        config: &mut Config,
        profile: &Profile,
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

        if let Some(warning) = policy_writer::write_policy_files(profile, preset, false)? {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Warn,
                    text: warning.to_string(),
                },
            )
            .await?;
        }

        // Mirror the marker into `config.autonomy` — the runtime gate reads the
        // config, so without this the preset selected here never took effect.
        // The wizard saves the config after each provisioner returns.
        policy_writer::sync_config_to_active_preset(&profile.policy_dir(), config);

        // Report the EFFECTIVE (on-disk) preset, not the offered one. The write
        // above is idempotent — an existing preset is left untouched — so a
        // profile that already had, say, Strict keeps it, and claiming "set to
        // Smart" would be a lie. Warn when they differ and point at --force.
        let effective = policy_writer::read_active_preset(&profile.policy_dir()).unwrap_or(preset);
        let effective_label = effective.label();
        if effective != preset {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Warn,
                    text: format!(
                        "An approval preset ({effective_label}) was already configured — keeping it. \
                         Run `rantaiclaw setup approvals --force` to switch to {label}."
                    ),
                },
            )
            .await?;
        }

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!("Approval policy set: {effective_label}"),
            },
        )
        .await?;

        Ok(ProvisionOutcome::Configured)
    }
}
