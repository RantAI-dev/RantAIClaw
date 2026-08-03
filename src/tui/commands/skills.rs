use anyhow::Result;

use super::{normalise_skill_name, CommandHandler, CommandResult};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

/// Built-in personality presets surfaced in the `/personality` picker.
/// Each tuple is `(key, summary shown as the muted secondary line)`.
const PERSONALITY_PRESETS: &[(&str, &str)] = &[
    ("default", "Balanced general-purpose assistant"),
    ("concise", "Terse responses, minimal preamble"),
    ("verbose", "Detailed explanations and rationale"),
    ("executive-assistant", "Calendar, email, scheduling focus"),
    ("friendly-companion", "Warm, conversational tone"),
];

/// Template for `/skill new`. Matches the shape the console's Form view
/// expects (`## Instructions` with `- ` items), so a skill written here stays
/// form-editable there.
fn new_skill_template(name: &str) -> String {
    format!("---\nname: {name}\ndescription: \ntags: []\n---\n\n# {name}\n\n## Instructions\n- \n")
}

/// `/skill new "<name>"` — stage a template and hand it to `$EDITOR`.
///
/// Everything that can be refused is refused *here*, before an editor opens or
/// a file is staged: failing at this point costs the user nothing, whereas
/// failing after they have typed costs them their work.
fn skill_new(arg: &str, ctx: &TuiContext) -> Result<CommandResult> {
    let name = arg.trim().trim_matches(['"', '\'']).trim();
    if name.is_empty() {
        return Ok(CommandResult::Message(
            "Usage: /skill new \"<name>\"".to_string(),
        ));
    }

    let slug = crate::tools::author_skill::slugify(name);
    if slug.is_empty() {
        return Ok(CommandResult::Message(format!(
            "'{name}' has no characters usable in a folder name."
        )));
    }

    // Both keys, across every loaded skill: two different display names can
    // slugify to one directory, so checking names alone leaves the other
    // collision reachable — and a shadowed skill stops working with no error.
    let loaded = active_skill_status_from_context(ctx);
    if let Some((clash, _)) = loaded
        .iter()
        .find(|(s, _)| s.name.eq_ignore_ascii_case(name))
    {
        return Ok(CommandResult::Message(format!(
            "A skill named '{}' already exists.",
            clash.name
        )));
    }
    if let Some((clash, _)) = loaded
        .iter()
        .find(|(s, _)| s.slug().is_some_and(|d| d.eq_ignore_ascii_case(&slug)))
    {
        return Ok(CommandResult::Message(format!(
            "'{name}' would use folder '{slug}', which '{}' already occupies.",
            clash.name
        )));
    }

    let profile = crate::profile::ProfileManager::active()?;
    let path = profile.skills_dir().join(&slug).join("SKILL.md");
    if path.exists() {
        return Ok(CommandResult::Message(format!(
            "Folder '{slug}' already exists."
        )));
    }

    Ok(CommandResult::OpenSkillInEditor {
        slug,
        path,
        initial: new_skill_template(name),
        is_new: true,
    })
}

