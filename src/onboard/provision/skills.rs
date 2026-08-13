//! Skills provisioner — implements [`TuiProvisioner`] for in-TUI skills setup.
//!
//! `run` does two things:
//!   1. Offer to install the bundled 5-skill starter pack.
//!   2. Point the user at `/skills install` (the live ClawHub search/install
//!      picker) for anything beyond the starter pack — that flow doesn't fit
//!      the provisioner's request-response `Choose`/`Prompt` protocol, so
//!      this provisioner doesn't attempt a wizard-embedded multi-select.
//!
//! Config writes: none (skills live in `<profile>/skills/`)

use super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, ProvisionResponse, Severity, TuiProvisioner,
};
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

    async fn run(
        &self,
        _config: &mut Config,
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
                text: "Let's set up your agent's skills.".into(),
            },
        )
        .await?;

        // Always-on core skills, installed before and independent of the
        // starter-pack choice below.
        //
        // `owner-permissions` teaches the agent the owner/guest model behind
        // `manage_permissions` and `issue_pairing_code` — both registered
        // unconditionally, so without it the tools exist but the agent has no
        // manual for them. Only the headless `SetupSection` installed it, and
        // `setup` reaches that section only when stdin is not a terminal; with
        // one it launches this provisioner instead. So the skill described as
        // always-on was, in practice, almost never installed.
        match bundled::install_core_skills(profile) {
            Ok(core) if !core.is_empty() => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Success,
                        text: format!("Installed core skill(s): {}", core.join(", ")),
                    },
                )
                .await?;
            }
            Ok(_) => {}
            // Non-fatal: the rest of skills setup is still worth running.
            Err(e) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: format!("Could not install the owner-permissions skill: {e}"),
                    },
                )
                .await?;
            }
        }

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

        Ok(ProvisionOutcome::Configured)
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

    /// Restores `HOME` on drop.
    ///
    /// `Profile::skills_dir()` resolves through `profile::paths`, which reads
    /// the home directory on every call and ignores `Profile::root` — so
    /// overriding `HOME` is the only way to keep this test off the developer's
    /// real profile tree, and the override has to outlive the assertions.
    struct HomeGuard(Option<std::ffi::OsString>);

    impl HomeGuard {
        fn set(path: &std::path::Path) -> Self {
            let prev = std::env::var_os("HOME");
            std::env::set_var("HOME", path);
            Self(prev)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(prev) => std::env::set_var("HOME", prev),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// Drive `run` to completion, answering every question with `Skip`
    /// (index 1) so the starter pack never installs.
    async fn run_answering_skip(profile: &Profile) {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(32);
        let (response_tx, response_rx) = tokio::sync::mpsc::channel(4);

        let responder = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if matches!(event, ProvisionEvent::Choose { .. })
                    && response_tx
                        .send(ProvisionResponse::Selection(vec![1]))
                        .await
                        .is_err()
                {
                    break;
                }
            }
        });

        let mut config = Config::default();
        SkillsProvisioner::new()
            .run(
                &mut config,
                profile,
                ProvisionIo {
                    events: event_tx,
                    responses: response_rx,
                },
            )
            .await
            .expect("provisioner run");
        responder.abort();
    }

    #[tokio::test]
    async fn installs_the_always_on_core_skill_even_when_the_starter_pack_is_skipped() {
        // `owner-permissions` teaches the agent the owner/guest model behind
        // `manage_permissions` and `issue_pairing_code` — both registered
        // unconditionally, so without it the tools exist but the agent has no
        // manual for them. It used to be installed only by the headless
        // `SetupSection`, which `setup` reaches only without a terminal, so on
        // the path users actually take it was never installed.
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp home");
        let _home = HomeGuard::set(tmp.path());
        let profile = Profile {
            name: "provision-skills-test".to_string(),
            root: tmp.path().to_path_buf(),
        };

        run_answering_skip(&profile).await;

        let core = crate::skills::bundled::CORE_PACK[0].slug;
        assert!(
            profile.skills_dir().join(core).join("SKILL.md").exists(),
            "core skill `{core}` must be installed regardless of the starter-pack answer"
        );

        // Guards the assertion above from passing for the wrong reason: if
        // "Skip" installed the pack anyway, a directory would exist either way.
        for entry in crate::skills::bundled::STARTER_PACK {
            assert!(
                !profile.skills_dir().join(entry.slug).exists(),
                "starter-pack skill `{}` must not be installed after Skip",
                entry.slug
            );
        }
    }
}
