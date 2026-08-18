use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind, ModelEntry};

/// Rows for one provider's model picker.
///
/// Resolves the same catalog the CLI (`models list`) and the web console
/// (`GET /providers/{id}/models`) resolve: the on-disk model cache written by
/// `models refresh` / `doctor models`, falling back to the curated list when
/// no cache exists. Before this, `/model` read the curated list *only*, so a
/// refresh had no effect on the TUI at all — on a profile whose cache held 400
/// live OpenRouter models the picker still offered 12, five of which OpenRouter
/// does not serve.
///
/// The catalog returns bare IDs, so the curated `(id, label)` pairs are kept as
/// a description lookup: catalog order wins, curated supplies a description
/// where it has one, and anything else shows the ID alone.
fn model_entries_for_provider(
    provider: &str,
    workspace_dir: Option<&std::path::Path>,
) -> Vec<ModelEntry> {
    let curated = crate::onboard::wizard::curated_models_for_provider(provider);

    let Some(workspace_dir) = workspace_dir else {
        // No config was available (unit tests) — curated is all there is.
        return curated
            .into_iter()
            .map(|(model_id, description)| ModelEntry {
                provider: provider.to_string(),
                model_id,
                description,
            })
            .collect();
    };

    let describe = |id: &str| {
        curated
            .iter()
            .find(|(curated_id, _)| curated_id == id)
            .map(|(_, description)| description.clone())
            .unwrap_or_default()
    };

    crate::onboard::wizard::provider_model_catalog(workspace_dir, provider)
        .models
        .into_iter()
        .map(|model_id| ModelEntry {
            description: describe(&model_id),
            provider: provider.to_string(),
            model_id,
        })
        .collect()
}

/// Split a `/model` target into `(provider, model)`.
///
/// Picker keys are always `provider:model` (`ModelEntry::target()`), but
/// model ids may themselves contain `:` (ollama `llama3:8b`), so only the
/// first segment is the provider. A bare string with no `:` (or an empty
/// first segment) names a model only — the caller keeps the current
/// provider.
pub(crate) fn split_model_target(target: &str) -> (Option<&str>, &str) {
    match target.split_once(':') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (Some(provider), model)
        }
        _ => (None, target),
    }
}

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
            return Ok(CommandResult::SetModel(model.to_string()));
        }

        // No args → open the interactive picker, restricted to providers
        // detected as enabled at TUI startup.
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
            entries.extend(model_entries_for_provider(
                provider,
                ctx.workspace_dir.as_deref(),
            ));
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
        use crate::tui::widgets::{InfoPanel, InfoSection};

        let u = &ctx.token_usage;
        let panel = InfoPanel::new("Token Usage")
            .with_subtitle("this session")
            .with_footer("Esc close · `/insights` for cumulative stats")
            .section(
                InfoSection::new("Tokens")
                    .key_value("Prompt", u.prompt_tokens.to_string())
                    .key_value("Completion", u.completion_tokens.to_string())
                    .key_value("Total", u.total_tokens.to_string()),
            )
            .section(
                InfoSection::new("Model")
                    .key_value("Active", &ctx.model)
                    .key_value(
                        "Context window",
                        ctx.context_window
                            .map(|w| format!("{w} tokens"))
                            .unwrap_or_else(|| "unknown".to_string()),
                    ),
            );
        Ok(CommandResult::OpenInfoPanel(panel))
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

        // The handler no longer mutates the label itself: it hands the app a
        // `SetModel` so the switch also persists the config and reloads the
        // agent (`TuiApp::apply_model_selection`). Before that, `/model` set
        // `ctx.model` and nothing else — the label changed, the agent didn't.
        match result {
            CommandResult::SetModel(target) => {
                assert_eq!(target, "openai:gpt-4o");
            }
            other => panic!("expected SetModel result, got {other:?}"),
        }
    }

    #[test]
    fn split_target_with_provider() {
        assert_eq!(
            split_model_target("openrouter:meta/llama-4-scout"),
            (Some("openrouter"), "meta/llama-4-scout")
        );
    }

    #[test]
    fn split_target_keeps_colons_inside_the_model_id() {
        assert_eq!(
            split_model_target("ollama:llama3:8b"),
            (Some("ollama"), "llama3:8b")
        );
    }

    #[test]
    fn split_target_bare_model_names_no_provider() {
        assert_eq!(split_model_target("gpt-5.3"), (None, "gpt-5.3"));
    }

    #[test]
    fn split_target_degenerate_colon_forms_are_model_only() {
        assert_eq!(split_model_target(":x"), (None, ":x"));
        assert_eq!(split_model_target("x:"), (None, "x:"));
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
                    |e| matches!(e, crate::tui::widgets::ListPickerEntry::Item(i) if i.key.starts_with("openai:"))
                ));
            }
            other => panic!("expected OpenListPicker, got {other:?}"),
        }
    }

    // ── the picker follows the model cache ───────────────────────
    //
    // `/model` used to read the curated list only, so `models refresh` and
    // `doctor models` — both of which write `models_cache.json` — had no effect
    // on the TUI whatsoever.

    /// Seed a workspace whose model cache holds `models` for `provider`.
    fn workspace_with_cached_models(provider: &str, models: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let payload = serde_json::json!({
            "entries": [{
                "provider": provider,
                "fetched_at_unix": 4_102_444_800_u64,
                "models": models,
            }]
        });
        std::fs::write(
            state.join("models_cache.json"),
            serde_json::to_string(&payload).unwrap(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn picker_offers_cached_models_the_curated_list_never_mentions() {
        let cache_only = "openai/some-model-only-the-live-catalog-has";
        assert!(
            !crate::onboard::wizard::curated_models_for_provider("openai")
                .iter()
                .any(|(id, _)| id == cache_only),
            "fixture must not collide with the curated list"
        );

        let workspace = workspace_with_cached_models("openai", &[cache_only]);
        let entries = model_entries_for_provider("openai", Some(workspace.path()));

        assert!(
            entries.iter().any(|e| e.model_id == cache_only),
            "the picker must surface what `models refresh` wrote"
        );
    }

    #[test]
    fn picker_keeps_curated_descriptions_for_models_present_in_both() {
        let (curated_id, curated_desc) =
            crate::onboard::wizard::curated_models_for_provider("openai")
                .into_iter()
                .next()
                .expect("openai has a curated list");

        let workspace = workspace_with_cached_models("openai", &[curated_id.as_str()]);
        let entries = model_entries_for_provider("openai", Some(workspace.path()));

        let entry = entries
            .iter()
            .find(|e| e.model_id == curated_id)
            .expect("cached curated model must be offered");
        assert_eq!(
            entry.description, curated_desc,
            "catalog supplies the ID, curated supplies the description"
        );
    }

    #[test]
    fn picker_falls_back_to_curated_without_a_cache() {
        // A fresh install has no cache; the picker must still be usable.
        let empty = tempfile::TempDir::new().unwrap();
        let entries = model_entries_for_provider("openai", Some(empty.path()));
        let curated = crate::onboard::wizard::curated_models_for_provider("openai");

        assert_eq!(entries.len(), curated.len());
        assert!(entries.iter().all(|e| !e.description.is_empty()));
    }
}