/// `/skill edit <name-or-slug>` — open an existing skill you authored.
///
/// Accepts either spelling because they differ: `normalise_skill_name` only
/// lowercases and maps `-` to `_`, leaving spaces alone, so a skill displayed
/// as "Kopi Pagi" and its own folder `kopi-pagi` never match each other. The
/// folder name is what the user sees on disk and in the console, so it has to
/// work here.
fn skill_edit(arg: &str, ctx: &TuiContext) -> Result<CommandResult> {
    let query = arg.trim().trim_matches(['"', '\'']).trim();
    if query.is_empty() {
        return Ok(CommandResult::Message(
            "Usage: /skill edit <name>".to_string(),
        ));
    }

    let loaded = active_skill_status_from_context(ctx);
    let wanted = normalise_skill_name(query);
    let found = loaded
        .iter()
        .find(|(s, _)| normalise_skill_name(&s.name) == wanted)
        .or_else(|| {
            loaded
                .iter()
                .find(|(s, _)| s.slug().is_some_and(|d| d.eq_ignore_ascii_case(query)))
        });

    let Some((skill, _)) = found else {
        return Ok(CommandResult::Message(format!(
            "No skill '{query}'. Run /skills to see what is installed."
        )));
    };

    // A skill body becomes the agent's standing instructions on the next load,
    // and editing one someone else manages loses the work silently: a bundled
    // skill is re-seeded by the next setup run, a vendor-managed one by its
    // installer.
    let kind = skill.origin.as_ref().map(|o| o.kind);
    if kind != Some(crate::skills::origin::SkillOriginKind::Authored) {
        let managed_by = match kind {
            Some(crate::skills::origin::SkillOriginKind::Clawhub) => "ClawHub",
            Some(crate::skills::origin::SkillOriginKind::Bundled) => "a bundled pack",
            Some(crate::skills::origin::SkillOriginKind::Git) => "a git remote",
            Some(crate::skills::origin::SkillOriginKind::Local) => "a local-path install",
            _ => "an unrecorded source",
        };
        return Ok(CommandResult::Message(format!(
            "'{}' is managed by {managed_by} — not editable here.",
            skill.name
        )));
    }

    let Some(path) = skill.location.clone() else {
        return Ok(CommandResult::Message(format!(
            "'{}' has no file on disk.",
            skill.name
        )));
    };
    let initial = std::fs::read_to_string(&path)?;
    let slug = skill.slug().unwrap_or_else(|| skill.name.clone());

    Ok(CommandResult::OpenSkillInEditor {
        slug,
        path,
        initial,
        is_new: false,
    })
}

/// Build picker rows from the loaded skills list. Primary text is the
/// skill name + version; secondary is the description (truncated by
/// the renderer if too long).
fn active_skill_status_from_context(ctx: &TuiContext) -> Vec<(crate::skills::Skill, Vec<String>)> {
    if ctx.available_skills_with_status.is_empty() {
        ctx.available_skills
            .iter()
            .cloned()
            .map(|skill| (skill, Vec::new()))
            .collect()
    } else {
        ctx.available_skills_with_status.clone()
    }
}

/// Short provenance tag for a picker row, or `None` when the gateway recorded
/// no origin. `@handle` for a ClawHub install so the publisher stays visible
/// after the install screen is gone; a bare word for the rest.
pub(crate) fn skill_origin_label(s: &crate::skills::Skill) -> Option<String> {
    use crate::skills::origin::SkillOriginKind;
    let origin = s.origin.as_ref()?;
    match origin.kind {
        SkillOriginKind::Authored => Some("yours".to_string()),
        // `source` is the `@owner/slug` reference the install ran with. Fall
        // back to the bare word when it is missing rather than dropping the
        // tag: "installed from ClawHub, publisher unrecorded" is still worth
        // more than a row that says nothing about where it came from.
        SkillOriginKind::Clawhub => Some(
            origin
                .source
                .as_deref()
                .and_then(clawhub_handle)
                .map_or_else(|| "clawhub".to_string(), |h| format!("@{h}")),
        ),
        SkillOriginKind::Bundled => Some("bundled".to_string()),
        SkillOriginKind::Git => Some("git".to_string()),
        SkillOriginKind::Local => Some("local".to_string()),
    }
}

/// Publisher handle out of an `@owner/slug` reference.
fn clawhub_handle(source: &str) -> Option<&str> {
    let handle = source.trim().strip_prefix('@')?.split('/').next()?;
    (!handle.is_empty()).then_some(handle)
}

pub(crate) fn build_skill_items(
    skills: &[(crate::skills::Skill, Vec<String>)],
) -> Vec<ListPickerItem> {
    skills
        .iter()
        .map(|(s, reasons)| {
            let primary = if s.version.is_empty() {
                s.name.clone()
            } else {
                format!("{} · v{}", s.name, s.version)
            };
            let primary = if reasons.is_empty() {
                primary
            } else {
                format!("✗ {primary}")
            };
            // Where this copy came from. Two skills with the same name do
            // different things depending on who wrote them, and after
            // installing `@steipete/weather` the row read simply `weather` —
            // the publisher you chose on the install screen was gone by the
            // time you looked at what you had. Absent origin says nothing
            // rather than guessing: unrecorded is not the same as bundled.
            let primary = match skill_origin_label(s) {
                Some(label) => format!("{primary}  {label}"),
                None => primary,
            };
            let mut secondary = s.description.clone();
            if !reasons.is_empty() {
                let reason = reasons.join("; ");
                secondary = if secondary.is_empty() {
                    format!("gated: {reason}")
                } else {
                    format!("{secondary}  · gated: {reason}")
                };
            }
            if !s.tags.is_empty() {
                secondary = format!("{secondary}  ({})", s.tags.join(", "));
            }
            let has_missing_bin = s
                .requires
                .unmet()
                .iter()
                .any(|reason| reason.starts_with("missing binary"));
            if has_missing_bin && !s.install_recipes.is_empty() {
                secondary = if secondary.is_empty() {
                    "Ctrl+I install deps".to_string()
                } else {
                    format!("{secondary}  · Ctrl+I install deps")
                };
            }
            ListPickerItem {
                key: s.name.clone(),
                primary,
                secondary,
            }
        })
        .collect()
}

