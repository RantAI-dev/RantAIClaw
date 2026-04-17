mod agent;
mod config;
mod core;
mod cron;
mod memory;
mod model;
mod session;
mod skills;

use anyhow::Result;
use std::collections::HashMap;

use super::context::TuiContext;

/// Result of executing a command
#[derive(Debug)]
pub enum CommandResult {
    Continue,
    Message(String),
    Quit,
    ClearError,
}

/// Trait for command handlers
pub trait CommandHandler: Send + Sync {
    fn name(&self) -> &str;
    fn aliases(&self) -> Vec<&str> {
        vec![]
    }
    fn description(&self) -> &str;
    fn usage(&self) -> &str {
        self.name()
    }
    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult>;
}

/// Registry of all available commands
pub struct CommandRegistry {
    commands: HashMap<String, Box<dyn CommandHandler>>,
    aliases: HashMap<String, String>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            commands: HashMap::new(),
            aliases: HashMap::new(),
        };
        registry.register_defaults();
        registry
    }

    fn register_defaults(&mut self) {
        self.register(Box::new(core::HelpCommand));
        self.register(Box::new(core::QuitCommand));
        self.register(Box::new(core::NewCommand));
        self.register(Box::new(session::SessionsCommand));
        self.register(Box::new(session::ResumeCommand));
        self.register(Box::new(session::SearchCommand));
        self.register(Box::new(session::TitleCommand));
        self.register(Box::new(model::ModelCommand));
        self.register(Box::new(model::UsageCommand));
        self.register(Box::new(agent::RetryCommand));
        self.register(Box::new(agent::UndoCommand));
        self.register(Box::new(agent::StopCommand));
        self.register(Box::new(config::StatusCommand));
        self.register(Box::new(config::DebugCommand));
        self.register(Box::new(config::ConfigCommand));
        self.register(Box::new(config::DoctorCommand));
        self.register(Box::new(config::PlatformsCommand));
        self.register(Box::new(memory::MemoryCommand));
        self.register(Box::new(memory::ForgetCommand));
        self.register(Box::new(memory::CompressCommand));
        self.register(Box::new(cron::CronCommand));
        self.register(Box::new(skills::SkillsCommand));
        self.register(Box::new(skills::SkillCommand));
        self.register(Box::new(skills::PersonalityCommand));
        self.register(Box::new(skills::InsightsCommand));
    }

    pub fn register(&mut self, handler: Box<dyn CommandHandler>) {
        let name = handler.name().to_string();
        for alias in handler.aliases() {
            self.aliases.insert(alias.to_string(), name.clone());
        }
        self.commands.insert(name, handler);
    }

    pub fn dispatch(&self, input: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let input = input.trim_start_matches('/');
        let parts: Vec<&str> = input.splitn(2, ' ').collect();
        let cmd_name = parts[0].to_lowercase();
        let args = parts.get(1).copied().unwrap_or("");

        let canonical_name = self
            .aliases
            .get(&cmd_name)
            .cloned()
            .unwrap_or(cmd_name.clone());

        if let Some(handler) = self.commands.get(&canonical_name) {
            handler.execute(args, ctx)
        } else {
            Ok(CommandResult::Message(format!(
                "Unknown command: /{}",
                cmd_name
            )))
        }
    }

    pub fn autocomplete(&self, partial: &str) -> Vec<String> {
        let partial = partial.trim_start_matches('/').to_lowercase();
        let mut matches: Vec<String> = self
            .commands
            .keys()
            .filter(|name| name.starts_with(&partial))
            .map(|name| format!("/{}", name))
            .collect();

        for (alias, _) in &self.aliases {
            if alias.starts_with(&partial) {
                matches.push(format!("/{}", alias));
            }
        }

        matches.sort();
        matches.dedup();
        matches
    }

    pub fn get_help(&self) -> Vec<(&str, &str)> {
        let mut help: Vec<_> = self
            .commands
            .values()
            .map(|h| (h.name(), h.description()))
            .collect();
        help.sort_by(|a, b| a.0.cmp(b.0));
        help
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionStore;

    fn test_context() -> TuiContext {
        let store = SessionStore::in_memory().expect("in-memory store");
        TuiContext::new(store, "test-model", None).expect("context creation")
    }

    #[test]
    fn registry_dispatches_known_command() {
        let registry = CommandRegistry::new();
        let mut ctx = test_context();

        let result = registry.dispatch("/quit", &mut ctx).unwrap();
        assert!(matches!(result, CommandResult::Quit));
    }

    #[test]
    fn registry_handles_unknown_command() {
        let registry = CommandRegistry::new();
        let mut ctx = test_context();

        let result = registry.dispatch("/nonexistent", &mut ctx).unwrap();
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[test]
    fn registry_resolves_aliases() {
        let registry = CommandRegistry::new();
        let mut ctx = test_context();

        let result = registry.dispatch("/exit", &mut ctx).unwrap();
        assert!(matches!(result, CommandResult::Quit));
    }

    #[test]
    fn autocomplete_finds_matching_commands() {
        let registry = CommandRegistry::new();
        let matches = registry.autocomplete("/he");
        assert!(matches.contains(&"/help".to_string()));
    }
}
