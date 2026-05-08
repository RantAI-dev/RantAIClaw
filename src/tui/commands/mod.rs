mod agent;
mod config;
mod core;
mod cron;
mod memory;
mod model;
mod session;
pub mod setup;
mod skills;

use anyhow::Result;
use std::collections::HashMap;

use super::context::TuiContext;

/// Result of executing a command
#[derive(Debug)]
pub enum CommandResult {
    Continue,
    Message(String),
    /// Open a modal overlay (Claude-Code-style). The renderer pins it
    /// above the chat until the user presses `Esc`. Use for content too
    /// big or too structured for an inline `Message` — `/help`, full
    /// `/skills` listings, `/sessions` directory, etc.
    Overlay(OverlayContent),
    Quit,
    ClearError,
    /// Dispatch a new agent turn with the given user message.
    /// The message is not appended to history again — the caller
    /// expects it to already live there (e.g. `/retry`).
    Resubmit(String),
    /// Open an interactive list picker (Up/Down/Enter/Esc). The
    /// `ListPicker.kind` tag tells the app's key handler which side
    /// effect to run when the user presses Enter — switch model,
    /// resume session, set personality, etc.
    OpenListPicker(crate::tui::widgets::ListPicker),
    /// Open a read-only info panel (Up/Down scroll, Esc close). Used by
    /// /channels, /config, /doctor, /insights, /status, /usage, /skill.
    /// Replaces the v0.6.7-and-earlier pattern of dumping data as a
    /// `System:` chat blob — visually inconsistent with the rest of the
    /// TUI, per v0.6.7 tester feedback.
    OpenInfoPanel(crate::tui::widgets::InfoPanel),
    /// Open the setup overlay. `provisioner` is `None` to show the picker
    /// of all available provisioners, or a specific name to jump straight in.
    OpenSetupOverlay {
        provisioner: Option<String>,
    },
    /// Launch the first-run wizard — sequential setup covering all topics.
    OpenFirstRunWizard,
    /// Fetch the ClawHub catalogue and open an interactive install picker.
    /// Mirrors the `/sessions` pattern (search + paginate via ListPicker)
    /// but fetches asynchronously since the catalogue lives on the network.
    /// `initial_query` pre-fills the search bar — empty for `/install`,
    /// populated for `/install <query>` invocations.
    OpenClawhubInstallPicker {
        initial_query: Option<String>,
    },
    /// Wipe the terminal's visible screen + scrollback in addition to
    /// any side effect the command already performed (e.g. starting a
    /// new session). The string is committed to scrollback after the
    /// clear so the user sees a confirmation line on the fresh screen.
    ClearTerminal(String),
    /// Replace the input buffer with the given text, leaving the cursor
    /// at the end so the user can finish typing and submit. Used by the
    /// `/<skill-name>` direct-invoke fallback to pre-fill a "Use the
    /// <skill> skill: " prompt.
    SetInput(String),
}

/// Pre-rendered content for the modal help overlay. Multiple "tabs" can
/// share one overlay (a la Claude Code's `general / commands /
/// custom-commands` strip); only the active tab is visible at a time.
#[derive(Debug, Clone)]
pub struct OverlayContent {
    pub title: String,
    pub tabs: Vec<OverlayTab>,
    pub active_tab: usize,
}