/// /skills command — open the interactive skills picker. Selecting a
/// skill pre-fills `Use the <name> skill: ` into the input buffer so
/// the user can complete the prompt and submit.
pub struct SkillsCommand;

impl CommandHandler for SkillsCommand {
    fn name(&self) -> &str {
        "skills"
    }

    fn description(&self) -> &str {
        "Browse available skills"
    }

    fn usage(&self) -> &str {
        "/skills [name] | new \"<name>\" | edit <name> | install [query]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        // `/skills install [query]` is an alias for `/install [query]` so
        // both discoverability paths reach the ClawHub browser. Anything
        // else (no args, or args that aren't `install*`) opens the local
        // skills picker as before.
        let trimmed = args.trim();
        // `new` and `edit` route to the same handlers as the singular. The
        // two commands are one letter apart and were never going to be
        // remembered separately: `/skills new "Kopi Pagi"` used to fall
        // through to the picker, which preselected nothing and said nothing,
        // so the request vanished without a word.
        if let Some(rest) = trimmed.strip_prefix("new") {
            return skill_new(rest.trim(), ctx);
        }
        if let Some(rest) = trimmed.strip_prefix("edit") {
            return skill_edit(rest.trim(), ctx);
        }
        if let Some(rest) = trimmed.strip_prefix("install") {
            let query = rest.trim();
            let initial_query = if query.is_empty() {
                None
            } else {
                Some(query.to_string())
            };
            return Ok(CommandResult::OpenClawhubInstallPicker { initial_query });
        }

        let status = active_skill_status_from_context(ctx);
        let items = build_skill_items(&status);
        // Honour a name argument instead of dropping it. `/skills <name>` used
        // to build the picker with `preselect_key: None`, so the arg vanished
        // without a word — one letter away from `/skill <name>`, which opens
        // that skill's detail panel. Same word, silently different outcomes.
        // The picker matches `preselect_key` against `ListPickerItem.key`
        // (the exact `s.name`), so resolve the arg through
        // `normalise_skill_name` to the matching skill's exact name first —
        // otherwise `/skills Image-Lab` wouldn't preselect a skill named
        // `image-lab`. A name that matches nothing falls back to the raw
        // arg, which preselects nothing (today's behaviour, preserved).
        let preselect = (!trimmed.is_empty()).then(|| {
            let key = normalise_skill_name(trimmed);
            status
                .iter()
                .find(|(s, _)| normalise_skill_name(&s.name) == key)
                .map(|(s, _)| s.name.clone())
                .unwrap_or_else(|| trimmed.to_string())
        });
        let picker = ListPicker::new(
            ListPickerKind::Skill,
            "Skills",
            items,
            preselect.as_deref(),
            "No skills loaded. Drop a SKILL.md in ~/.rantaiclaw/profiles/<profile>/skills/<name>/, or run `/setup skills`.",
        );
        Ok(CommandResult::OpenListPicker(picker))
    }
}

/// /skill command — same as `/skills` when no args; with a name arg,
/// pre-fills the invocation prompt directly without opening the picker.
pub struct SkillCommand;

