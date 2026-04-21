use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

/// /model command — display or change the current model
pub struct ModelCommand;

impl CommandHandler for ModelCommand {
    fn name(&self) -> &str {
        "model"
    }

    fn description(&self) -> &str {
        "Change or display current model"
    }

    fn usage(&self) -> &str {
        "/model [provider:model]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let model = args.trim();
        if model.is_empty() {
            return Ok(CommandResult::Message(format!(
                "Current model: {}\n\nUsage: /model <provider:model>\nExamples:\n  /model anthropic:claude-sonnet-4-20250514\n  /model openai:gpt-4o\n  /model ollama:llama3",
                ctx.model
            )));
        }
        ctx.model = model.to_string();
        Ok(CommandResult::Message(format!("Model set to: {}", model)))
    }
}

/// /usage command — show accumulated token usage for the session
pub struct UsageCommand;

impl CommandHandler for UsageCommand {
    fn name(&self) -> &str {
        "usage"
    }

    fn description(&self) -> &str {
        "Show token usage statistics"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let usage = &ctx.token_usage;
        Ok(CommandResult::Message(format!(
            "Token usage this session:\n  Prompt tokens: {}\n  Completion tokens: {}\n  Total tokens: {}",
            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
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
    fn model_command_shows_current_model() {
        let cmd = ModelCommand;
        let mut ctx = test_context();

        let result = cmd.execute("", &mut ctx).unwrap();

        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("mock-model"));
            }
            _ => panic!("Expected Message result"),
        }
    }

    #[test]
    fn model_command_changes_model() {
        let cmd = ModelCommand;
        let mut ctx = test_context();

        let result = cmd.execute("openai:gpt-4o", &mut ctx).unwrap();

        assert_eq!(ctx.model, "openai:gpt-4o");
        match result {
            CommandResult::Message(msg) => {
                assert!(msg.contains("openai:gpt-4o"));
            }
            _ => panic!("Expected Message result"),
        }
    }
}
