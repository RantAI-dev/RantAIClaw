//! Approvals provisioner — implements [`TuiProvisioner`] for in-TUI
//! preset selection (Manual / Smart / Strict / Off).
//!
//! Mirrors the legacy flow in [`crate::onboard::section::approvals`]:
//!   1. Choose preset (Manual / Smart / Strict / Off)
//!   2. Write policy files via `crate::approval::policy_writer::write_policy_files`
//!
//! Config writes: `<profile>/policy/autonomy.toml`, `command_allowlist.toml`

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner};
use crate::approval::policy_writer::{self, PolicyPreset};
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, send};
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const APPROVALS_NAME: &str = "approvals";
pub const APPROVALS_DESC: &str = "Approval policy preset — Manual / Smart / Strict / Off";

/// The approvals provisioner, with the caller's `--force` intent.
///
/// `force` is the answer to "may this run replace what is already on disk?".
/// Without it, an already-configured preset is KEPT — the write is
/// idempotent, so a preset offered here could not be applied, and offering a
/// choice the run cannot honour is its own defect.
#[derive(Debug, Clone)]
pub struct ApprovalsProvisioner {
    force: bool,
}

impl ApprovalsProvisioner {
    /// A provisioner that keeps an already-configured preset.
    pub fn new() -> Self {
        Self { force: false }
    }