impl CommandHandler for SkillCommand {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Invoke, inspect, write, or edit a skill"
    }

    fn usage(&self) -> &str {
        "/skill <name> (details) | new \"<name>\" | edit <name> | install [query]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        use crate::tui::widgets::{InfoPanel, InfoSection};

        let trimmed = args.trim();

        // `/skill install [query]` mirrors `/skills install [query]` so
        // typing the singular doesn't fall through to the
        // "/<skill-name>" lookup. Tester ask: "/skill should mirror
        // /skills" — same args, same surfaces.
        if let Some(rest) = trimmed.strip_prefix("install") {
            let query = rest.trim();
            let initial_query = if query.is_empty() {
                None
            } else {
                Some(query.to_string())
            };
            return Ok(CommandResult::OpenClawhubInstallPicker { initial_query });
        }

        if let Some(rest) = trimmed.strip_prefix("new") {
            return skill_new(rest.trim(), ctx);
        }
        if let Some(rest) = trimmed.strip_prefix("edit") {
            return skill_edit(rest.trim(), ctx);
        }

        let name = trimmed;
        if name.is_empty() {
            // /skill (no args) — open the same interactive picker as `/skills`.
            // Pre-v0.6.23 this opened a static InfoPanel which felt out of
            // place next to the picker that `/skills` shows. Tester ask:
            // "make /skill same as /skills (with the same UI)".
            let status = active_skill_status_from_context(ctx);
            let items = build_skill_items(&status);
            let picker = ListPicker::new(
                ListPickerKind::Skill,
                "Skills",
                items,
                None,
                "No skills loaded. Drop a SKILL.md in \
                 ~/.rantaiclaw/profiles/<profile>/skills/<name>/, or run \
                 `/skills install <slug>` to grab one from ClawHub.",
            );
            return Ok(CommandResult::OpenListPicker(picker));
        }

        // With a name arg, find it — routed through `normalise_skill_name` so
        // `/skill Image_Lab` matches a skill named `image-lab`. Search
        // `available_skills` (active) first; a gated/disabled skill won't
        // be there (it's excluded by `load_skills_with_config`), so fall
        // back to `available_skills_with_status` — which carries every
        // disk-loaded skill plus its gating reasons — so a gated skill can
        // still be inspected instead of returning "No skill named". Only
        // the not-found message is returned when it appears in neither.
        let key = normalise_skill_name(name);
        let found = ctx
            .available_skills
            .iter()
            .find(|s| normalise_skill_name(&s.name) == key)
            .map(|s| (s.clone(), Vec::new()))
            .or_else(|| {
                active_skill_status_from_context(ctx)
                    .into_iter()
                    .find(|(s, _)| normalise_skill_name(&s.name) == key)
            });
        match found {
            Some((s, reasons)) => {
                let mut panel = InfoPanel::new(format!("Skill · {}", s.name))
                    .with_subtitle(if s.version.is_empty() {
                        "no version".to_string()
                    } else {
                        format!("v{}", s.version)
                    })
                    .with_footer("Esc close · `/skills` for full picker");
                let mut sec = InfoSection::new("Detail");
                if !s.description.is_empty() {
                    sec = sec.plain(s.description.clone());
                }
                if !reasons.is_empty() {
                    sec = sec.spacer().key_value("Gated", reasons.join("; "));
                }
                if !s.tags.is_empty() {
                    sec = sec.spacer().key_value("Tags", s.tags.join(", "));
                }
                // Who put this on disk, and where. The panel used to answer
                // neither, so a skill installed from ClawHub was
                // indistinguishable from one shipped in the box — and the two
                // differ in who can change what the agent will read.
                if let Some(origin) = skill_origin_label(&s) {
                    sec = sec.spacer().key_value("Source", origin);
                }
                if let Some(slug) = s.slug() {
                    if slug != s.name {
                        sec = sec.key_value("Folder", slug);
                    }
                }
                if !s.tools.is_empty() {
                    let names: Vec<&str> = s.tools.iter().map(|t| t.name.as_str()).collect();
                    sec = sec.key_value("Tools", names.join(", "));
                }
                // Naming the skill makes this true of the skill in front of
                // you. It used to read `e.g. summarize today's standup notes`
                // on every panel, so the weather skill explained itself with a
                // summarizer's example.
                panel = panel.section(sec).section(
                    InfoSection::new("Activate")
                        .plain("Describe the task and the agent reaches for this skill on its own.")
                        .spacer()
                        .key_value("Force it", format!("Use the {} skill: <task>", s.name)),
                );
                Ok(CommandResult::OpenInfoPanel(panel))
            }
            None => Ok(CommandResult::Message(format!(
                "No skill named '{name}'. Run /skills to browse the loaded list."
            ))),
        }
    }
}

/// /personality command — show or switch the agent personality
pub struct PersonalityCommand;

impl CommandHandler for PersonalityCommand {
    fn name(&self) -> &str {
        "personality"
    }

