use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{ListPicker, ListPickerEntry, ListPickerItem, ListPickerKind};

/// Built-in personality presets surfaced in the `/personality` picker.
/// Each tuple is `(key, summary shown as the muted secondary line)`.
const PERSONALITY_PRESETS: &[(&str, &str)] = &[
    ("default", "Balanced general-purpose assistant"),
    ("concise", "Terse responses, minimal preamble"),
    ("verbose", "Detailed explanations and rationale"),
    ("executive-assistant", "Calendar, email, scheduling focus"),
    ("friendly-companion", "Warm, conversational tone"),
];

/// Build picker rows from the loaded skills list. Primary text is the
/// skill name + version; secondary is the description (truncated by
/// the renderer if too long).
fn build_skill_items(skills: &[crate::skills::Skill]) -> Vec<ListPickerItem> {
    skills
        .iter()
        .map(|s| {
            let primary = if s.version.is_empty() {
                s.name.clone()
            } else {
                format!("{} · v{}", s.name, s.version)
            };
            let mut secondary = s.description.clone();
            if !s.tags.is_empty() {
                secondary = format!("{secondary}  ({})", s.tags.join(", "));
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

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        // `/skills install [query]` is an alias for `/install [query]` so
        // both discoverability paths reach the ClawHub browser. Anything
        // else (no args, or args that aren't `install*`) opens the local
        // skills picker as before.
        let trimmed = args.trim();
        if let Some(rest) = trimmed.strip_prefix("install") {
            let query = rest.trim();
            let initial_query = if query.is_empty() {
                None
            } else {
                Some(query.to_string())
            };
            return Ok(CommandResult::OpenClawhubInstallPicker { initial_query });
        }

        let items = build_skill_items(&ctx.available_skills);
        let picker = ListPicker::new(
            ListPickerKind::Skill,
            "Skills",
            items,
            None,
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
        "Invoke or inspect a skill"
    }

    fn usage(&self) -> &str {
        "/skill [name]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        use crate::tui::widgets::{InfoPanel, InfoSection};

        let name = args.trim();
        if name.is_empty() {
            // /skill (no args) opens an InfoPanel with usage + loaded
            // skill list. /skills opens the interactive picker. v0.6.8
            // moves both surfaces off the chat-blob layout in favor of
            // proper TUI panels.
            let mut panel = InfoPanel::new("Skills")
                .with_subtitle(format!("{} loaded", ctx.available_skills.len()))
                .with_footer("Esc close · `/skills` opens the interactive picker");
            if ctx.available_skills.is_empty() {
                panel = panel.section(
                    InfoSection::new("Loaded")
                        .plain("No skills are loaded yet.")
                        .plain("Run `/setup skills` to install the starter pack."),
                );
            } else {
                let mut sec = InfoSection::new("Loaded");
                for s in &ctx.available_skills {
                    let title = if s.version.is_empty() {
                        s.name.clone()
                    } else {
                        format!("{} · v{}", s.name, s.version)
                    };
                    if s.description.is_empty() {
                        sec = sec.bullet(title);
                    } else {
                        sec = sec.bullet_with(title, s.description.clone());
                    }
                }
                panel = panel.section(sec);
            }
            panel = panel.section(
                InfoSection::new("Usage")
                    .key_value("/skill <name>", "show metadata for a skill")
                    .key_value("/skills", "interactive picker (search + select)"),
            );
            return Ok(CommandResult::OpenInfoPanel(panel));
        }

        // With a name arg, find it and render its detail in an InfoPanel.
        let found = ctx
            .available_skills
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name));
        match found {
            Some(s) => {
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
                if !s.tags.is_empty() {
                    sec = sec.spacer().key_value("Tags", s.tags.join(", "));
                }
                panel = panel.section(sec).section(
                    InfoSection::new("Activate")
                        .plain(
                            "Describe what you want and the agent will use this \
                             skill — e.g. `summarize today's standup notes`.",
                        ),
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
        });
        let result = cmd.execute("summarizer", &mut ctx).unwrap();
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("summarizer"));
                assert!(msg.contains("0.2.0"));
                assert!(msg.contains("bullets"));
            }
            _ => panic!("Expected Message result"),
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
                    .any(|e| matches!(e, ListPickerEntry::Item(i) if i.key == "default")));
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

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Total sessions"));
            }
            _ => panic!("Expected Message result"),
        }
    }
}
