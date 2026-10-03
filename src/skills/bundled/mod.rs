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
use std::path::Path;

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

/// Core skills installed for **every** profile regardless of the optional
/// starter-pack choice. These cover built-in administration the owner is
/// expected to be able to drive from chat — currently just the owner-only
/// permissions setup skill, which pairs with the `manage_permissions` tool.
pub const CORE_PACK: &[StarterPackSkill] = &[StarterPackSkill {
    slug: OWNER_PERMISSIONS_SLUG,
    display_name: "Owner Permissions Setup",
    summary: "Owner-only: manage owners + the non-owner capability ceiling from chat.",
    skill_md: include_str!("owner_permissions/SKILL.md"),
}];

/// The guest passage of the bundled owner-permissions skill, as it reads now.
const OWNER_PERMISSIONS_GUEST_PASSAGE: &str = "they always get skills, plus only the tools an owner has added to\n  the guest tool allowlist (none by default), and (for `shell`) only the";

/// The same passage as earlier releases shipped it, before a guest's tools
/// were limited to `guest_allowed_tools`. An installed copy that still holds
/// it was written by the project, not edited by the operator.
const OWNER_PERMISSIONS_OLD_GUEST_PASSAGE: &str = "they may use skills + read-only tools always, plus any tools an\n  owner has added to the guest tool allowlist, and (for `shell`) only the";

/// Slug of the bundled owner-permissions skill, the one skill whose installed
/// text is refreshed.
const OWNER_PERMISSIONS_SLUG: &str = "owner-permissions";

/// Stale tool names from before plan 531, mapped to the names the default
/// tool registry actually exposes. Plan 531 renamed a handful of tools
/// (`web_search` -> `web_search_tool`, `fetch_url` -> `http_request`,
/// `cron_schedule` -> `cron_add`, `cron_cancel` -> `cron_remove`) so each
/// request describes each tool once, in the tool's own words. An older release
/// of a bundled skill listed the old name; this map lets the refresh path
/// turn that line into the new one without rewriting the rest of the file.
const STALE_TOOL_NAMES: &[(&str, &str)] = &[
    ("web_search", "web_search_tool"),
    ("fetch_url", "http_request"),
    ("cron_schedule", "cron_add"),
    ("cron_cancel", "cron_remove"),
];

/// The exact `- name: <stale>` -> `- name: <new>` line substitution the refresh
/// performs. Whitespace is significant so an operator who wrote a different
/// line (e.g. `name: web_search` without the leading dash) is left alone.
/// The match is line-anchored: a `- name: <stale>` line is only replaced when
/// the stale name fills the whole token, so `- name: web_search_tool` is
/// never re-substituted by `- name: web_search` -> `- name: web_search_tool`.
fn stale_line_replacements(installed: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (old, new) in STALE_TOOL_NAMES {
        // Walk the file line by line. A line that starts with `- name: ` and
        // whose name token equals `<old>` exactly is the one we want.
        let mut found = None;
        for line in installed.lines() {
            let Some(rest) = line.strip_prefix("- name: ") else {
                continue;
            };
            if rest.trim() == *old {
                found = Some(line.to_string());
                break;
            }
        }
        if let Some(stale_line) = found {
            let fresh_line = stale_line.replacen(old, new, 1);
            out.push((stale_line, fresh_line));
        }
    }
    out
}

