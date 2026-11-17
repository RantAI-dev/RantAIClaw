//! Skills provisioner — implements [`TuiProvisioner`] for in-TUI skills setup.
//!
//! `run` does two things:
//!   1. Offer to install the bundled starter pack.
//!   2. Point the user at `/skills install` (the live ClawHub search/install
//!      picker) for anything beyond the starter pack — that flow doesn't fit
//!      the provisioner's request-response `Choose`/`Prompt` protocol, so
//!      this provisioner doesn't attempt a wizard-embedded multi-select.
//!
//! Config writes: none (skills live in `<profile>/skills/`)

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner};
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, send};
use crate::profile::Profile;
use crate::skills::bundled::{self};
use anyhow::Result;
use async_trait::async_trait;

pub const SKILLS_NAME: &str = "skills";

/// The one-line description the wizard shows for the skills step.
///
/// It names no count of skills on purpose. The number was hand-written and
/// could not keep itself true: the starter pack grows without this text
/// being touched, and the always-on core skill installs on top of it.
pub const SKILLS_DESC: &str = "Bundled starter pack + optional ClawHub skills";

/// The label the wizard asks the operator with before the starter pack.
pub const SKILLS_INSTALL_LABEL: &str = "Install the starter pack?";

/// The message shown when an accepted install finds every starter-pack skill
/// on disk after all, because the skills appeared between the check and the
/// install. A pack that is already complete before the question skips it.
pub const SKILLS_ALREADY_PRESENT: &str = "All starter-pack skills already present.";

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

        // Step 1: Install the starter pack, but only ask when either answer
        // changes something. A pack that is fully on disk needs no question,
        // and a partly installed one is offered only what is missing.
        let missing = bundled::starter_pack_missing(profile);
        let mut installed_names: Vec<String> = Vec::new();

        if missing.is_empty() {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Info,
                    text: format!(
                        "Starter pack already installed ({} skills).",
                        bundled::STARTER_PACK.len()
                    ),
                },
            )
            .await?;
        } else {
            let (label, yes) = if missing.len() == bundled::STARTER_PACK.len() {
                (
                    SKILLS_INSTALL_LABEL.to_string(),
                    "Yes — install starter pack".to_string(),
                )
            } else {
                let slugs: Vec<&str> = missing.iter().map(|skill| skill.slug).collect();
                (
                    format!(
                        "Install the {} missing starter-pack skill(s): {}?",
                        missing.len(),
                        slugs.join(", ")
                    ),
                    "Yes — install the missing skills".to_string(),
                )
            };
            send(
                &events,
                ProvisionEvent::Choose {
                    id: "install_pack".into(),
                    label,
                    options: vec![yes, "Skip".to_string()],
                    multi: false,
                    filterable: false,
                },
            )
            .await?;

            let install_pack = {
                let sel = recv_selection(&mut responses).await?;
                sel.first().copied() == Some(0)
            };

            if install_pack {
                match bundled::install_starter_pack(profile) {
                    Ok(installed) => {
                        if installed.is_empty() {
                            send(
                                &events,
                                ProvisionEvent::Message {
                                    severity: Severity::Info,
                                    text: SKILLS_ALREADY_PRESENT.into(),
                                },
                            )
                            .await?;
                        } else {
                            send(
                                &events,
                                ProvisionEvent::Message {
                                    severity: Severity::Success,
                                    text: format!(
                                        "Installed starter pack: {}",
                                        installed.join(", ")
                                    ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::traits::ProvisionResponse;

    // Restores `HOME` on drop.
    //
    // `Profile::skills_dir()` resolves through `profile::paths`, which reads
    // the home directory on every call and ignores `Profile::root` — so
    // overriding `HOME` is the only way to keep this test off the developer's
    // real profile tree, and the override has to outlive the assertions.
    //
    // The crate guard restores the previous value on drop, so a panicking
    // assertion cannot leak the override into the next test.
    use crate::test_env::HomeGuard;

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

    /// Drive `run` to completion, answering every `Choose` with `answer`
    /// (0 = yes, 1 = skip), and return every event it sent.
    async fn run_recording(profile: &Profile, answer: usize) -> Vec<ProvisionEvent> {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(32);
        let (response_tx, response_rx) = tokio::sync::mpsc::channel(4);

        let responder = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(event) = event_rx.recv().await {
                let is_choose = matches!(event, ProvisionEvent::Choose { .. });
                seen.push(event);
                if is_choose
                    && response_tx
                        .send(ProvisionResponse::Selection(vec![answer]))
                        .await
                        .is_err()
                {
                    break;
                }
            }
            seen
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
        responder.await.expect("responder")
    }

    fn chooses(events: &[ProvisionEvent]) -> Vec<&ProvisionEvent> {
        events
            .iter()
            .filter(|e| matches!(e, ProvisionEvent::Choose { .. }))
            .collect()
    }

    fn message_texts(events: &[ProvisionEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                ProvisionEvent::Message { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn done_summary(events: &[ProvisionEvent]) -> &str {
        events
            .iter()
            .find_map(|e| match e {
                ProvisionEvent::Done { summary } => Some(summary.as_str()),
                _ => None,
            })
            .expect("the provisioner finished with a summary")
    }

    #[tokio::test]
    async fn a_complete_starter_pack_is_reported_and_not_asked_about() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp home");
        let _home = HomeGuard::set(tmp.path());
        let profile = Profile {
            name: "provision-skills-test".to_string(),
            root: tmp.path().to_path_buf(),
        };
        crate::skills::bundled::install_starter_pack(&profile).expect("pack installs");

        // Answering "Skip" to any question keeps a base build from hanging.
        let events = run_recording(&profile, 1).await;

        assert!(
            chooses(&events).is_empty(),
            "a question was asked about a pack that is fully installed: {:?}",
            chooses(&events)
        );
        let expected = format!(
            "Starter pack already installed ({} skills).",
            crate::skills::bundled::STARTER_PACK.len()
        );
        assert!(
            message_texts(&events).contains(&expected.as_str()),
            "no already-installed message in {:?}",
            message_texts(&events)
        );
        assert!(
            done_summary(&events).contains("Skills installed: 0 from starter pack"),
            "{}",
            done_summary(&events)
        );
    }

    #[tokio::test]
    async fn a_partial_starter_pack_asks_about_the_missing_skills_only() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp home");
        let _home = HomeGuard::set(tmp.path());
        let profile = Profile {
            name: "provision-skills-test".to_string(),
            root: tmp.path().to_path_buf(),
        };
        crate::skills::bundled::install_starter_pack(&profile).expect("pack installs");
        for slug in ["summarizer", "web-search"] {
            std::fs::remove_dir_all(profile.skills_dir().join(slug)).expect("remove skill");
        }

        let events = run_recording(&profile, 0).await;

        let asked = chooses(&events);
        assert_eq!(asked.len(), 1, "{asked:?}");
        let ProvisionEvent::Choose { label, .. } = asked[0] else {
            unreachable!("chooses() returns only Choose events");
        };
        assert!(
            label.contains("summarizer") && label.contains("web-search"),
            "the question must name the missing skills: {label:?}"
        );
        assert!(
            !label.contains("meeting-notes"),
            "the question must not name an installed skill: {label:?}"
        );
        for skill in crate::skills::bundled::STARTER_PACK {
            assert!(
                profile
                    .skills_dir()
                    .join(skill.slug)
                    .join("SKILL.md")
                    .exists(),
                "{} is not installed after Yes",
                skill.slug
            );
        }
        assert!(
            done_summary(&events).contains("Skills installed: 2 from starter pack"),
            "the summary counts only what this run installed: {}",
            done_summary(&events)
        );
    }

    #[tokio::test]
    async fn skipping_a_partial_starter_pack_installs_nothing() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp home");
        let _home = HomeGuard::set(tmp.path());
        let profile = Profile {
            name: "provision-skills-test".to_string(),
            root: tmp.path().to_path_buf(),
        };
        crate::skills::bundled::install_starter_pack(&profile).expect("pack installs");
        std::fs::remove_dir_all(profile.skills_dir().join("summarizer")).expect("remove skill");

        let events = run_recording(&profile, 1).await;

        assert_eq!(chooses(&events).len(), 1);
        assert!(
            !profile.skills_dir().join("summarizer").exists(),
            "Skip must not install the missing skill"
        );
        assert!(
            done_summary(&events).contains("Skills installed: 0 from starter pack"),
            "{}",
            done_summary(&events)
        );
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
