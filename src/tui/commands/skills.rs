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

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let items = build_skill_items(&ctx.available_skills);
        let picker = ListPicker::new(
            ListPickerKind::Skill,
            "Skills",
            items,
            None,
            "No skills loaded. Drop a SKILL.toml in ~/.rantaiclaw/workspace/skills/<name>/.",
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
        let name = args.trim();
        if name.is_empty() {
            // Same as /skills — open the picker.
            let items = build_skill_items(&ctx.available_skills);
            let picker = ListPicker::new(
                ListPickerKind::Skill,
                "Skills",
                items,
                None,
                "No skills loaded. Drop a SKILL.toml in ~/.rantaiclaw/workspace/skills/<name>/.",
            );
            return Ok(CommandResult::OpenListPicker(picker));
        }

        // With a name arg, find it in the loaded list and surface a
        // helpful message. The actual "invoke" lives in the picker
        // selection handler so both /skill <name> and the picker share
        // the same activation path.
        let found = ctx
            .available_skills
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name));
        match found {
            Some(s) => Ok(CommandResult::Message(format!(
                "Skill '{}' (v{})\n  {}\nType /skill to open the picker, or just describe what you want and the agent will use this skill.",
                s.name, s.version, s.description
            ))),
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

        let items: Vec<ListPickerItem> = PERSONALITY_PRESETS
            .iter()
            .map(|(key, summary)| ListPickerItem {
                key: (*key).to_string(),
                primary: (*key).to_string(),
                secondary: (*summary).to_string(),
            })
            .collect();
        let picker = ListPicker::new(
            ListPickerKind::Personality,
            "Personality",
            items,
            Some("default"),
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
        let sessions = ctx.session_store.list_sessions(100)?;
        let total_sessions = sessions.len();
        let total_messages: i64 = sessions.iter().map(|s| s.message_count).sum();
        let current_messages = ctx.messages.len();

        Ok(CommandResult::Message(format!(
            "Session insights:\n  Total sessions: {}\n  Total messages: {}\n  Current session messages: {}",
            total_sessions, total_messages, current_messages
        )))
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
