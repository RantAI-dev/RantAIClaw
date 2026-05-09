//! Skills provisioner — implements [`TuiProvisioner`] for in-TUI skills setup.
//!
//! Mirrors the legacy flow in [`crate::onboard::section::skills`]:
//!   1. Confirm starter pack install
//!   2. Optionally browse ClawHub top-20 and multi-select
//!   3. Install selected skills
//!
//! Config writes: none (skills live in `<profile>/skills/`)

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner};
use crate::config::Config;
use crate::profile::Profile;
use crate::skills::bundled::{self};
use anyhow::Result;
use async_trait::async_trait;

pub const SKILLS_NAME: &str = "skills";
pub const SKILLS_DESC: &str = "Bundled 5-skill starter pack + optional ClawHub skills";

#[derive(Debug, Clone)]
pub struct SkillsProvisioner;

impl SkillsProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SkillsProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for SkillsProvisioner {
    fn name(&self) -> &'static str {
        SKILLS_NAME
    }

    fn description(&self) -> &'static str {
        SKILLS_DESC
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
                text: "Let's set up your agent's skills.".into(),
            },
        )
        .await?;

        // Step 1: Install starter pack?
        send(
            &events,
            ProvisionEvent::Choose {
                id: "install_pack".into(),
                label: "Install the 5-skill starter pack?".into(),
                options: vec!["Yes — install starter pack".to_string(), "Skip".to_string()],
                multi: false,
            },
        )
        .await?;

        let install_pack = {
            let sel = recv_selection(&mut responses).await?;
            sel.first().copied() == Some(0)
        };

        let mut installed_names: Vec<String> = Vec::new();

        if install_pack {
            match bundled::install_starter_pack(profile) {
                Ok(installed) => {
                    if installed.is_empty() {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Info,
                                text: "All 5 starter-pack skills already present.".into(),
                            },
                        )
                        .await?;
                    } else {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Success,
                                text: format!("Installed starter pack: {}", installed.join(", ")),
                            },
                        )
                        .await?;
                        installed_names.extend(installed);
                    }
                }
                Err(e) => {
                    send(
                        &events,
                        ProvisionEvent::Message {
                            severity: Severity::Warn,
                            text: format!("Starter pack install failed: {e}"),
                        },
                    )
                    .await?;
                }
            }
        }

        // Step 2: Point the user to `/skills install`. The provisioner
        // protocol (Choose / Prompt) is request-response by design — the
        // wizard sends options, the user picks. ClawHub install is
        // interactive, network-driven, and stateful in a way that
        // doesn't fit that mold cleanly. Rather than build a parallel
        // mini-picker inside the overlay, point the user at the
        // already-working `/skills install` command, which has live
        // search, install-and-stay-open, and predictable Enter semantics.
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Run `/skills install` after setup to browse ClawHub \
                       (live search, install one or many, Esc to close)."
                    .into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!(
                    "Skills installed: {} from starter pack — `/skills install` for more",
                    installed_names.len()
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
    fn provisioner_name_is_skills() {
        let p = SkillsProvisioner::new();
        assert_eq!(p.name(), "skills");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        let p = SkillsProvisioner::new();
        assert!(!p.description().is_empty());
    }
}