#[derive(Debug, Clone)]
pub struct OverlayTab {
    pub label: String,
    /// Body lines, plain text. The renderer applies brand styling at
    /// draw time (sky for keywords, muted for descriptions, etc.).
    pub body: Vec<String>,
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
        self.register(Box::new(config::ChannelsCommand));
        // /platforms alias dropped in v0.6.8 — was redundant with /channels
        // (literally the same output) per tester feedback.
        self.register(Box::new(memory::MemoryCommand));
        self.register(Box::new(memory::ForgetCommand));
        self.register(Box::new(memory::CompressCommand));
        self.register(Box::new(cron::CronCommand));
        self.register(Box::new(skills::SkillsCommand));
        self.register(Box::new(skills::SkillCommand));
        self.register(Box::new(skills::PersonalityCommand));
        self.register(Box::new(skills::InsightsCommand));
        self.register(Box::new(setup::SetupCommand));
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
            return handler.execute(args, ctx);
        }

        // Fallback: `/<skill-name> [args]` — direct skill invoke.
        // Looks up an installed skill whose canonical name (or its
        // dash-or-underscore-normalised form) matches `cmd_name`. If found,
        // pre-fill the input buffer with a "Use the <name> skill: <args>"
        // prompt the user can tweak before submitting. Mirrors Hermes'
        // `/<skill>` shortcut without committing to broader Hermes
        // alignment — purely a UX win on top of the existing /skills flow.
        let normalised = |s: &str| s.to_lowercase().replace('-', "_");
        if let Some(skill) = ctx
            .available_skills
            .iter()
            .find(|s| normalised(&s.name) == normalised(&cmd_name))
        {
            let prefill = if args.is_empty() {
                format!("Use the {} skill: ", skill.name)
            } else {
                format!("Use the {} skill: {}", skill.name, args)
            };
            return Ok(CommandResult::SetInput(prefill));
        }

        Ok(CommandResult::Message(format!(
            "Unknown command: /{}",
            cmd_name
        )))
    }

    pub fn autocomplete(&self, partial: &str) -> Vec<String> {
        self.autocomplete_with_skills(partial, &[])
    }

    /// Autocomplete that also surfaces installed skill names as
    /// `/<skill-name>` (direct-invoke shortcut). Skills appear in
    /// addition to built-in commands; both share alphabetical order.
    pub fn autocomplete_with_skills(
        &self,
        partial: &str,
        skills: &[crate::skills::Skill],
    ) -> Vec<String> {
        let partial = partial.trim_start_matches('/').to_lowercase();
        let mut matches: Vec<String> = self
            .commands
            .keys()
            .filter(|name| name.starts_with(&partial))
            .map(|name| format!("/{}", name))
            .collect();

        for alias in self.aliases.keys() {
            if alias.starts_with(&partial) {
                matches.push(format!("/{}", alias));
            }
        }

        for skill in skills {
            let key = skill.name.to_lowercase().replace('-', "_");
            // Don't duplicate if a built-in command already covers it.
            if !self.commands.contains_key(&key) && key.starts_with(&partial) {
                matches.push(format!("/{}", skill.name));
            }
        }

        matches.sort();
        matches.dedup();
        matches
    }

    /// Same prefix-match as `autocomplete` but returns `(name, description)`
    /// tuples sorted by command name. Aliases are shown only when they
    /// match without their canonical name also matching, and inherit the
    /// canonical command's description.
    pub fn autocomplete_with_descriptions(&self, partial: &str) -> Vec<(String, String)> {
        self.autocomplete_with_descriptions_and_skills(partial, &[])
    }

    /// Description-flavoured autocomplete that also surfaces installed
    /// skills as `/<skill-name>` rows.  The skill's frontmatter
    /// description is used as the row's secondary text so the
    /// dropdown reads the same as a built-in command.
    pub fn autocomplete_with_descriptions_and_skills(
        &self,
        partial: &str,
        skills: &[crate::skills::Skill],
    ) -> Vec<(String, String)> {
        let partial = partial.trim_start_matches('/').to_lowercase();
        let mut out: Vec<(String, String)> = Vec::new();

        for (name, handler) in &self.commands {
            if name.starts_with(&partial) {
                out.push((format!("/{name}"), handler.description().to_string()));
            }
        }
        for (alias, canonical) in &self.aliases {
            if alias.starts_with(&partial) && !canonical.starts_with(&partial) {
                if let Some(handler) = self.commands.get(canonical) {
                    out.push((format!("/{alias}"), handler.description().to_string()));
                }
            }
        }

        for skill in skills {
            let key = skill.name.to_lowercase().replace('-', "_");
            if self.commands.contains_key(&key) || self.aliases.contains_key(&key) {
                continue;
            }
            if !key.starts_with(&partial) {
                continue;
            }
            let desc = if skill.description.is_empty() {
                format!("Invoke the `{}` skill", skill.name)
            } else {
                let chars: String = skill.description.chars().take(80).collect();
                format!("[skill] {chars}")
            };
            out.push((format!("/{}", skill.name), desc));
        }

        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by(|a, b| a.0 == b.0);
        out
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

    fn test_context() -> TuiContext {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx
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
