//! Approvals setup section — prompts for one of the four named presets
//! (Manual / Smart / Strict / Off) and materialises the three policy
//! files via `crate::approval::policy_writer`.
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §6 "Approval runtime" + §"Preset bundles".
//!
//! On `run`:
//!   * if interactive — show the four presets with one-line descriptions,
//!     prompt with `dialoguer::Select`, then write the files;
//!   * if headless — pick the Smart default, emit a hint to stderr, and bail.
//!
//! `is_already_configured` is a presence check on
//! `<profile>/policy/autonomy.toml`. `setup approvals --force` lets the
//! user bump their preset later.

use anyhow::Result;

use super::{SetupContext, SetupSection};
use crate::approval::policy_writer::{self, PolicyPreset};
use crate::config::Config;
use crate::profile::Profile;

pub struct ApprovalsSection;

impl SetupSection for ApprovalsSection {
    fn name(&self) -> &'static str {
        "approvals"
    }

    fn description(&self) -> &'static str {
        "Approval policy preset (Manual / Smart / Strict / Off)"
    }

    /// Configured iff `autonomy.toml` already exists. The orchestrator
    /// surfaces this as `[skip / reconfigure / show]`.
    fn is_already_configured(&self, profile: &Profile, _config: &Config) -> bool {
        profile.policy_dir().join("autonomy.toml").exists()
    }

    fn run(&self, ctx: &mut SetupContext) -> Result<()> {
        let preset = if ctx.interactive {
            prompt_for_preset()?
        } else {
            // Headless default: Smart (safe-by-default). We still write
            // the files so downstream sections can rely on the policy
            // directory being populated, but emit the CLI hint so the user
            // knows how to override.
            eprintln!("{}", self.headless_hint());
            PolicyPreset::Smart
        };
        // Honor `--force`: without it an already-configured profile keeps its
        // old preset (idempotent write), so `setup approvals --force` was a
        // no-op that still claimed to set the chosen preset.
        if let Some(warning) = policy_writer::write_policy_files(ctx.profile, preset, ctx.force)? {
            eprintln!("{warning}");
        }
        // When not forcing and a different preset already exists on disk, say so
        // rather than letting the operator believe the offered preset took.
        if !ctx.force {
            if let Some(active) = policy_writer::read_active_preset(&ctx.profile.policy_dir()) {
                if active != preset {
                    eprintln!(
                        "Existing approval preset kept: {}. Re-run with --force to switch to {}.",
                        active.label(),
                        preset.label()
                    );
                }
            }
        }
        // The gate reads `config.autonomy`, not the marker. Without this the
        // preset chosen here was cosmetic: a fresh install that picked Strict
        // ran under the default `supervised` until the user re-ran `rantaiclaw
        // autonomy <preset>`. The wizard saves the config after each section.
        policy_writer::sync_config_to_active_preset(&ctx.profile.policy_dir(), ctx.config);
        Ok(())
    }

    fn headless_hint(&self) -> &'static str {
        "rantaiclaw setup approvals to choose Manual / Smart / Strict / Off preset."
    }
}

// ── Interactive prompt ──────────────────────────────────────────

fn prompt_for_preset() -> Result<PolicyPreset> {
    use dialoguer::{theme::ColorfulTheme, Select};

    let options = preset_menu();
    let labels: Vec<&str> = options.iter().map(|(_, label)| *label).collect();

    let idx = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Choose an approval policy preset")
        .items(&labels)
        .default(1) // Smart — recommended default per spec.
        .interact()?;
    Ok(options[idx].0)
}

