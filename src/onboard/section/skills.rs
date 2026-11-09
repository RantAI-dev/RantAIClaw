//! `setup skills` section — install the bundled starter pack, then offer
//! a multi-select picker over ClawHub's top-stars listing.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Section 4 — skills (NEW)".
//!
//! Wave 2 ships against the synchronous `_stub::SetupSection` trait. Wave 3
//! will replace the trait with the orchestrator's real one — the body of
//! `run` should survive that transition unchanged because the core flow
//! (prompt → bundled install → optional ClawHub picker) is trait-agnostic.

use anyhow::Result;

use crate::config::Config;
use crate::onboard::section::{SetupContext, SetupSection};
use crate::profile::Profile;
use crate::skills::bundled::{self, STARTER_PACK};

/// The command an operator runs instead, shown when the section could not
/// prompt.
///
/// The comment names no count of skills on purpose. The number was
/// hand-written and could not keep itself true: the starter pack grows
/// without this text being touched, and the command installs the
/// always-on core skill on top of the starter pack.
const HEADLESS_HINT: &str =
    "rantaiclaw setup skills --starter-pack         # install the bundled skills\n  \
     rantaiclaw setup skills --skip                  # skip skills setup entirely";

pub struct SkillsSection;

impl SetupSection for SkillsSection {
    fn name(&self) -> &'static str {
        "skills"
    }

    fn description(&self) -> &'static str {
        "Bundled starter pack + optional ClawHub multi-select"
    }

    fn is_already_configured(&self, profile: &Profile, _: &Config) -> bool {
        // "Already configured" = at least one starter-pack skill is on disk.
        // Wave 3's orchestrator will use this to offer the [skip / reconfigure]
        // prompt.
        let dir = profile.skills_dir();
        STARTER_PACK.iter().any(|s| dir.join(s.slug).exists())
    }

    fn run(&self, ctx: &mut SetupContext) -> Result<()> {
        // Always-on core skills (owner permissions setup) — independent of the
        // optional starter-pack choice below.
        let core = bundled::install_core_skills(ctx.profile)?;
        if !core.is_empty() {
            eprintln!("Installed core skill(s): {}", core.join(", "));
        }
        // Install the starter pack idempotently. There is no interactive
        // branch here: `setup` only reaches this section when stdin is not a
        // terminal (see `main.rs`), and with a terminal it launches the TUI
        // setup overlay instead. ClawHub browsing lives in the TUI's
        // `/skills install`, which the overlay points at.
        let installed = bundled::install_starter_pack(ctx.profile)?;
        if !installed.is_empty() {
            eprintln!(
                "Installed starter pack ({}): {}",
                installed.len(),
                installed.join(", ")
            );
        }
        eprintln!("Browse ClawHub from the TUI with `/skills install`.");
        Ok(())
    }

    fn headless_hint(&self) -> &'static str {
        HEADLESS_HINT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    // Tests mutate the process-global HOME env; serialize against the
    // crate-shared ENV_LOCK so they can't clobber other tests (e.g. in
    // channels/config) that mutate the same process-global env.
    fn with_home<F: FnOnce()>(f: F) {
        let _guard = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("RANTAICLAW_PROFILE");
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        if let Some(h) = prev {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
        if let Err(e) = r {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn metadata() {
        let s = SkillsSection;
        assert_eq!(s.name(), "skills");
        assert!(s.description().contains("starter pack"));
        assert!(s.headless_hint().contains("--starter-pack"));
    }

    /// True when `text` states how many skills something holds.
    ///
    /// Every count these texts carried stood next to the word "skill"
    /// ("the 5 bundled skills", "5-skill starter pack", "All 5
    /// starter-pack skills"), so a bare number in a text that talks about
    /// skills is that shape.
    fn states_a_skill_count(text: &str) -> bool {
        let words: Vec<String> = text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_ascii_lowercase())
            .collect();
        let mentions_skills = words.iter().any(|w| w.starts_with("skill"));
        mentions_skills && words.iter().any(|w| w.chars().all(|c| c.is_ascii_digit()))
    }

    /// The onboarding skills text states no count of skills, and keeps the
    /// commands and the offer it always carried.
    ///
    /// The number was hand-written and could not keep itself true: the
    /// starter pack grows without this text being touched, and `setup
    /// skills --starter-pack` installs the starter pack *and* the
    /// always-on core skill, so "5" was never the number of skills an
    /// operator ends up with.
    #[test]
    fn onboarding_skills_text_states_no_skill_count() {
        let s = SkillsSection;
        let texts = [
            ("headless hint", s.headless_hint()),
            ("section description", s.description()),
            (
                "provision description",
                crate::onboard::provision::skills::SKILLS_DESC,
            ),
            (
                "provision install label",
                crate::onboard::provision::skills::SKILLS_INSTALL_LABEL,
            ),
            (
                "provision already-present message",
                crate::onboard::provision::skills::SKILLS_ALREADY_PRESENT,
            ),
        ];

        for (what, text) in texts {
            assert!(
                !states_a_skill_count(text),
                "{what} states a skill count: {text:?}"
            );
        }

        // The texts keep what the operator needs: the two commands that set
        // skills up, the promise that a starter pack is on offer, and the
        // ClawHub picker behind it. The hint keeps its shape: two commands,
        // the second one indented under the first, each with a trailing
        // comment.
        let hint = s.headless_hint();
        let lines: Vec<&str> = hint.lines().collect();
        assert_eq!(lines.len(), 2, "hint must stay two lines: {hint:?}");
        assert!(!lines[0].starts_with(' '), "{hint:?}");
        assert!(lines[1].starts_with("  "), "{hint:?}");
        for line in &lines {
            assert!(
                line.trim_start().starts_with("rantaiclaw setup skills"),
                "{line:?}"
            );
        }
        assert!(lines[0].contains("--starter-pack"), "{hint:?}");
        assert!(lines[1].contains("--skip"), "{hint:?}");

        assert!(s.description().contains("starter pack"));
        assert!(s.description().contains("ClawHub"));
        assert!(crate::onboard::provision::skills::SKILLS_DESC.contains("starter pack"));
        assert!(crate::onboard::provision::skills::SKILLS_DESC.contains("ClawHub"));
        assert!(crate::onboard::provision::skills::SKILLS_INSTALL_LABEL.contains("starter pack"));
    }

    #[test]
    fn is_already_configured_false_on_empty_profile() {
        with_home(|| {
            let profile = crate::profile::ProfileManager::ensure_default().unwrap();
            let cfg = Config::default();
            let s = SkillsSection;
            assert!(!s.is_already_configured(&profile, &cfg));
        });
    }

    #[test]
    fn is_already_configured_true_after_install() {
        with_home(|| {
            let profile = crate::profile::ProfileManager::ensure_default().unwrap();
            let cfg = Config::default();
            let installed = bundled::install_starter_pack(&profile).unwrap();
            assert!(!installed.is_empty());
            let s = SkillsSection;
            assert!(s.is_already_configured(&profile, &cfg));
        });
    }
}
