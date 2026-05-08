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
use crate::skills::bundled::{self, STARTER_PACK};
use crate::skills::clawhub;
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

        // Step 2: Browse ClawHub?
        send(
            &events,
            ProvisionEvent::Choose {
                id: "browse_clawhub".into(),
                label: "Browse ClawHub for more skills?".into(),
                options: vec!["Yes — browse ClawHub".to_string(), "No — skip".to_string()],
                multi: false,
            },
        )
        .await?;

        let browse = {
            let sel = recv_selection(&mut responses).await?;
            sel.first().copied() == Some(0)
        };

        if browse {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Info,
                    text: "Fetching top ClawHub skills…".into(),
                },
            )
            .await?;

            let top = match clawhub::list_top(20).await {
                Ok(items) => items,
                Err(err) => {
                    send(
                        &events,
                        ProvisionEvent::Message {
                            severity: Severity::Warn,
                            text: format!("ClawHub fetch failed: {err}; skipping browse step."),
                        },
                    )
                    .await?;
                    send(
                        &events,
                        ProvisionEvent::Done {
                            summary: format!(
                                "Skills installed: {} from starter pack",
                                installed_names.len()
                            ),
                        },
                    )
                    .await?;
                    return Ok(());
                }
            };

            if top.is_empty() {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Info,
                        text: "ClawHub returned no skills.".into(),
                    },
                )
                .await?;
            } else {
                // Hand off to the live ClawHub install picker — same UX as
                // `/skills install`: search bar at top, server-side search
                // on Enter, top-by-stars when query empty. Picker drives
                // installs inline; we receive the list of installed slugs
                // when the user closes it.
                send(
                    &events,
                    ProvisionEvent::OpenSkillInstallPicker {
                        label: "Install ClawHub skills".into(),
                    },
                )
                .await?;
                let installed = recv_installed_skills(&mut responses).await?;
                if !installed.is_empty() {
                    send(
                        &events,
                        ProvisionEvent::Message {
                            severity: Severity::Success,
                            text: format!(
                                "Installed from ClawHub: {}",
                                installed.join(", ")
                            ),
                        },
                    )
                    .await?;
                    installed_names.extend(installed);
                }
            }
        }

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!("Skills installed: {} total", installed_names.len()),
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

async fn recv_selection_multi(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<Vec<usize>> {
    recv_selection(responses).await
}

async fn recv_installed_skills(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<Vec<String>> {
    match responses.recv().await {
        Some(ProvisionResponse::InstalledSkills(slugs)) => Ok(slugs),
        // Esc-from-picker is treated the same as "user installed nothing"
        // — the wizard advances rather than aborting, since the user has
        // already gotten a starter pack from the bundled step.
        Some(ProvisionResponse::Cancelled) => Ok(Vec::new()),
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