fn preset_menu() -> [(PolicyPreset, &'static str); 4] {
    [
        (
            PolicyPreset::Manual,
            "Manual — prompt for every tool call (safest)",
        ),
        (
            PolicyPreset::Smart,
            "Smart — safe read-only commands pre-allowed (recommended)",
        ),
        (
            PolicyPreset::Strict,
            "Strict — deny-by-default, no prompts (unattended agents)",
        ),
        (
            PolicyPreset::Off,
            "Off — no gating at all (CI / fully-trusted only)",
        ),
    ]
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use crate::profile::ProfileManager;
    use tempfile::TempDir;

    // Tests mutate the process-global HOME/RANTAICLAW_PROFILE env; serialize
    // against the crate-shared ENV_LOCK so they can't clobber other tests
    // (e.g. in channels/config) that mutate the same process-global env.
    fn with_home<F: FnOnce()>(f: F) {
        let _guard = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = TempDir::new().expect("tempdir");
        let prev_home = std::env::var_os("HOME");
        let prev_profile = std::env::var_os("RANTAICLAW_PROFILE");
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("RANTAICLAW_PROFILE");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        if let Some(h) = prev_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(p) = prev_profile {
            std::env::set_var("RANTAICLAW_PROFILE", p);
        } else {
            std::env::remove_var("RANTAICLAW_PROFILE");
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn metadata_is_stable() {
        let s = ApprovalsSection;
        assert_eq!(s.name(), "approvals");
        assert!(!s.description().is_empty());
        // Headless hint references the four presets so users learn the new names.
        let hint = s.headless_hint();
        assert!(hint.contains("Manual"));
        assert!(hint.contains("Off"));
    }

    #[test]
    fn preset_menu_lists_all_four_levels() {
        let menu = preset_menu();
        assert_eq!(menu.len(), 4);
        assert_eq!(menu[0].0, PolicyPreset::Manual);
        assert_eq!(menu[1].0, PolicyPreset::Smart);
        assert_eq!(menu[2].0, PolicyPreset::Strict);
        assert_eq!(menu[3].0, PolicyPreset::Off);
    }

    #[test]
    fn is_already_configured_flips_after_write() {
        with_home(|| {
            let profile = ProfileManager::ensure("rt-approvals-section").unwrap();
            let config = Config::default();

            let section = ApprovalsSection;
            assert!(!section.is_already_configured(&profile, &config));

            policy_writer::write_policy_files(&profile, PolicyPreset::Smart, false).unwrap();

            assert!(section.is_already_configured(&profile, &config));
        });
    }

    #[test]
    fn headless_run_writes_files_with_smart_default() {
        with_home(|| {
            let profile = ProfileManager::ensure("rt-approvals-headless").unwrap();
            let mut config = Config::default();
            let mut ctx = SetupContext {
                profile: &profile,
                config: &mut config,
                interactive: false,
                force: false,
            };
            ApprovalsSection.run(&mut ctx).unwrap();

            let autonomy =
                std::fs::read_to_string(profile.policy_dir().join("autonomy.toml")).unwrap();
            assert!(autonomy.contains("preset = \"smart\""));
        });
    }

    #[test]
    fn run_mirrors_the_preset_into_config_autonomy() {
        with_home(|| {
            let profile = ProfileManager::ensure("rt-approvals-mirror").unwrap();
            let mut config = Config::default();
            // Default carries the `ssh`/`pty` always-ask pair; Smart clears it,
            // so this is an observable change rather than a coincidence with
            // the default level.
            assert!(!config.autonomy.always_ask.is_empty());

            let mut ctx = SetupContext {
                profile: &profile,
                config: &mut config,
                interactive: false,
                force: false,
            };
            ApprovalsSection.run(&mut ctx).unwrap();

            // The runtime gate reads config.autonomy — if setup leaves it at the
            // default, the preset the user picked never takes effect.
            assert!(
                config.autonomy.always_ask.is_empty(),
                "expected the Smart encoding in config, got {:?}",
                config.autonomy.always_ask
            );
        });
    }

    #[test]
    fn run_mirrors_the_marker_on_disk_not_the_offered_preset() {
        with_home(|| {
            let profile = ProfileManager::ensure("rt-approvals-existing").unwrap();
            // Pre-existing policy: Strict. `write_policy_files(force = false)`
            // will leave it alone, so the headless Smart default is NOT what
            // ends up active — the config must follow the marker, not the offer.
            policy_writer::write_policy_files(&profile, PolicyPreset::Strict, false).unwrap();

            let mut config = Config::default();
            let mut ctx = SetupContext {
                profile: &profile,
                config: &mut config,
                interactive: false,
                force: false,
            };
            ApprovalsSection.run(&mut ctx).unwrap();

            assert_eq!(
                config.autonomy.level,
                crate::security::AutonomyLevel::ReadOnly,
                "config should track the Strict marker left on disk"
            );
        });
    }

    #[test]
    fn force_overwrites_existing_preset() {
        with_home(|| {
            let profile = ProfileManager::ensure("rt-approvals-force").unwrap();
            // Pre-existing Strict on disk.
            policy_writer::write_policy_files(&profile, PolicyPreset::Strict, false).unwrap();
            assert_eq!(
                policy_writer::read_active_preset(&profile.policy_dir()),
                Some(PolicyPreset::Strict)
            );

            let mut config = Config::default();
            let mut ctx = SetupContext {
                profile: &profile,
                config: &mut config,
                interactive: false, // headless → Smart default
                force: true,
            };
            ApprovalsSection.run(&mut ctx).unwrap();

            // `--force` must actually overwrite Strict → Smart, not no-op.
            assert_eq!(
                policy_writer::read_active_preset(&profile.policy_dir()),
                Some(PolicyPreset::Smart),
                "--force should overwrite the existing preset"
            );
        });
    }
}
