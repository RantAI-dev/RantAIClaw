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
    model_entries_for_provider_with_active(provider, workspace_dir, "")
}

/// [`model_entries_for_provider`] plus the active model when the catalog lacks
/// it, so the row the operator is on is always in the picker. The list is the
/// same one the setup model step offers
/// (`onboard::wizard::provider_model_list`).
fn model_entries_for_provider_with_active(
    provider: &str,
    workspace_dir: Option<&std::path::Path>,
    active_model: &str,
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

    crate::onboard::wizard::provider_model_list(workspace_dir, provider, active_model)
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
/// model ids may themselves contain `:` (ollama `llama3:8b`), so for an
/// ordinary provider only the first segment is the provider. A bare string
/// with no `:` (or an empty first segment) names a model only — the caller
/// keeps the current provider.
///
/// A `custom:<base url>` provider is the exception: its name embeds a URL, so
/// it contains colons of its own (`custom:http://host:port/v1`). When the
/// target starts with `custom:` and the rest holds a `://`, the provider is the
/// whole `custom:<url>` and the model follows the first `:` after the URL's
/// last `/`. The limit: a base URL with no path, or a model id containing `/`,
/// is not read correctly, the same ambiguity a typed target always had. A `custom:model`
/// with no URL has no embedded name and splits on its first colon.
pub(crate) fn split_model_target(target: &str) -> (Option<&str>, &str) {
    if let Some(rest) = target.strip_prefix("custom:") {
        if rest.contains("://") {
            // The URL's path ends at its last `/`; the model follows the first
            // `:` after that. Splitting at the very last `:` would wrongly cut a
            // model id such as `llama3:8b`.
            if let Some(slash) = rest.rfind('/') {
                if let Some(colon) = rest[slash..].find(':') {
                    let cut = slash + colon;
                    let (url, model) = (&rest[..cut], &rest[cut + 1..]);
                    if !url.is_empty() && !model.is_empty() {
                        let provider_len = "custom:".len() + url.len();
                        return (Some(&target[..provider_len]), model);
                    }
                }
            }
        }
    }
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

        // The active model belongs to one provider. It is appended only to that
        // provider's list, never to another's.
        let (active_provider, active_model) = split_model_target(&ctx.model);
        for provider in &providers {
            let active = match active_provider {
                Some(ap) if ap == provider => active_model,
                _ => "",
            };
            entries.extend(model_entries_for_provider_with_active(
                provider,
                ctx.workspace_dir.as_deref(),
                active,
            ));
        }

        let items: Vec<ListPickerItem> = entries
            .iter()
            .map(|e| ListPickerItem {
                key: e.target(),
                primary: e.target(),
                secondary: e.description.clone(),
                disabled: false,
            })
            .collect();

        // The empty text must be true. With no provider configured, setup is
        // the fix. With a provider configured but no models listed, the operator
        // can still name one: `/model <provider:model>` sets it.
        let known_provider = providers.iter().find(|p| !p.is_empty());
        let empty_hint = match known_provider {
            Some(provider) => format!(
                "No models are known for {provider}. Set one by name with `/model <provider:model>`."
            ),
            None => "No providers with credentials detected. Run `rantaiclaw setup provider`."
                .to_string(),
        };
        let picker = ListPicker::new(
            ListPickerKind::Model,
            "Select Model",
            items,
            Some(&ctx.model),
            empty_hint,
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

        // "not reported" rather than `0`: the agent emits a usage event only
        // when the provider sent one, and several backends never do. A zero
        // here claimed a measurement that was never taken.
        let counts = ctx.token_usage.as_ref().map_or_else(
            || {
                [
                    "not reported".to_string(),
                    "not reported".to_string(),
                    "not reported".to_string(),
                ]
            },
            |u| {
                [
                    u.prompt_tokens.to_string(),
                    u.completion_tokens.to_string(),
                    u.total_tokens.to_string(),
                ]
            },
        );
        let panel = InfoPanel::new("Token Usage")
            .with_subtitle("this session")
            .with_footer("Esc close · `/insights` for cumulative stats")
            .section(
                InfoSection::new("Tokens")
                    .key_value("Prompt", &counts[0])
                    .key_value("Completion", &counts[1])
                    .key_value("Total", &counts[2]),
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

    /// Rows from every section of the panel a command produced, as
    /// `"key = value"` — enough to assert what an operator would read.
    fn panel_rows(result: &CommandResult) -> Vec<String> {
        let CommandResult::OpenInfoPanel(panel) = result else {
            panic!("expected an info panel");
        };
        panel
            .sections
            .iter()
            .flat_map(|s| s.rows.iter())
            .filter_map(|row| match row {
                crate::tui::widgets::InfoRow::KeyValue { key, value } => {
                    Some(format!("{key} = {value}"))
                }
                _ => None,
            })
            .collect()
    }

    /// `/usage` used to render `0` for a session no provider reported usage
    /// for. A turn that consumed zero tokens does not exist, so that zero could
    /// only ever have meant "nobody told us" — and several backends never do.
    #[test]
    fn usage_says_not_reported_when_no_provider_reported() {
        let mut ctx = test_context();
        assert!(ctx.token_usage.is_none(), "fixture starts unreported");

        let result = UsageCommand.execute("", &mut ctx).expect("panel");
        let rows = panel_rows(&result);
        for key in ["Prompt", "Completion", "Total"] {
            assert!(
                rows.contains(&format!("{key} = not reported")),
                "{key} must not be shown as a measured zero: {rows:?}"
            );
        }
    }

    /// The other half: real numbers still render as numbers.
    #[test]
    fn usage_shows_the_counts_a_provider_reported() {
        let mut ctx = test_context();
        ctx.token_usage = Some(crate::tui::context::TokenUsage {
            prompt_tokens: 120,
            completion_tokens: 34,
            total_tokens: 154,
        });

        let rows = panel_rows(&UsageCommand.execute("", &mut ctx).expect("panel"));
        assert!(rows.contains(&"Prompt = 120".to_string()), "{rows:?}");
        assert!(rows.contains(&"Completion = 34".to_string()), "{rows:?}");
        assert!(rows.contains(&"Total = 154".to_string()), "{rows:?}");
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

    /// A `custom:<url>` provider name embeds a URL and so contains colons.
    /// Picker rows for it are `custom:<url>:<model>`, and the split must return
    /// the whole `custom:<url>` as the provider.
    #[test]
    fn split_target_custom_endpoint_keeps_its_url_as_the_provider() {
        assert_eq!(
            split_model_target("custom:http://127.0.0.1:28521/v1:llama-a"),
            (Some("custom:http://127.0.0.1:28521/v1"), "llama-a")
        );
        assert_eq!(
            split_model_target("custom:http://h:1/v1:llama3:8b"),
            (Some("custom:http://h:1/v1"), "llama3:8b")
        );
    }

    /// Without a URL there is no embedded provider name: the legacy form
    /// `custom:model` splits on its first colon, as every other provider does.
    #[test]
    fn split_target_custom_without_a_url_splits_on_the_first_colon() {
        assert_eq!(
            split_model_target("custom:model"),
            (Some("custom"), "model")
        );
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

    /// On a `custom:<url>` session the `/model` picker, run end to end, offers
    /// the endpoint's cached models plus the active one, once each. The active
    /// model's provider is the whole `custom:<url>`, not `custom`.
    #[test]
    fn picker_on_a_custom_session_offers_the_cache_and_the_active_model_once() {
        let url = "custom:http://127.0.0.1:28521/v1";
        let workspace = workspace_with_cached_models(url, &["llama-a", "llama-b", "llama-c"]);
        let mut ctx = test_context();
        ctx.available_providers = vec![url.to_string()];
        ctx.model = format!("{url}:my-own-model");
        ctx.workspace_dir = Some(workspace.path().to_path_buf());

        let CommandResult::OpenListPicker(picker) = ModelCommand.execute("", &mut ctx).unwrap()
        else {
            panic!("expected OpenListPicker");
        };
        let keys: Vec<String> = picker
            .entries()
            .iter()
            .filter_map(|e| e.as_item().map(|i| i.key.clone()))
            .collect();

        assert_eq!(keys.len(), 4, "3 cached + the active model: {keys:?}");
        for id in ["llama-a", "llama-b", "llama-c", "my-own-model"] {
            assert!(
                keys.contains(&format!("{url}:{id}")),
                "{id} is offered: {keys:?}"
            );
        }
    }

    /// Boot path: `rantaiclaw chat` starts with `ctx.model` = `provider:model`
    /// (the label `boot_model_label` builds). The picker must then offer the
    /// catalog plus the active model, not the catalog alone.
    #[test]
    fn picker_at_boot_with_a_full_label_offers_the_catalog_plus_the_active_model() {
        let models: Vec<String> = (0..150).map(|i| format!("openai/cached-{i:03}")).collect();
        let refs: Vec<&str> = models.iter().map(String::as_str).collect();
        let workspace = workspace_with_cached_models("openai", &refs);
        let mut ctx = test_context();
        ctx.available_providers = vec!["openai".to_string()];
        ctx.model = "openai:openai/active-model".to_string();
        ctx.workspace_dir = Some(workspace.path().to_path_buf());

        let CommandResult::OpenListPicker(picker) = ModelCommand.execute("", &mut ctx).unwrap()
        else {
            panic!("expected OpenListPicker");
        };
        let keys: Vec<String> = picker
            .entries()
            .iter()
            .filter_map(|e| e.as_item().map(|i| i.key.clone()))
            .collect();

        assert_eq!(keys.len(), 151, "150 cached + the active model");
        assert!(keys.contains(&"openai:openai/active-model".to_string()));
    }

    /// The picker offers a 150-model catalog and the active model: 151 rows,
    /// not the 120 the setup step used to cap at.
    #[test]
    fn picker_offers_the_whole_catalog_plus_the_active_model() {
        let models: Vec<String> = (0..150).map(|i| format!("openai/cached-{i:03}")).collect();
        let refs: Vec<&str> = models.iter().map(String::as_str).collect();
        let workspace = workspace_with_cached_models("openai", &refs);

        let entries = model_entries_for_provider_with_active(
            "openai",
            Some(workspace.path()),
            "openai/active-model",
        );

        assert_eq!(entries.len(), 151);
        assert!(entries.iter().any(|e| e.model_id == "openai/active-model"));
    }

    /// `custom:<url>` with a cache of 3: the picker offers those 3 and the
    /// active model. Before, the picker was empty for a custom provider.
    #[test]
    fn picker_offers_a_custom_endpoints_cached_models_and_the_active_one() {
        let workspace = workspace_with_cached_models(
            "custom:http://localhost:1234",
            &["llama-a", "llama-b", "llama-c"],
        );

        let entries = model_entries_for_provider_with_active(
            "custom:http://localhost:1234",
            Some(workspace.path()),
            "my-own-model",
        );

        let ids: Vec<&str> = entries.iter().map(|e| e.model_id.as_str()).collect();
        assert_eq!(ids, vec!["llama-a", "llama-b", "llama-c", "my-own-model"]);
    }

    /// A provider is known but its list is empty: the picker must not say "no
    /// providers with credentials" (that is false, the provider is configured);
    /// it says no models are known and how to set one by name.
    #[test]
    fn empty_picker_for_a_known_provider_says_no_models_are_known() {
        let mut ctx = test_context();
        ctx.available_providers = vec!["some-unlisted-provider".to_string()];
        ctx.workspace_dir = Some(tempfile::TempDir::new().unwrap().keep());

        let CommandResult::OpenListPicker(picker) = ModelCommand.execute("", &mut ctx).unwrap()
        else {
            panic!("expected OpenListPicker");
        };

        assert!(
            picker.empty_hint.contains("some-unlisted-provider")
                && picker.empty_hint.contains("/model <provider:model>"),
            "the hint names the provider and the by-name form: {}",
            picker.empty_hint
        );
        assert!(!picker.empty_hint.contains("No providers with credentials"));
    }

    /// No provider is configured at all: the picker keeps the setup hint, which
    /// is true in this case.
    #[test]
    fn empty_picker_with_no_provider_points_to_setup() {
        let mut ctx = test_context();
        ctx.available_providers = Vec::new();
        ctx.model = String::new();

        let CommandResult::OpenListPicker(picker) = ModelCommand.execute("", &mut ctx).unwrap()
        else {
            panic!("expected OpenListPicker");
        };

        assert!(
            picker.empty_hint.contains("No providers with credentials"),
            "{}",
            picker.empty_hint
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