    fn description(&self) -> &str {
        "Show or switch the agent personality"
    }

    fn usage(&self) -> &str {
        "/personality [name]"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let name = args.trim();
        if !name.is_empty() {
            return Ok(CommandResult::Message(format!(
                "Personality set to: {}\n(Full integration with system prompt pending)",
                name
            )));
        }

        // v0.6.8: read the active persona from `<profile>/persona/persona.toml`
        // so the picker (a) opens with the cursor on the current preset and
        // (b) annotates that row with `· current` so the user can tell at a
        // glance what's loaded — pre-v0.6.8 the picker hardcoded `Some("default")`
        // as preselect even when the actual persona was something else.
        //
        // Note: `PresetId::slug()` uses snake_case (`concise_pro`,
        // `friendly_companion`) while the picker keys here use kebab-case
        // (`friendly-companion`). Match by lowercasing + normalizing `_`
        // to `-`. The picker also has `concise` and `verbose` keys with
        // no exact PresetId mapping; those rows just won't get the
        // `· current` marker, which is acceptable.
        let active_preset_slug = {
            let profile = crate::profile::ProfileManager::active().ok();
            profile.and_then(|p| {
                crate::persona::read_persona_toml(&p)
                    .ok()
                    .flatten()
                    .map(|t| t.preset.slug().replace('_', "-").to_string())
            })
        };

        let items: Vec<ListPickerItem> = PERSONALITY_PRESETS
            .iter()
            .map(|(key, summary)| {
                let is_current = active_preset_slug
                    .as_deref()
                    .map(|p| p == *key)
                    .unwrap_or(false);
                let secondary = if is_current {
                    format!("{summary}  · current")
                } else {
                    (*summary).to_string()
                };
                ListPickerItem {
                    key: (*key).to_string(),
                    primary: (*key).to_string(),
                    secondary,
                }
            })
            .collect();
        let preselect = active_preset_slug.as_deref().or(Some("default"));
        let picker = ListPicker::new(
            ListPickerKind::Personality,
            "Personality",
            items,
            preselect,
            "No personality presets registered.",
        );
        Ok(CommandResult::OpenListPicker(picker))
    }
}

/// /insights command — show session and message statistics
pub struct InsightsCommand;

impl CommandHandler for InsightsCommand {
    fn name(&self) -> &str {
        "insights"
    }

    fn description(&self) -> &str {
        "Show session and message statistics"
    }

    fn usage(&self) -> &str {
        "/insights [--days N]"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        use crate::tui::widgets::{InfoPanel, InfoSection};

        let sessions = ctx.session_store.list_sessions(100)?;
        let total_sessions = sessions.len();
        let total_messages: i64 = sessions.iter().map(|s| s.message_count).sum();
        let current_messages = ctx.messages.len();
        let avg_per_session = if total_sessions > 0 {
            (total_messages as f64) / (total_sessions as f64)
        } else {
            0.0
        };
        let session_age = ctx.started_at.elapsed();
        let age_label = format_duration(session_age);

        let panel = InfoPanel::new("Insights")
            .with_subtitle("session + message stats")
            .with_footer("Esc close · `/usage` for token-level breakdown")
            .section(
                InfoSection::new("Sessions")
                    .key_value("Total", total_sessions.to_string())
                    .key_value("Current age", age_label),
            )
            .section(
                InfoSection::new("Messages")
                    .key_value("Total", total_messages.to_string())
                    .key_value("This session", current_messages.to_string())
                    .key_value("Per session avg", format!("{:.1}", avg_per_session)),
            )
            .section(
                InfoSection::new("Tokens (this session)")
                    .key_value("Prompt", ctx.token_usage.prompt_tokens.to_string())
                    .key_value("Completion", ctx.token_usage.completion_tokens.to_string())
                    .key_value("Total", ctx.token_usage.total_tokens.to_string()),
            );
        Ok(CommandResult::OpenInfoPanel(panel))
    }
}

