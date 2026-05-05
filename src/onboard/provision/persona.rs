//! Persona provisioner — implements [`TuiProvisioner`] for in-TUI persona setup.
//!
//! Mirrors the 4-step interview from [`crate::persona::interview`]:
//!   1. Choose preset (5 options)
//!   2. Prompt role
//!   3. Choose tone (formal / neutral / casual)
//!   4. Prompt avoid (optional)

use super::traits::{ProvisionEvent, ProvisionIo, Severity, TuiProvisioner};
use crate::config::Config;
use crate::persona::{self, PersonaToml, PresetId};
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const PERSONA_NAME: &str = "persona";
pub const PERSONA_DESC: &str = "Agent personality — preset, role, tone, and preferences";

#[derive(Debug, Clone)]
pub struct PersonaProvisioner;

impl PersonaProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PersonaProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for PersonaProvisioner {
    fn name(&self) -> &'static str {
        PERSONA_NAME
    }

    fn description(&self) -> &'static str {
        PERSONA_DESC
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
                text: "Let's set up your agent persona.".into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Step 1/4 — choose a preset".into(),
            },
        )
        .await?;

        let preset_options: Vec<String> = PresetId::ALL
            .iter()
            .map(|p| format!("{} — {}", p.slug(), p.description()))
            .collect();

        send(
            &events,
            ProvisionEvent::Choose {
                id: "preset".into(),
                label: "Pick a persona preset".into(),
                options: preset_options.clone(),
                multi: false,
            },
        )
        .await?;

        let selection = recv_selection(&mut responses).await?;
        let preset_idx = selection.first().copied().unwrap_or(0);
        let preset = PresetId::ALL
            .get(preset_idx)
            .copied()
            .unwrap_or(PresetId::Default);

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: format!("Selected: {} — {}", preset.slug(), preset.description()),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Step 2/4 — primary role".into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Prompt {
                id: "role".into(),
                label: "Primary role for this agent (one sentence)".into(),
                default: Some("general productivity and helpful assistance".into()),
                secret: false,
            },
        )
        .await?;

        let role = recv_text(&mut responses).await?;
        let role = if role.is_empty() {
            "general productivity and helpful assistance".to_string()
        } else {
            role
        };

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Step 3/4 — tone".into(),
            },
        )
        .await?;

        let tone_options = vec!["formal".into(), "neutral".into(), "casual".into()];

        send(
            &events,
            ProvisionEvent::Choose {
                id: "tone".into(),
                label: "Tone".into(),
                options: tone_options,
                multi: false,
            },
        )
        .await?;

        let tone_selection = recv_selection(&mut responses).await?;
        let tone_idx = tone_selection.first().copied().unwrap_or(1);
        let tone = match tone_idx {
            0 => "formal",
            2 => "casual",
            _ => "neutral",
        }
        .to_string();

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: format!("Tone: {tone}"),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Step 4/4 — anything to avoid (optional)".into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Prompt {
                id: "avoid".into(),
                label: "Anything to avoid? (Enter to skip)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let avoid = recv_text(&mut responses).await?;
        let avoid = if avoid.trim().is_empty() {
            None
        } else {
            Some(avoid.trim().to_string())
        };

        let persona_record = PersonaToml {
            preset,
            name: String::new(),
            timezone: "UTC".to_string(),
            role,
            tone,
            avoid,
        };

        persona::write_persona_toml(profile, &persona_record)?;
        persona::render_system_md(profile, &persona_record)?;

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!(
                    "Persona saved as `{}` preset. SYSTEM.md generated.",
                    preset.slug()
                ),
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
        .map_err(|e| anyhow::anyhow!("failed to send provision event: {e}"))
}

async fn recv_selection(
    responses: &mut tokio::sync::mpsc::Receiver<super::traits::ProvisionResponse>,
) -> Result<Vec<usize>> {
    match responses.recv().await {
        Some(super::traits::ProvisionResponse::Selection(indices)) => Ok(indices),
        Some(super::traits::ProvisionResponse::Cancelled) => {
            anyhow::bail!("persona setup cancelled by user")
        }
        Some(_) => anyhow::bail!("unexpected response type"),
        None => anyhow::bail!("response channel closed unexpectedly"),
    }
}

async fn recv_text(
    responses: &mut tokio::sync::mpsc::Receiver<super::traits::ProvisionResponse>,
) -> Result<String> {
    match responses.recv().await {
        Some(super::traits::ProvisionResponse::Text(t)) => Ok(t),
        Some(super::traits::ProvisionResponse::Cancelled) => {
            anyhow::bail!("persona setup cancelled by user")
        }
        Some(_) => anyhow::bail!("unexpected response type"),
        None => anyhow::bail!("response channel closed unexpectedly"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_persona() {
        let p = PersonaProvisioner::new();
        assert_eq!(p.name(), "persona");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        let p = PersonaProvisioner::new();
        assert!(!p.description().is_empty());
    }
}