/// Replace stale tool-name lines in an installed bundled skill with the
/// current names, line by line.
///
/// `install_pack` never overwrites an installed skill, so an install made
/// before the rename still lists the old names and the model is told to call a
/// tool that is not registered. Only the named lines are substituted, and
/// only when the installed file still holds them verbatim, so a file the
/// operator rewrote is left alone and any other edit survives. The file as it
/// was is saved to `SKILL.md.bak` first; an existing `.bak` is never
/// overwritten, and then the file is left as it is. After a refresh the stale
/// lines are gone, so a second run changes nothing. The replacement is
/// written through [`crate::migration::replace_file_atomically`] so a crash
/// mid-write leaves either the old file or the new one. Returns whether the
/// file changed.
fn refresh_stale_tool_names(slug: &str, dir: &Path) -> Result<bool> {
    use std::io::Write;

    let skill_md = dir.join("SKILL.md");
    let Ok(installed) = fs::read_to_string(&skill_md) else {
        return Ok(false);
    };
    let replacements = stale_line_replacements(&installed);
    if replacements.is_empty() {
        return Ok(false);
    }

    let backup = dir.join("SKILL.md.bak");
    let mut backup_file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            tracing::warn!(
                "bundled: {slug} still carries stale tool names, but {} already exists and is never overwritten; move the .bak away and the next setup run refreshes the skill",
                backup.display()
            );
            return Ok(false);
        }
        Err(e) => return Err(e).with_context(|| format!("create {}", backup.display())),
    };
    backup_file
        .write_all(installed.as_bytes())
        .with_context(|| format!("write {}", backup.display()))?;
    backup_file
        .sync_all()
        .with_context(|| format!("sync {}", backup.display()))?;

    let mut refreshed = installed.clone();
    for (stale, fresh) in &replacements {
        refreshed = refreshed.replacen(stale, fresh, 1);
    }
    crate::migration::replace_file_atomically(&skill_md, refreshed.as_bytes())
        .with_context(|| format!("write {}", skill_md.display()))?;
    tracing::info!(
        "bundled: refreshed the tool names of {}; the previous file is {}",
        skill_md.display(),
        backup.display()
    );
    Ok(true)
}

/// Replace the old guest passage in an installed owner-permissions skill.
///
/// `install_pack` never overwrites an installed skill, so an install made
/// before the guest tool gate kept telling the model that guests get
/// "skills + read-only tools". Only that passage is replaced, and only when the
/// installed file still holds it verbatim, so a file the operator rewrote there
/// is left alone and any other edit survives. The file as it was is saved to
/// `SKILL.md.bak` first; an existing `.bak` is never overwritten, and then the
/// file is left as it is. After a refresh the old passage is gone, so a second
/// run changes nothing. The replacement is written through
/// [`crate::migration::replace_file_atomically`] so a crash mid-write leaves
/// either the old file or the new one. Returns whether the file changed.
fn refresh_old_owner_permissions_passage(dir: &Path) -> Result<bool> {
    use std::io::Write;

    let skill_md = dir.join("SKILL.md");
    let Ok(installed) = fs::read_to_string(&skill_md) else {
        return Ok(false);
    };
    if !installed.contains(OWNER_PERMISSIONS_OLD_GUEST_PASSAGE) {
        return Ok(false);
    }

    let backup = dir.join("SKILL.md.bak");
    let mut backup_file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            tracing::warn!(
                "bundled: {} still carries the old guest passage, but {} already exists and is never overwritten; move the .bak away and the next setup run refreshes the skill",
                skill_md.display(),
                backup.display()
            );
            return Ok(false);
        }
        Err(e) => return Err(e).with_context(|| format!("create {}", backup.display())),
    };
    backup_file
        .write_all(installed.as_bytes())
        .with_context(|| format!("write {}", backup.display()))?;
    backup_file
        .sync_all()
        .with_context(|| format!("sync {}", backup.display()))?;

    let refreshed = installed.replacen(
        OWNER_PERMISSIONS_OLD_GUEST_PASSAGE,
        OWNER_PERMISSIONS_GUEST_PASSAGE,
        1,
    );
    crate::migration::replace_file_atomically(&skill_md, refreshed.as_bytes())
        .with_context(|| format!("write {}", skill_md.display()))?;
    tracing::info!(
        "bundled: refreshed the guest passage of {}; the previous file is {}",
        skill_md.display(),
        backup.display()
    );
    Ok(true)
}