    /// A provisioner that replaces an already-configured preset.
    pub fn with_force(force: bool) -> Self {
        Self { force }
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

        // Read the preset that is ALREADY on disk BEFORE offering anything.
        // Without `--force` the write below is idempotent — an existing
        // preset is left untouched — so a picker offered here would take an
        // answer this run cannot honour. The defect that was: the overlay
        // showed all four presets, the operator picked one, and the old
        // preset silently stayed.
        let existing = policy_writer::read_active_preset(&profile.policy_dir());

        if let Some(existing) = existing {
            if !self.force {
                let label = existing.label();
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Info,
                        text: format!(
                            "An approval preset ({label}) is already configured — keeping it. \
                             Run `rantaiclaw setup approvals --force` to switch to another one."
                        ),
                    },
                )
                .await?;

                // The config is NOT touched here. The runtime gate reads
                // `config.autonomy`, so mirroring the marker into it would
                // change what is enforced — and this run was asked to keep
                // what is on disk, not to re-derive the policy from a
                // marker this surface never wrote. A profile whose config
                // and marker disagree keeps that disagreement until the
                // operator resolves it.

                send(
                    &events,
                    ProvisionEvent::Done {
                        summary: format!("Approval policy set: {label}"),
                    },
                )
                .await?;

                // `Configured`, not `Aborted`, even though this run changed
                // nothing: the wizard advances only after a SUCCESSFUL
                // save, and the save runs only on `Configured`. On
                // `Aborted` the save oneshot is dropped unsent, the
                // advance gate sees `Closed` instead of a saved config,
                // and `rantaiclaw setup` would never advance past the
                // approvals step on a profile that already has a preset.
                // The save this triggers writes the config unchanged.
                return Ok(ProvisionOutcome::Configured);
            }
        }

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
                filterable: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let idx = sel.first().copied().unwrap_or(1);

        #[allow(
            clippy::match_same_arms,
            reason = "arms are the menu contract; the wildcard is the out-of-range default, which coincides with option 1"
        )]
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

        // `--force` replaces the preset's policy files wholesale, so say
        // which files are regenerated — a hand edit to them is lost, and
        // silence would be a surprise.
        if self.force {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Info,
                    text: format!(
                        "`--force` regenerates `policy/autonomy.toml` and `policy/command_allowlist.toml` \
                         from the {label} preset — hand edits to them are lost."
                    ),
                },
            )
            .await?;
        }

        // `force` is passed because `write_policy_files`' own doc names
        // `rantaiclaw setup approvals --force` as its `force = true` caller,
        // and the section path makes the same call
        // (`src/onboard/section/approvals.rs`). Without it the write is
        // idempotent: the picker's answer would be dropped and the run would
        // report a preset it never applied.
        if let Some(warning) = policy_writer::write_policy_files(profile, preset, self.force)? {
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
        // above is idempotent without `--force` — an existing preset is left
        // untouched — so a profile that already had, say, Strict keeps it, and
        // claiming "set to Smart" would be a lie. Warn when they differ and
        // point at --force.
        //
        // This is now a GUARD, not the normal path: the branch above keeps an
        // existing preset instead of offering a picker, so the two can only
        // differ if the write did not take effect.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::test_support::{drive, scratch_profile, Answer};

    /// The four preset options, in the order the picker offers them. The
    /// index is the provisioner's own mapping, so a test that hardcodes
    /// "Strict" as an index would silently repoint at another preset if the
    /// menu ever reorders.
    const OPT_MANUAL: usize = 0;
    const OPT_SMART: usize = 1;
    const OPT_STRICT: usize = 2;

    fn chose_a_preset(events: &[ProvisionEvent]) -> bool {
        events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Choose { .. }))
    }

    fn done_summary(events: &[ProvisionEvent]) -> Option<&str> {
        events.iter().find_map(|e| match e {
            ProvisionEvent::Done { summary } => Some(summary.as_str()),
            _ => None,
        })
    }

    /// A profile that already has a preset on disk. The picker used to be
    /// offered here anyway, and the answer was then silently dropped: the
    /// write is idempotent without `--force`, so the old preset stayed and
    /// the overlay said "Selected: <what the operator picked>".
    #[tokio::test]
    async fn an_existing_preset_is_kept_and_no_picker_is_offered() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        policy_writer::write_policy_files(&profile, PolicyPreset::Strict, false)
            .expect("preset written");
        let mut config = Config::default();
        let level_before = config.autonomy.level;
        let always_ask_before = config.autonomy.always_ask.clone();

        // No answers scripted: a picker would have nothing to answer it.
        let t = drive(
            &ApprovalsProvisioner::new(),
            &mut config,
            &profile,
            Vec::new(),
        )
        .await;

        assert!(
            !chose_a_preset(&t.events),
            "an already-configured preset must not be offered a picker: {:?}",
            t.events
        );
        let messages = t.messages();
        assert!(
            messages.iter().any(|m| m.contains("Strict")),
            "the message must name the preset that is active: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("--force")),
            "the message must point at the way to switch: {messages:?}"
        );
        assert_eq!(
            done_summary(&t.events),
            Some("Approval policy set: Strict"),
            "the summary must name the preset that is on disk"
        );
        assert!(
            t.configured(),
            "the keep path reports `Configured`, so the wizard's save gate \
             can advance: {:?}",
            t.outcome
        );
        assert_eq!(
            policy_writer::read_active_preset(&profile.policy_dir()),
            Some(PolicyPreset::Strict),
            "the marker on disk must be untouched"
        );
        // Keeping the preset changes nothing. The runtime gate reads
        // `config.autonomy`, so mirroring the marker into it would change
        // what is enforced — and this run was asked to keep what is on
        // disk, not to re-derive the policy from the marker.
        assert_eq!(
            config.autonomy.level, level_before,
            "keeping the preset must not rewrite the config's autonomy level"
        );
        assert_eq!(
            config.autonomy.always_ask, always_ask_before,
            "keeping the preset must not rewrite the config's always_ask"
        );
    }

    /// The keep path says it is keeping the preset, so it must keep it —
    /// including the policy the runtime gate actually enforces. The gate
    /// reads `config.autonomy`, so mirroring the marker into it silently
    /// changes what is enforced on a profile whose config and marker
    /// disagree: a change the operator did not ask for.
    #[tokio::test]
    async fn keeping_the_preset_does_not_rewrite_the_saved_policy() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        // The marker on disk says Strict.
        policy_writer::write_policy_files(&profile, PolicyPreset::Strict, false)
            .expect("preset written");
        let mut config = Config::default();
        // The policy the operator actually runs says something else.
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        // `"*"` is the "always prompt" mark; a sync from the Strict marker
        // drops it, so this field detects the rewrite as well as the level
        // does.
        config.autonomy.always_ask = vec!["*".to_string()];
        let level_before = config.autonomy.level;
        let always_ask_before = config.autonomy.always_ask.clone();

        let t = drive(
            &ApprovalsProvisioner::new(),
            &mut config,
            &profile,
            Vec::new(),
        )
        .await;

        assert!(
            t.configured(),
            "the keep path reports `Configured`: {:?}",
            t.outcome
        );
        assert_eq!(
            config.autonomy.level, level_before,
            "keeping the preset must not change the level the gate enforces"
        );
        assert_eq!(
            config.autonomy.always_ask, always_ask_before,
            "keeping the preset must not change the always_ask the gate enforces"
        );
        assert_eq!(
            policy_writer::read_active_preset(&profile.policy_dir()),
            Some(PolicyPreset::Strict),
            "the marker on disk must be untouched"
        );
    }

    /// `--force` is the answer to "may this run replace what is on disk?".
    /// With it, the picker is offered and the answer is applied.
    #[tokio::test]
    async fn force_offers_the_picker_and_writes_the_chosen_preset() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        policy_writer::write_policy_files(&profile, PolicyPreset::Strict, false)
            .expect("preset written");
        let mut config = Config::default();

        let t = drive(
            &ApprovalsProvisioner::with_force(true),
            &mut config,
            &profile,
            vec![Answer::Pick(OPT_MANUAL)],
        )
        .await;

        assert!(
            chose_a_preset(&t.events),
            "a forced run must offer the picker: {:?}",
            t.events
        );
        assert!(t.configured());
        assert_eq!(
            policy_writer::read_active_preset(&profile.policy_dir()),
            Some(PolicyPreset::Manual),
            "`--force` must overwrite the existing preset"
        );
        assert_eq!(
            done_summary(&t.events),
            Some("Approval policy set: Manual"),
            "the summary must name the preset that was written"
        );
    }

    /// A profile with no preset on disk has nothing to keep, so the picker
    /// is offered and the answer is written.
    #[tokio::test]
    async fn a_profile_without_a_preset_is_offered_the_picker() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &ApprovalsProvisioner::new(),
            &mut config,
            &profile,
            vec![Answer::Pick(OPT_SMART)],
        )
        .await;

        assert!(
            chose_a_preset(&t.events),
            "a profile with no preset must be offered the picker: {:?}",
            t.events
        );
        assert!(t.configured());
        assert_eq!(
            policy_writer::read_active_preset(&profile.policy_dir()),
            Some(PolicyPreset::Smart),
            "the chosen preset must be written"
        );
        assert_eq!(
            done_summary(&t.events),
            Some("Approval policy set: Smart"),
            "the summary must name the preset that was written"
        );
    }

    /// `--force` has to land the chosen preset ON DISK, not only in the
    /// report. Three artefacts move together: the marker in
    /// `<profile>/policy/autonomy.toml`, the `config.autonomy` the runtime
    /// gate actually reads, and the allowlist file that is regenerated
    /// with the preset.
    ///
    /// Strict is the chosen preset because it is the only one whose
    /// autonomy level (`readonly`) differs from the default config's
    /// (`supervised`), so the config assertion can tell a config that
    /// followed the NEW marker from one that kept the old one.
    #[tokio::test]
    async fn force_writes_the_chosen_preset_to_the_policy_files() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        // Smart is on disk first. Its allowlist holds `curl --head *`,
        // which Strict's does not, so the regenerated file can be told
        // apart from the one that was there before.
        policy_writer::write_policy_files(&profile, PolicyPreset::Smart, false)
            .expect("preset written");
        let mut config = Config::default();

        let t = drive(
            &ApprovalsProvisioner::with_force(true),
            &mut config,
            &profile,
            vec![Answer::Pick(OPT_STRICT)],
        )
        .await;

        assert!(
            t.configured(),
            "a forced run that wrote the preset is configured: {:?}",
            t.outcome
        );
        assert_eq!(
            policy_writer::read_active_preset(&profile.policy_dir()),
            Some(PolicyPreset::Strict),
            "`--force` must write the chosen preset to the policy marker"
        );
        // The runtime gate reads `config.autonomy`, not the marker file.
        assert_eq!(
            config.autonomy.level,
            crate::security::AutonomyLevel::ReadOnly,
            "the config must follow the preset that was written"
        );
        let allowlist =
            std::fs::read_to_string(profile.policy_dir().join("command_allowlist.toml"))
                .expect("the allowlist file is written together with the preset");
        assert!(
            allowlist.contains("session_*"),
            "the allowlist must hold the chosen preset's patterns: {allowlist}"
        );
        assert!(
            !allowlist.contains("curl --head *"),
            "the allowlist must not still hold the previous preset's patterns: {allowlist}"
        );
    }

    /// `--force` replaces the preset's policy files wholesale, so the
    /// operator is told which files are regenerated — a hand edit to them
    /// is lost, and silence would be a surprise.
    #[tokio::test]
    async fn force_says_the_policy_files_are_regenerated() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        policy_writer::write_policy_files(&profile, PolicyPreset::Smart, false)
            .expect("preset written");
        let mut config = Config::default();

        let t = drive(
            &ApprovalsProvisioner::with_force(true),
            &mut config,
            &profile,
            vec![Answer::Pick(OPT_SMART)],
        )
        .await;

        let messages = t.messages();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("autonomy.toml")
                    && m.contains("command_allowlist.toml")
                    && m.contains("--force")),
            "the message must name both regenerated files and say that `--force` regenerates them: \
             {messages:?}"
        );
    }
}
