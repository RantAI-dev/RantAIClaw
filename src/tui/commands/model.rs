use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{
    ListPicker, ListPickerEntry, ListPickerItem, ListPickerKind, ModelEntry,
};

/// /model command — display, change, or interactively pick the active model.
pub struct ModelCommand;

impl CommandHandler for ModelCommand {
    fn name(&self) -> &str {
        "model"
    }

    fn description(&self) -> &str {
        "Pick or change the active model"
    }

    fn usage(&self) -> &str {
        "/model [provider:model]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let model = args.trim();
        if !model.is_empty() {
            ctx.model = model.to_string();
            return Ok(CommandResult::Message(format!("Model set to: {model}")));
        }

        // No args → open the interactive picker. Build entries from the
        // curated per-provider lists, restricted to providers detected as
        // enabled at TUI startup.
        let mut entries: Vec<ModelEntry> = Vec::new();
        let providers = if ctx.available_providers.is_empty() {
            ctx.model
                .split(':')
                .next()
                .map(|s| vec![s.to_string()])
                .unwrap_or_default()
        } else {
            ctx.available_providers.clone()
        };

        for provider in &providers {
            for (id, desc) in crate::onboard::wizard::curated_models_for_provider(provider) {
                entries.push(ModelEntry {
                    provider: provider.clone(),
                    model_id: id,
                    description: desc,
                });
            }
        }

        let items: Vec<ListPickerItem> = entries
            .iter()
            .map(|e| ListPickerItem {
                key: e.target(),
                primary: e.target(),
                secondary: e.description.clone(),
            })
            .collect();

        let picker = ListPicker::new(
            ListPickerKind::Model,
            "Select Model",
            items,
            Some(&ctx.model),
            "No providers with credentials detected. Run `rantaiclaw setup provider`.",
        );
        Ok(CommandResult::OpenListPicker(picker))
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

    #[test]
    fn model_command_with_empty_args_opens_picker() {
        let cmd = ModelCommand;
        let mut ctx = test_context();
        ctx.available_providers = vec!["openai".to_string()];

        let result = cmd.execute("", &mut ctx).unwrap();
        match result {
            CommandResult::OpenListPicker(picker) => {
                assert_eq!(picker.kind, ListPickerKind::Model);
                assert!(!picker.entries().is_empty());
                assert!(picker.entries().iter().all(
                    |e| matches!(e, ListPickerEntry::Item(i) if i.key.starts_with("openai:"))
                ));
            }
            other => panic!("expected OpenListPicker, got {other:?}"),
        }
    }
}