/// Idempotently install a set of bundled skills into the profile's `skills/`
/// directory. Returns the slugs that were newly created (dirs that did not
/// exist). Never overwrites an existing skill directory (preserves user edits).
/// Two exceptions refresh installed text without touching operator edits:
/// the old guest passage of `owner-permissions`
/// ([`refresh_old_owner_permissions_passage`]), and the pre-531 tool-name
/// lines in every starter-pack skill ([`refresh_stale_tool_names`]).
fn install_pack(profile: &Profile, pack: &[StarterPackSkill]) -> Result<Vec<String>> {
    let skills_root = profile.skills_dir();
    fs::create_dir_all(&skills_root)
        .with_context(|| format!("create skills dir {}", skills_root.display()))?;

    let mut installed = Vec::new();
    for skill in pack {
        let dir = skills_root.join(skill.slug);
        if dir.exists() {
            if skill.slug == OWNER_PERMISSIONS_SLUG {
                // Best-effort: a file we cannot rewrite must not fail setup.
                if let Err(e) = refresh_old_owner_permissions_passage(&dir) {
                    tracing::warn!("bundled: could not refresh {}: {e}", skill.slug);
                }
            } else if let Err(e) = refresh_stale_tool_names(skill.slug, &dir) {
                tracing::warn!("bundled: could not refresh {}: {e}", skill.slug);
            }
            continue;
        }
        fs::create_dir_all(&dir).with_context(|| format!("create skill dir {}", dir.display()))?;
        let skill_md = dir.join("SKILL.md");
        fs::write(&skill_md, skill.skill_md)
            .with_context(|| format!("write {}", skill_md.display()))?;
        // Only for directories we just created — the `dir.exists()` guard
        // above skips existing ones by design, and that includes their marker.
        // Best-effort: a pack member without a marker still resolves correctly
        // via the bundled slug list.
        if let Err(e) = crate::skills::origin::write_origin(
            &dir,
            &crate::skills::origin::SkillOrigin::new(
                crate::skills::origin::SkillOriginKind::Bundled,
                None,
            ),
        ) {
            tracing::warn!("bundled: could not record origin for {}: {e}", skill.slug);
        }
        installed.push(skill.slug.to_string());
    }
    Ok(installed)
}

/// Idempotently install the five-skill starter pack into the profile's
/// `skills/` directory. Returns the slugs that were newly created (i.e.
/// the ones whose directory did not exist beforehand).
///
/// Existing skill directories are left untouched — this function never
/// overwrites user edits.
pub fn install_starter_pack(profile: &Profile) -> Result<Vec<String>> {
    install_pack(profile, STARTER_PACK)
}

/// Idempotently install the always-on [`CORE_PACK`] skills. Call this on any
/// setup path that should guarantee the owner can self-serve administration
/// from chat (fresh onboarding, channel configuration).
pub fn install_core_skills(profile: &Profile) -> Result<Vec<String>> {
    install_pack(profile, CORE_PACK)
}