fn format_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> TuiContext {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx
    }

    #[test]
    fn skills_command_opens_picker() {
        let cmd = SkillsCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();
        match result {
            CommandResult::OpenListPicker(picker) => {
                assert_eq!(picker.kind, crate::tui::widgets::ListPickerKind::Skill);
            }
            other => panic!("Expected OpenListPicker, got {other:?}"),
        }
    }

    #[test]
    fn skills_plural_accepts_the_same_subcommands_as_the_singular() {
        // The two are one letter apart. `/skills new "Kopi Pagi"` used to fall
        // through to the picker, which preselected nothing and said nothing —
        // the request vanished without a word.
        let cmd = SkillsCommand;
        let mut ctx = test_context();

        match cmd.execute("new \"Kopi Pagi\"", &mut ctx).unwrap() {
            CommandResult::OpenSkillInEditor { slug, is_new, .. } => {
                assert_eq!(slug, "kopi-pagi");
                assert!(is_new);
            }
            other => panic!("expected the editor to open, got {other:?}"),
        }

        // `edit` reaches the same lookup, and reports the same miss.
        match cmd.execute("edit no-such-skill", &mut ctx).unwrap() {
            CommandResult::Message(m) => assert!(m.contains("no-such-skill"), "got {m:?}"),
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[test]
    fn skills_command_install_subcommand_routes_to_clawhub_picker() {
        let cmd = SkillsCommand;
        let mut ctx = test_context();
        let result = cmd.execute("install", &mut ctx).unwrap();
        match result {
            CommandResult::OpenClawhubInstallPicker { initial_query } => {
                assert!(initial_query.is_none());
            }
            other => panic!("Expected OpenClawhubInstallPicker, got {other:?}"),
        }
    }

    #[test]
    fn skills_command_install_with_query_passes_through() {
        let cmd = SkillsCommand;
        let mut ctx = test_context();
        let result = cmd.execute("install github", &mut ctx).unwrap();
        match result {
            CommandResult::OpenClawhubInstallPicker { initial_query } => {
                assert_eq!(initial_query.as_deref(), Some("github"));
            }
            other => panic!("Expected OpenClawhubInstallPicker, got {other:?}"),
        }
    }

    #[test]
    fn skill_command_with_no_args_opens_picker() {
        let cmd = SkillCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();
        match result {
            CommandResult::OpenListPicker(picker) => {
                assert_eq!(picker.kind, crate::tui::widgets::ListPickerKind::Skill);
            }
            other => panic!("Expected OpenListPicker, got {other:?}"),
        }
    }

    #[test]
    fn skill_command_with_unknown_name_returns_friendly_message() {
        let cmd = SkillCommand;
        let mut ctx = test_context();

        let result = cmd.execute("nonexistent-skill", &mut ctx).unwrap();
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("nonexistent-skill"));
                assert!(msg.to_lowercase().contains("no skill"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn skill_command_with_known_name_shows_details() {
        let cmd = SkillCommand;
        let mut ctx = test_context();
        ctx.available_skills.push(crate::skills::Skill {
            name: "summarizer".to_string(),
            description: "Distills long text into bullets.".to_string(),
            version: "0.2.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![],
            prompts: vec![],
            location: None,
            requires: Default::default(),
            install_recipes: Vec::new(),
            remote: false,
            origin: None,
        });
        let result = cmd.execute("summarizer", &mut ctx).unwrap();
        // A known skill name renders its detail in an InfoPanel (since v0.6.23),
        // not a plain Message.
        match result {
            CommandResult::OpenInfoPanel(panel) => {
                let blob = format!("{panel:?}");
                assert!(blob.contains("summarizer"), "{blob}");
                assert!(blob.contains("0.2.0"), "{blob}");
                assert!(blob.contains("bullets"), "{blob}");
            }
            other => panic!("Expected OpenInfoPanel result, got {other:?}"),
        }
    }

    /// A skill in `ctx`, with the origin and on-disk location the edit gate
    /// reads. `dir` is where its `SKILL.md` would live.
    fn push_skill(
        ctx: &mut TuiContext,
        name: &str,
        dir: &std::path::Path,
        origin: Option<crate::skills::origin::SkillOriginKind>,
    ) {
        ctx.available_skills.push(crate::skills::Skill {
            name: name.to_string(),
            description: "A test skill.".to_string(),
            version: "0.1.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![],
            prompts: vec![],
            location: Some(dir.join("SKILL.md")),
            requires: crate::skills::SkillRequires::default(),
            install_recipes: Vec::new(),
            remote: false,
            origin: origin.map(|kind| crate::skills::origin::SkillOrigin::new(kind, None)),
        });
    }

    #[test]
    fn skill_new_refuses_before_opening_an_editor() {
        let cmd = SkillCommand;
        let mut ctx = test_context();
        let tmp = tempfile::tempdir().unwrap();
        push_skill(
            &mut ctx,
            "Kopi Pagi",
            &tmp.path().join("kopi-pagi"),
            Some(crate::skills::origin::SkillOriginKind::Authored),
        );

        // Everything refusable is refused at dispatch, so a rejection costs the
        // user nothing — no editor opened, no file staged.
        for (args, expect) in [
            ("new", "Usage"),
            ("new \"!!!\"", "folder name"),
            ("new \"Kopi Pagi\"", "already exists"),
            // A different display name that slugifies onto the same folder —
            // the collision a name-only check would miss.
            ("new \"kopi  pagi\"", "already occupies"),
        ] {
            match cmd.execute(args, &mut ctx).unwrap() {
                CommandResult::Message(msg) => {
                    assert!(msg.contains(expect), "{args}: got {msg:?}");
                }
                other => panic!("{args}: expected Message, got {other:?}"),
            }
        }
    }

    #[test]
    fn skill_edit_accepts_display_name_or_folder_name() {
        let cmd = SkillCommand;
        let mut ctx = test_context();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("kopi-pagi");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: Kopi Pagi\n---\n# x\n").unwrap();
        push_skill(
            &mut ctx,
            "Kopi Pagi",
            &dir,
            Some(crate::skills::origin::SkillOriginKind::Authored),
        );

        // `normalise_skill_name` only lowercases and maps `-` to `_`, so the
        // display name and its own folder name never match each other. Both
        // spellings have to work: the folder is what the user sees on disk.
        for args in ["edit Kopi Pagi", "edit kopi-pagi"] {
            match cmd.execute(args, &mut ctx).unwrap() {
                CommandResult::OpenSkillInEditor { slug, is_new, .. } => {
                    assert_eq!(slug, "kopi-pagi", "{args}");
                    assert!(!is_new, "{args}");
                }
                other => panic!("{args}: expected OpenSkillInEditor, got {other:?}"),
            }
        }
    }

    #[test]
    fn skill_edit_refuses_skills_managed_by_someone_else() {
        let cmd = SkillCommand;
        let mut ctx = test_context();
        let tmp = tempfile::tempdir().unwrap();

        for (name, kind, expect) in [
            (
                "weather",
                Some(crate::skills::origin::SkillOriginKind::Clawhub),
                "ClawHub",
            ),
            (
                "summarizer",
                Some(crate::skills::origin::SkillOriginKind::Bundled),
                "bundled",
            ),
            ("mystery", None, "unrecorded"),
        ] {
            let dir = tmp.path().join(name);
            push_skill(&mut ctx, name, &dir, kind);
            match cmd.execute(&format!("edit {name}"), &mut ctx).unwrap() {
                CommandResult::Message(msg) => {
                    assert!(msg.contains(expect), "{name}: got {msg:?}");
                }
                other => panic!("{name}: expected Message, got {other:?}"),
            }
        }
    }

    #[test]
    fn personality_command_opens_picker_on_empty_args() {
        let cmd = PersonalityCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::OpenListPicker(picker) => {
                assert_eq!(
                    picker.kind,
                    crate::tui::widgets::ListPickerKind::Personality
                );
                assert!(!picker.entries().is_empty());
                assert!(picker
                    .entries()
                    .iter()
                    .any(|e| matches!(e, crate::tui::widgets::ListPickerEntry::Item(i) if i.key == "default")));
            }
            other => panic!("Expected OpenListPicker, got {other:?}"),
        }
    }

    #[test]
    fn personality_command_sets_personality() {
        let cmd = PersonalityCommand;
        let mut ctx = test_context();

        let result = cmd.execute("concise", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("concise"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn insights_command_shows_stats() {
        let cmd = InsightsCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();

        // Insights renders an InfoPanel (since the stats refactor), not a Message.
        match result {
            CommandResult::OpenInfoPanel(panel) => {
                assert_eq!(panel.title, "Insights");
                let blob = format!("{panel:?}");
                assert!(blob.contains("Sessions"), "{blob}");
                assert!(blob.contains("Total"), "{blob}");
            }
            other => panic!("Expected OpenInfoPanel result, got {other:?}"),
        }
    }
}
