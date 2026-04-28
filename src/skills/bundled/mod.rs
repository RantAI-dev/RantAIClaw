//! Bundled starter-pack skills shipped inside the `rantaiclaw` binary via
//! `include_str!`. Used by the `setup skills` section to seed a fresh
//! profile with five general-assistant skills on first run.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Section 4 — skills (NEW)" and §"Skills bootstrap".
//!
//! Maintainer rule: **no coding skills in the starter pack** — the goal is
//! a useful general-purpose assistant out of the box, not a code agent.

use std::fs;

use anyhow::{Context, Result};

use crate::profile::Profile;

/// One bundled skill, embedded at compile time.
#[derive(Debug, Clone, Copy)]
pub struct StarterPackSkill {
    /// Filesystem-safe identifier; becomes the directory name under
    /// `<profile>/skills/<slug>/`.
    pub slug: &'static str,
    /// Human-readable name for the multi-select UI.
    pub display_name: &'static str,
    /// One-line summary for the multi-select UI.
    pub summary: &'static str,
    /// Full `SKILL.md` content embedded via `include_str!`.
    pub skill_md: &'static str,
}

/// The five-skill general-assistant starter pack. Order = display order in
/// the wizard. Adding a sixth: append here, ship a new SKILL.md, update tests.
pub const STARTER_PACK: &[StarterPackSkill] = &[
    StarterPackSkill {
        slug: "web-search",
        display_name: "Web Search",
        summary: "Multi-source web research with citations.",
        skill_md: include_str!("web_search/SKILL.md"),
    },
    StarterPackSkill {
        slug: "scheduler-reminders",
        display_name: "Scheduler & Reminders",
        summary: "Cron-driven reminders, time-aware scheduling.",
        skill_md: include_str!("scheduler_reminders/SKILL.md"),
    },
    StarterPackSkill {
        slug: "summarizer",
        display_name: "Summarizer",
        summary: "Long-document and meeting summarization.",
        skill_md: include_str!("summarizer/SKILL.md"),
    },
    StarterPackSkill {
        slug: "research-assistant",
        display_name: "Research Assistant",
        summary: "Deep research with structured outlines.",
        skill_md: include_str!("research_assistant/SKILL.md"),
    },
    StarterPackSkill {
        slug: "meeting-notes",
        display_name: "Meeting Notes",
        summary: "Capture, organize, and follow up on meeting notes.",
        skill_md: include_str!("meeting_notes/SKILL.md"),
    },
];

/// Idempotently install the five-skill starter pack into the profile's
/// `skills/` directory. Returns the slugs that were newly created (i.e.
/// the ones whose directory did not exist beforehand).
///
/// Existing skill directories are left untouched — this function never
/// overwrites user edits.
pub fn install_starter_pack(profile: &Profile) -> Result<Vec<String>> {
    let skills_root = profile.skills_dir();
    fs::create_dir_all(&skills_root)
        .with_context(|| format!("create skills dir {}", skills_root.display()))?;

    let mut installed = Vec::new();
    for skill in STARTER_PACK {
        let dir = skills_root.join(skill.slug);
        if dir.exists() {
            continue;
        }
        fs::create_dir_all(&dir)
            .with_context(|| format!("create skill dir {}", dir.display()))?;
        let skill_md = dir.join("SKILL.md");
        fs::write(&skill_md, skill.skill_md)
            .with_context(|| format!("write {}", skill_md.display()))?;
        installed.push(skill.slug.to_string());
    }
    Ok(installed)
}

/// Look up a starter-pack entry by slug. Used by the wizard to render
/// summaries and by tests.
pub fn find_by_slug(slug: &str) -> Option<&'static StarterPackSkill> {
    STARTER_PACK.iter().find(|s| s.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_pack_has_exactly_five_skills() {
        assert_eq!(STARTER_PACK.len(), 5);
    }

    #[test]
    fn starter_pack_slugs_are_unique() {
        let mut slugs: Vec<&str> = STARTER_PACK.iter().map(|s| s.slug).collect();
        slugs.sort_unstable();
        let len_before = slugs.len();
        slugs.dedup();
        assert_eq!(len_before, slugs.len(), "duplicate slug in starter pack");
    }

    #[test]
    fn every_skill_md_is_non_empty_and_starts_with_heading() {
        for s in STARTER_PACK {
            assert!(!s.skill_md.is_empty(), "{} has empty skill_md", s.slug);
            assert!(
                s.skill_md.trim_start().starts_with('#'),
                "{} SKILL.md must start with a markdown heading",
                s.slug
            );
        }
    }

    #[test]
    fn no_coding_skills_in_starter_pack() {
        // Maintainer-mandated invariant. If you add a code/programming skill,
        // this test should fail and a separate "code pack" should be added.
        let banned = ["code", "coding", "programmer", "developer", "git"];
        for s in STARTER_PACK {
            let slug_lower = s.slug.to_ascii_lowercase();
            for word in banned {
                assert!(
                    !slug_lower.contains(word),
                    "starter pack must not include coding skill {:?}",
                    s.slug
                );
            }
        }
    }

    #[test]
    fn find_by_slug_roundtrip() {
        for s in STARTER_PACK {
            let found = find_by_slug(s.slug).expect("slug present");
            assert_eq!(found.slug, s.slug);
        }
        assert!(find_by_slug("does-not-exist").is_none());
    }
}
