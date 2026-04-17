use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

/// /skills command — list available skills
pub struct SkillsCommand;

impl CommandHandler for SkillsCommand {
    fn name(&self) -> &str {
        "skills"
    }

    fn description(&self) -> &str {
        "List available skills"
    }

    fn execute(&self, _args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        Ok(CommandResult::Message(
            "Available skills:\n  (No skills loaded)\n\nSkills will be loaded from ~/.rantaiclaw/skills/".to_string(),
        ))
    }
}

/// /skill command — invoke or inspect a specific skill by name
pub struct SkillCommand;

impl CommandHandler for SkillCommand {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Invoke or inspect a skill by name"
    }

    fn usage(&self) -> &str {
        "/skill <name>"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let name = args.trim();
        if name.is_empty() {
            return Ok(CommandResult::Message("Usage: /skill <name>".to_string()));
        }

        Ok(CommandResult::Message(format!(
            "Skill '{}': Integration with skills system pending.",
            name
        )))
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
        if name.is_empty() {
            return Ok(CommandResult::Message(
                "Current personality: default\n\nAvailable personalities:\n  default\n  concise\n  verbose\n\nUsage: /personality <name>".to_string(),
            ));
        }

        Ok(CommandResult::Message(format!(
            "Personality set to: {}\n(Full integration with system prompt pending)",
            name
        )))
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
    use crate::sessions::SessionStore;

    fn test_context() -> TuiContext {
        let store = SessionStore::in_memory().unwrap();
        TuiContext::new(store, "test", None).unwrap()
    }

    #[test]
    fn skills_command_lists_skills() {
        let cmd = SkillsCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.to_lowercase().contains("skills"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn skill_command_shows_usage_on_empty_args() {
        let cmd = SkillCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("Usage"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn skill_command_returns_name_in_message() {
        let cmd = SkillCommand;
        let mut ctx = test_context();

        let result = cmd.execute("my-skill", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("my-skill"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn personality_command_shows_current_on_empty_args() {
        let cmd = PersonalityCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("default"));
            }
            _ => panic!("Expected Message result"),
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
