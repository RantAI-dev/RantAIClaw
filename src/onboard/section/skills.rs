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
use crate::skills::clawhub;

const HEADLESS_HINT: &str =
    "rantaiclaw setup skills --starter-pack         # install the 5 bundled skills\n  \
     rantaiclaw setup skills --skip                  # skip skills setup entirely";

pub struct SkillsSection;

impl SetupSection for SkillsSection {
    fn name(&self) -> &'static str {
        "skills"
    }

    fn description(&self) -> &'static str {
        "Bundled 5-skill starter pack + optional ClawHub multi-select"
    }

    fn is_already_configured(&self, profile: &Profile, _: &Config) -> bool {
        // "Already configured" = at least one starter-pack skill is on disk.
        // Wave 3's orchestrator will use this to offer the [skip / reconfigure]
        // prompt.
        let dir = profile.skills_dir();
        STARTER_PACK.iter().any(|s| dir.join(s.slug).exists())
    }

    fn run(&self, ctx: &mut SetupContext) -> Result<()> {
        if ctx.interactive {
            run_interactive(ctx.profile)?;
        } else {
            // Headless: install the starter pack idempotently. The wizard's
            // headless flag handler in main.rs will gate this — by the time
            // we get here, the user opted in.
            let installed = bundled::install_starter_pack(ctx.profile)?;
            if !installed.is_empty() {
                eprintln!(
                    "Installed starter pack ({}): {}",
                    installed.len(),
                    installed.join(", ")
                );
            }
        }
        Ok(())
    }

    fn headless_hint(&self) -> &'static str {
        HEADLESS_HINT
    }
}

/// Interactive flow — split out so it can be reused by `rantaiclaw setup
/// skills` once Wave 3 wires the subcommand.
fn run_interactive(profile: &Profile) -> Result<()> {
    use dialoguer::theme::ColorfulTheme;
    use dialoguer::{Confirm, MultiSelect};

    let theme = ColorfulTheme::default();

    let install_pack = Confirm::with_theme(&theme)
        .with_prompt("Install the recommended starter pack? (5 skills)")
        .default(true)
        .interact()
        .unwrap_or(true);

    if install_pack {
        let installed = bundled::install_starter_pack(profile)?;
        if installed.is_empty() {
            println!("  All 5 starter-pack skills already present — nothing to do.");
        } else {
            println!("  Installed: {}", installed.join(", "));
        }
    }

    let browse = Confirm::with_theme(&theme)
        .with_prompt("Browse ClawHub for more skills?")
        .default(false)
        .interact()
        .unwrap_or(false);

    if browse {
        // Fetch top-20 in a one-shot blocking runtime so we don't bleed
        // async assumptions into Wave 3's still-synchronous orchestrator.
        let top = match block_on_clawhub_list_top(20) {
            Ok(items) => items,
            Err(err) => {
                eprintln!("ClawHub fetch failed: {err}; skipping browse step.");
                return Ok(());
            }
        };

        if top.is_empty() {
            println!("  ClawHub returned no skills.");
            return Ok(());
        }

        let labels: Vec<String> = top
            .iter()
            .map(|s| {
                let name = if s.display_name.is_empty() {
                    s.slug.as_str()
                } else {
                    s.display_name.as_str()
                };
                if s.summary.is_empty() {
                    format!("{name}  ({}*)", s.stats.stars)
                } else {
                    format!("{name}  ({}*) — {}", s.stats.stars, s.summary)
                }
            })
            .collect();

        let picks = MultiSelect::with_theme(&theme)
            .with_prompt("Select skills to install (space to toggle, Enter to confirm)")
            .items(&labels)
            .interact()
            .unwrap_or_default();

        if picks.is_empty() {
            println!("  No skills selected.");
            return Ok(());
        }

        let slugs: Vec<String> = picks.into_iter().map(|i| top[i].slug.clone()).collect();
        match block_on_clawhub_install_many(profile, &slugs) {
            Ok(installed) => println!("  Installed from ClawHub: {}", installed.join(", ")),
            Err(err) => eprintln!("  ClawHub install failed: {err}"),
        }
    }

    Ok(())
}

fn block_on_clawhub_list_top(n: usize) -> Result<Vec<clawhub::ClawHubSkill>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(clawhub::list_top(n))
}

fn block_on_clawhub_install_many(profile: &Profile, slugs: &[String]) -> Result<Vec<String>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(clawhub::install_many(profile, slugs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use tempfile::TempDir;

    static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn with_home<F: FnOnce()>(f: F) {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