/// Look up a starter-pack entry by slug. Used by the wizard to render
/// summaries and by tests.
pub fn find_by_slug(slug: &str) -> Option<&'static StarterPackSkill> {
    STARTER_PACK.iter().find(|s| s.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Memory;
    use std::path::PathBuf;
    use std::sync::Arc;

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

    /// A bundled skill names only tools that exist, and none of them saves a
    /// note or a file on the model's own initiative. `memory_write` is not a
    /// tool; `memory_store` is, and the two skills that offer to keep their
    /// output say it happens when the user asks.
    #[test]
    fn bundled_skills_store_a_note_only_when_the_user_asks() {
        for skill in STARTER_PACK.iter().chain(CORE_PACK) {
            assert!(
                !skill.skill_md.contains("memory_write"),
                "{} names a tool that does not exist",
                skill.slug
            );
        }

        for slug in ["research-assistant", "meeting-notes"] {
            let skill = find_by_slug(slug).expect("the skill is in the starter pack");
            assert!(
                skill.skill_md.contains("- name: memory_store"),
                "{slug} does not list the tool that exists"
            );
            assert!(
                skill
                    .skill_md
                    .contains("with `memory_store` only when the user asks"),
                "{slug} does not say a note is stored only when asked"
            );
            for urging in ["Save the final brief", "Save the rendered notes", "memory/"] {
                assert!(
                    !skill.skill_md.contains(urging),
                    "{slug} still tells the model to save on its own: {urging:?}"
                );
            }
        }
    }

    /// Every tool a bundled SKILL.md names under `## Tools` must exist in the
    /// default tool registry. Plan 531 renamed a handful (`web_search` ->
    /// `web_search_tool`, `fetch_url` -> `http_request`, `cron_schedule` ->
    /// `cron_add`, `cron_cancel` -> `cron_remove`) so the model is never told
    /// to call a tool that is not registered.
    #[test]
    fn bundled_skills_name_only_tools_that_exist_in_the_default_registry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        let runtime: Arc<dyn crate::runtime::RuntimeAdapter> =
            Arc::from(crate::runtime::create_runtime(&config.runtime).expect("runtime builds"));
        let security = Arc::new(crate::security::SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ));
        let memory: Arc<dyn Memory> = Arc::new(
            crate::memory::SqliteMemory::new(&config.workspace_dir).expect("memory opens"),
        );
        let registry = crate::tools::all_tools_with_runtime(
            Arc::new(config.clone()),
            &security,
            runtime,
            memory,
            None,
            None,
            &config.browser,
            &config.http_request,
            &config.workspace_dir,
            &config.agents,
            config.api_key.as_deref(),
            &config,
        );
        let registry_names: std::collections::HashSet<&str> =
            registry.iter().map(|t| t.name()).collect();

        for skill in STARTER_PACK.iter().chain(CORE_PACK) {
            for name in parse_tools_section(skill.skill_md) {
                assert!(
                    registry_names.contains(name.as_str()),
                    "{} names `{}`, but the default registry does not expose it; \
                     registered names: {:?}",
                    skill.slug,
                    name,
                    registry_names
                );
            }
        }
    }

    /// Parse the `- name: X` lines from a bundled SKILL.md's `## Tools`
    /// section. Order is preserved; duplicates are reported as-is so the
    /// caller sees a useful failure instead of a silent set collapse.
    fn parse_tools_section(skill_md: &str) -> Vec<String> {
        let Some((_, rest)) = skill_md.split_once("## Tools\n") else {
            return Vec::new();
        };
        let body = rest.split("\n## ").next().unwrap_or(rest);
        body.lines()
            .filter_map(|line| {
                line.trim_start()
                    .strip_prefix("- name:")
                    .map(|rest| rest.trim().to_string())
            })
            .filter(|name| !name.is_empty())
            .collect()
    }

    #[test]
    fn install_pack_records_bundled_origin_and_leaves_existing_dirs_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = Profile {
            name: "test".to_string(),
            root: tmp.path().to_path_buf(),
        };

        // A directory that already exists must survive untouched — including
        // a marker claiming a different origin, since `install_pack` skips it.
        let squatted = profile.skills_dir().join(CORE_PACK[0].slug);
        fs::create_dir_all(&squatted).unwrap();
        fs::write(squatted.join("SKILL.md"), "user edits").unwrap();
        crate::skills::origin::write_origin(
            &squatted,
            &crate::skills::origin::SkillOrigin::new(
                crate::skills::origin::SkillOriginKind::Authored,
                None,
            ),
        )
        .unwrap();

        install_pack(&profile, STARTER_PACK).unwrap();
        install_pack(&profile, CORE_PACK).unwrap();

        for slug in STARTER_PACK.iter().map(|s| s.slug) {
            let origin = crate::skills::origin::read_origin(&profile.skills_dir().join(slug))
                .unwrap_or_else(|| panic!("{slug} has no origin marker"));
            assert_eq!(
                origin.kind,
                crate::skills::origin::SkillOriginKind::Bundled,
                "{slug}"
            );
        }

        assert_eq!(
            fs::read_to_string(squatted.join("SKILL.md")).unwrap(),
            "user edits",
            "install_pack must not overwrite an existing skill directory"
        );
        assert_eq!(
            crate::skills::origin::read_origin(&squatted).map(|o| o.kind),
            Some(crate::skills::origin::SkillOriginKind::Authored),
            "an existing directory keeps its own marker"
        );
    }

    /// The owner-permissions guest passage as earlier releases shipped it,
    /// written out here so the test does not depend on the production constant.
    const HISTORIC_PASSAGE: &str = "they may use skills + read-only tools always, plus any tools an\n  owner has added to the guest tool allowlist, and (for `shell`) only the";

    fn profile_in(tmp: &tempfile::TempDir) -> Profile {
        Profile {
            name: "test".to_string(),
            root: tmp.path().to_path_buf(),
        }
    }

    /// An installed owner-permissions file as an older release wrote it: the
    /// current text with the guest passage swapped back.
    fn install_old_owner_permissions(profile: &Profile) -> (PathBuf, String) {
        let dir = profile.skills_dir().join("owner-permissions");
        fs::create_dir_all(&dir).unwrap();
        let old = CORE_PACK[0]
            .skill_md
            .replace(OWNER_PERMISSIONS_GUEST_PASSAGE, HISTORIC_PASSAGE);
        assert_ne!(
            old, CORE_PACK[0].skill_md,
            "fixture must differ from current"
        );
        fs::write(dir.join("SKILL.md"), &old).unwrap();
        (dir, old)
    }

    #[test]
    fn bundled_owner_permissions_carries_the_current_guest_passage_only() {
        assert!(CORE_PACK[0]
            .skill_md
            .contains(OWNER_PERMISSIONS_GUEST_PASSAGE));
        assert!(!CORE_PACK[0].skill_md.contains(HISTORIC_PASSAGE));
    }

    #[test]
    fn install_core_skills_refreshes_the_old_guest_passage_and_keeps_a_backup() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = profile_in(&tmp);
        let (dir, old) = install_old_owner_permissions(&profile);

        let installed = install_core_skills(&profile).unwrap();

        assert!(installed.is_empty(), "the directory already existed");
        let refreshed = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(
            refreshed.contains(OWNER_PERMISSIONS_GUEST_PASSAGE),
            "{refreshed}"
        );
        assert!(!refreshed.contains(HISTORIC_PASSAGE), "{refreshed}");
        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md.bak")).unwrap(),
            old,
            "the backup holds the file as it was"
        );
    }

    #[test]
    fn install_core_skills_leaves_an_operator_edited_file_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = profile_in(&tmp);
        let dir = profile.skills_dir().join("owner-permissions");
        fs::create_dir_all(&dir).unwrap();
        // The operator rewrote the guest sentence, so the shipped one is gone.
        let edited = CORE_PACK[0].skill_md.replace(
            OWNER_PERMISSIONS_GUEST_PASSAGE,
            "they may use only what I list in guest_allowed_tools, and (for `shell`) only the",
        );
        fs::write(dir.join("SKILL.md"), &edited).unwrap();

        install_core_skills(&profile).unwrap();

        assert_eq!(fs::read_to_string(dir.join("SKILL.md")).unwrap(), edited);
        assert!(!dir.join("SKILL.md.bak").exists());
    }

    #[test]
    fn install_core_skills_leaves_a_current_file_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = profile_in(&tmp);
        install_core_skills(&profile).unwrap();
        let dir = profile.skills_dir().join("owner-permissions");

        install_core_skills(&profile).unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            CORE_PACK[0].skill_md
        );
        assert!(!dir.join("SKILL.md.bak").exists());
    }

    #[test]
    fn a_second_install_after_a_refresh_changes_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = profile_in(&tmp);
        let (dir, old) = install_old_owner_permissions(&profile);
        install_core_skills(&profile).unwrap();
        let after_first = fs::read_to_string(dir.join("SKILL.md")).unwrap();

        install_core_skills(&profile).unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            after_first
        );
        assert_eq!(fs::read_to_string(dir.join("SKILL.md.bak")).unwrap(), old);
    }

    #[test]
    fn an_existing_backup_is_never_overwritten() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = profile_in(&tmp);
        let (dir, old) = install_old_owner_permissions(&profile);
        fs::write(dir.join("SKILL.md.bak"), "an earlier backup").unwrap();

        install_core_skills(&profile).unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md.bak")).unwrap(),
            "an earlier backup"
        );
        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            old,
            "without a safe place for the original, the file stays as it is"
        );
    }

    #[test]
    fn a_refresh_keeps_the_operators_other_edits() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = profile_in(&tmp);
        let (dir, old) = install_old_owner_permissions(&profile);
        let edited = format!("{old}\n- Reply in French.\n");
        fs::write(dir.join("SKILL.md"), &edited).unwrap();

        install_core_skills(&profile).unwrap();

        let refreshed = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(refreshed.ends_with("\n- Reply in French.\n"), "{refreshed}");
        assert!(refreshed.contains(OWNER_PERMISSIONS_GUEST_PASSAGE));
        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md.bak")).unwrap(),
            edited
        );
    }

    /// The starter-pack SKILL.md files as earlier releases shipped them: each
    /// skill with its `## Tools` block rewritten to the pre-531 names so the
    /// refresh has something to do. Returns the slug -> (dir, pre-531 text)
    /// pairs the tests then poke at.
    fn install_old_tool_names(profile: &Profile) -> Vec<(&'static str, PathBuf, String)> {
        let swaps: &[(&'static str, &[(&str, &str)])] = &[
            (
                "web-search",
                &[
                    ("web_search_tool", "web_search"),
                    ("http_request", "fetch_url"),
                ],
            ),
            (
                "scheduler-reminders",
                &[
                    ("cron_add", "cron_schedule"),
                    ("cron_remove", "cron_cancel"),
                ],
            ),
            ("summarizer", &[("http_request", "fetch_url")]),
            (
                "research-assistant",
                &[
                    ("web_search_tool", "web_search"),
                    ("http_request", "fetch_url"),
                ],
            ),
            ("meeting-notes", &[("cron_add", "cron_schedule")]),
        ];
        let mut out = Vec::new();
        for (slug, replacements) in swaps {
            let dir = profile.skills_dir().join(slug);
            fs::create_dir_all(&dir).unwrap();
            let mut text = find_by_slug(slug)
                .expect("starter pack")
                .skill_md
                .to_string();
            for (new_name, old_name) in *replacements {
                let stale = format!("- name: {new_name}");
                let fresh = format!("- name: {old_name}");
                assert!(text.contains(&stale), "{slug}: fixture missing {stale}");
                text = text.replacen(&stale, &fresh, 1);
            }
            fs::write(dir.join("SKILL.md"), &text).unwrap();
            out.push((*slug, dir, text));
        }
        out
    }

    #[test]
    fn install_starter_pack_refreshes_pre_531_tool_names_and_keeps_a_backup() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = profile_in(&tmp);
        let mut installed = install_old_tool_names(&profile);
        installed.sort_by_key(|(slug, _, _)| *slug);

        let refreshed = install_starter_pack(&profile).unwrap();
        assert!(refreshed.is_empty(), "the directories already existed");

        for (slug, dir, old_text) in &installed {
            let current = fs::read_to_string(dir.join("SKILL.md")).unwrap();
            assert!(
                !current.contains("- name: web_search\n"),
                "{slug}: stale `web_search` line survives:\n{current}"
            );
            assert!(
                !current.contains("- name: fetch_url\n"),
                "{slug}: stale `fetch_url` line survives:\n{current}"
            );
            assert!(
                !current.contains("- name: cron_schedule\n"),
                "{slug}: stale `cron_schedule` line survives:\n{current}"
            );
            assert!(
                !current.contains("- name: cron_cancel\n"),
                "{slug}: stale `cron_cancel` line survives:\n{current}"
            );
            // The bundled name reappears.
            let bundled = find_by_slug(slug).expect("starter pack").skill_md;
            assert_eq!(
                current, bundled,
                "{slug}: refreshed SKILL.md does not match bundled SKILL.md"
            );
            assert_eq!(
                fs::read_to_string(dir.join("SKILL.md.bak")).unwrap(),
                *old_text,
                "{slug}: the backup should hold the pre-531 text"
            );
        }
    }

    #[test]
    fn install_starter_pack_leaves_a_current_skill_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = profile_in(&tmp);
        install_starter_pack(&profile).unwrap();

        install_starter_pack(&profile).unwrap();

        for skill in STARTER_PACK {
            let dir = profile.skills_dir().join(skill.slug);
            assert_eq!(
                fs::read_to_string(dir.join("SKILL.md")).unwrap(),
                skill.skill_md,
                "{}: a current install changed",
                skill.slug
            );
            assert!(!dir.join("SKILL.md.bak").exists());
        }
    }

    #[test]
    fn install_starter_pack_leaves_an_operator_edited_skill_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = profile_in(&tmp);
        // Drop in a meeting-notes install with a single stale line, so the
        // operator edit can target that line without leaving siblings that
        // the refresh would still need to substitute.
        let slug = "meeting-notes";
        let dir = profile.skills_dir().join(slug);
        fs::create_dir_all(&dir).unwrap();
        let fresh_line = "- name: cron_add\n";
        let old_text = find_by_slug(slug)
            .expect("starter pack")
            .skill_md
            .to_string();
        assert!(old_text.contains(fresh_line), "fixture premise");
        let edited = old_text.replacen(fresh_line, "- name: schedule_cron\n", 1);
        assert_ne!(edited, old_text);
        fs::write(dir.join("SKILL.md"), &edited).unwrap();

        install_starter_pack(&profile).unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            edited,
            "{slug}: an operator edit must survive"
        );
        assert!(
            !dir.join("SKILL.md.bak").exists(),
            "{slug}: a no-op refresh must not create a backup"
        );
    }

    #[test]
    fn install_starter_pack_keeps_a_pre_531_backup_when_a_backup_already_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = profile_in(&tmp);
        let installed = install_old_tool_names(&profile);
        let (slug, dir, _) = &installed[0];
        fs::write(dir.join("SKILL.md.bak"), "an earlier backup").unwrap();

        install_starter_pack(&profile).unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("SKILL.md.bak")).unwrap(),
            "an earlier backup",
            "{slug}: an existing backup must not be overwritten"
        );
        // The main file is left untouched because there was no safe place for
        // the original. This matches the owner-permissions refresh contract.
        let current = fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(
            current.contains("- name: web_search\n")
                || current.contains("- name: fetch_url\n")
                || current.contains("- name: cron_schedule\n")
                || current.contains("- name: cron_cancel\n"),
            "{slug}: the file should still carry at least one stale line, got:\n{current}"
        );
    }

    #[test]
    fn a_second_install_after_a_tool_name_refresh_changes_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let profile = profile_in(&tmp);
        install_starter_pack(&profile).unwrap();

        // Run twice — the second run must not create a backup.
        install_starter_pack(&profile).unwrap();

        for skill in STARTER_PACK {
            let dir = profile.skills_dir().join(skill.slug);
            assert_eq!(
                fs::read_to_string(dir.join("SKILL.md")).unwrap(),
                skill.skill_md
            );
            assert!(!dir.join("SKILL.md.bak").exists());
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
