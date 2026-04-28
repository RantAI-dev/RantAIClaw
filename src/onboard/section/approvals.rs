//! Approvals setup section — prompts for an L1-L4 preset and materialises
//! the three policy files via `crate::approval::policy_writer`.
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §6 "Approval runtime" + §"L1-L4 presets".
//!
//! On `run`:
//!   * if interactive — show the four presets with one-line descriptions,
//!     prompt with `dialoguer::Select`, then write the files;
//!   * if headless — pick the L2 default, emit a hint to stderr, and bail.
//!
//! `is_already_configured` is a presence check on
//! `<profile>/policy/autonomy.toml`. Wave 5's `setup approvals --force`
//! flag will let the user bump their preset later.

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
        "Approval policy preset (L1 manual … L4 off)"
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
            // Headless default: L2 (smart, safe-by-default). We still write
            // the files so downstream sections can rely on the policy
            // directory being populated, but emit the CLI hint so the user
            // knows how to override.
            eprintln!("{}", self.headless_hint());
            PolicyPreset::L2Smart
        };
        policy_writer::write_policy_files(ctx.profile, preset, false)?;
        Ok(())
    }

    fn headless_hint(&self) -> &'static str {
        "rantaiclaw setup approvals to choose L1-L4 preset."
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
        .default(1) // L2 — recommended default per spec.
        .interact()?;
    Ok(options[idx].0)
}

fn preset_menu() -> [(PolicyPreset, &'static str); 4] {
    [
        (
            PolicyPreset::L1Manual,
            "L1 — Manual: prompt for every tool call (safest)",
        ),
        (
            PolicyPreset::L2Smart,
            "L2 — Smart: safe read-only commands pre-allowed (recommended)",
        ),
        (
            PolicyPreset::L3Strict,
            "L3 — Strict: deny-by-default, no prompts (unattended agents)",
        ),
        (
            PolicyPreset::L4Off,
            "L4 — Off: no gating at all (CI / fully-trusted only)",
        ),
    ]
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use crate::profile::ProfileManager;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn with_home<F: FnOnce()>(f: F) {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        assert!(s.headless_hint().contains("L1-L4"));
    }

    #[test]
    fn preset_menu_lists_all_four_levels() {
        let menu = preset_menu();
        assert_eq!(menu.len(), 4);
        assert_eq!(menu[0].0, PolicyPreset::L1Manual);
        assert_eq!(menu[1].0, PolicyPreset::L2Smart);
        assert_eq!(menu[2].0, PolicyPreset::L3Strict);
        assert_eq!(menu[3].0, PolicyPreset::L4Off);
    }

    #[test]
    fn is_already_configured_flips_after_write() {
        with_home(|| {
            let profile = ProfileManager::ensure("rt-approvals-section").unwrap();
            let config = Config::default();

            let section = ApprovalsSection;
            assert!(!section.is_already_configured(&profile, &config));

            policy_writer::write_policy_files(&profile, PolicyPreset::L2Smart, false).unwrap();

            assert!(section.is_already_configured(&profile, &config));
        });
    }

    #[test]
    fn headless_run_writes_files_with_l2_default() {
        with_home(|| {
            let profile = ProfileManager::ensure("rt-approvals-headless").unwrap();
            let mut config = Config::default();
            let mut ctx = SetupContext {
                profile: &profile,
                config: &mut config,
                interactive: false,
            };
            ApprovalsSection.run(&mut ctx).unwrap();

            let autonomy =
                std::fs::read_to_string(profile.policy_dir().join("autonomy.toml")).unwrap();
            assert!(autonomy.contains("preset = \"L2\""));
        });
    }
}
