//! Provider provisioner — implements [`TuiProvisioner`] for in-TUI LLM provider + API key + model setup.
//!
//! Mirrors the legacy flow in [`crate::onboard::wizard::setup_provider`]:
//!   1. Choose provider tier
//!   2. Choose specific provider
//!   3. Prompt for API key (validation via probe against /v1/models)
//!   4. Fetch and select model
//!   5. Write config
//!
//! Config writes: `config.api_key`, `config.default_provider`, `config.default_model`, `config.api_url`

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner};
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::validate::http::probe_get;
use crate::profile::Profile;
use anyhow::{anyhow, Result};
use async_trait::async_trait;

pub const PROVIDER_NAME: &str = "provider";
pub const PROVIDER_DESC: &str = "AI provider, API key, and default model";

#[derive(Debug, Clone)]
pub struct ProviderProvisioner;

impl ProviderProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ProviderProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for ProviderProvisioner {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    fn description(&self) -> &'static str {
        PROVIDER_DESC
    }

    async fn run(
        &self,
        config: &mut Config,
        _profile: &Profile,
        io: ProvisionIo,
    ) -> Result<ProvisionOutcome> {
        let ProvisionIo {
            events,
            mut responses,
        } = io;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Let's configure your AI provider.".into(),
            },
        )
        .await?;

        // ── Tier selection ─────────────────────────────────────────
        // One table shared with the CLI wizard. This used to be a
        // hand-copy that drifted 11 providers behind (openai-codex,
        // astrai, kimi-code, qwen-code, glm-cn, minimax-cn, qwen-intl,
        // qwen-us, zai-cn, synthetic, opencode).
        let tiers: Vec<String> = crate::onboard::wizard::PROVIDER_SETUP_TIERS
            .iter()
            .map(|t| t.label.to_string())
            .collect();

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Step 1/4 — provider tier".into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Choose {
                id: "tier".into(),
                label: "Select provider category".into(),
                options: tiers.clone(),
                multi: false,
            },
        )
        .await?;

        let tier_sel = recv_selection(&mut responses).await?;
        let tier_idx = tier_sel.first().copied().unwrap_or(0);

        // ── Provider selection ─────────────────────────────────────
        let providers: Vec<(&str, &str)> = crate::onboard::wizard::PROVIDER_SETUP_TIERS
            .get(tier_idx)
            .map(|t| t.providers.to_vec())
            .unwrap_or_default();

        if providers.is_empty() {
            // Custom provider
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Info,
                    text: "Custom provider setup".into(),
                },
            )
            .await?;

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "custom_url".into(),
                    label: "API base URL (e.g. http://localhost:1234)".into(),
                    default: Some("http://localhost:1234".into()),
                    secret: false,
                },
            )
            .await?;

            let base_url = recv_text(&mut responses).await?;
            let base_url = base_url.trim().trim_end_matches('/').to_string();
            if base_url.is_empty() {
                send(
                    &events,
                    ProvisionEvent::Failed {
                        error: "Custom provider requires a base URL.".into(),
                    },
                )
                .await?;
                return Ok(ProvisionOutcome::Aborted(
                    "Custom provider requires a base URL.".into(),
                ));
            }

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "custom_key".into(),
                    label: "API key (Enter to skip)".into(),
                    default: None,
                    secret: true,
                },
            )
            .await?;

            let api_key = recv_text(&mut responses).await?;

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "custom_model".into(),
                    label: "Model name (e.g. llama3, gpt-4o)".into(),
                    default: Some("default".into()),
                    secret: false,
                },
            )
            .await?;

            let model = recv_text(&mut responses).await?;
            let model = if model.trim().is_empty() {
                "default".to_string()
            } else {
                model
            };

            config.default_provider = Some(format!("custom:{base_url}"));
            config.api_url = Some(base_url.clone());
            if !api_key.trim().is_empty() {
                config.api_key = Some(api_key);
            }
            config.default_model = Some(model);

            send(
                &events,
                ProvisionEvent::Done {
                    summary: format!("Custom provider configured: custom:{}", base_url),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Configured);
        }

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Step 2/4 — specific provider".into(),
            },
        )
        .await?;

        let provider_labels: Vec<String> = providers
            .iter()
            .map(|(_, label)| label.to_string())
            .collect();

        send(
            &events,
            ProvisionEvent::Choose {
                id: "provider".into(),
                label: "Select AI provider".into(),
                options: provider_labels.clone(),
                multi: false,
            },
        )
        .await?;

        let provider_sel = recv_selection(&mut responses).await?;
        let provider_idx = provider_sel.first().copied().unwrap_or(0);
        let (provider_name, _provider_label) = providers
            .get(provider_idx)
            .copied()
            .ok_or_else(|| anyhow!("invalid provider selection"))?;

        // ── API key ────────────────────────────────────────────────
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: format!("Selected: {provider_name}"),
            },
        )
        .await?;

        let needs_key = !matches!(provider_name, "ollama" | "llamacpp");

        let mut api_key = String::new();
        let mut provider_api_url: Option<String> = None;

        if needs_key {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Info,
                    text: "Step 3/4 — API key".into(),
                },
            )
            .await?;

            let prompt_label: String = if provider_name == "gemini" {
                "Gemini API key (Enter to skip if using CLI auth)".into()
            } else {
                "API key".into()
            };

            // Retry loop. As of v0.6.53-alpha, hitting 401/403 no longer
            // silently advances to the next step — the user gets a
            // three-way choice (Re-enter / Continue anyway / Abort).
            // Default is "re-enter", because the most common cause is a
            // typo or copy-paste truncation.
            //
            // The loop also handles transient validation network errors
            // separately: those go straight through with a warning (we
            // don't want to block setup just because the validation
            // endpoint is temporarily unreachable).
            loop {
                send(
                    &events,
                    ProvisionEvent::Prompt {
                        id: "api_key".into(),
                        label: prompt_label.clone(),
                        default: None,
                        secret: true,
                    },
                )
                .await?;

                api_key = recv_text(&mut responses).await?;

                if api_key.trim().is_empty() {
                    // Empty key: skip validation only when this provider can
                    // actually construct without one. The factory is the same
                    // oracle boot uses — including the per-provider env-var
                    // fallback (OPENAI_API_KEY etc.) — so what passes here is
                    // exactly what will start later. Keyless-capable flows
                    // (ollama, gemini CLI auth, exported env key) sail
                    // through; a provider that cannot build without a key
                    // (openai/anthropic/gemini route through rig and fail at
                    // construction) must not be saved silently: that config
                    // used to abort every later launch — including
                    // `rantaiclaw setup`, the repair path — before the TUI
                    // existed.
                    if crate::providers::create_provider(provider_name, None).is_ok() {
                        break;
                    }
                    send(
                        &events,
                        ProvisionEvent::Message {
                            severity: Severity::Warn,
                            text: format!(
                                "{provider_name} cannot start without an API key — \
                                 saving it keyless would break the next launch."
                            ),
                        },
                    )
                    .await?;
                    send(
                        &events,
                        ProvisionEvent::Choose {
                            id: "empty_key_retry".into(),
                            label: "What would you like to do?".into(),
                            options: vec![
                                "Re-enter the API key".into(),
                                "Abort setup (nothing will be saved)".into(),
                            ],
                            multi: false,
                        },
                    )
                    .await?;
                    let choice = recv_selection(&mut responses).await?;
                    match choice.first().copied() {
                        Some(1) | None => {
                            send(
                                &events,
                                ProvisionEvent::Failed {
                                    error: format!(
                                        "{provider_name} requires an API key; setup aborted."
                                    ),
                                },
                            )
                            .await?;
                            return Ok(ProvisionOutcome::Aborted(format!(
                                "{provider_name} requires an API key."
                            )));
                        }
                        // Some(0) = re-enter; an unknown index re-prompts
                        // too, the safest reading of a malformed selection.
                        _ => continue,
                    }
                }

                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Info,
                        text: "Validating API key…".into(),
                    },
                )
                .await?;

                // Ask `doctor` where this provider's `/models` lives. It is the
                // same question that check answers, and it resolves
                // region-varying families from the constants `create_provider`
                // builds the client with.
                //
                // This replaced a local table of bare hostnames. That table had
                // drifted badly — two of its entries named domains that do not
                // exist (`api.zPUmlw.com` for Z.AI, `api.moonshot.io` for
                // Moonshot International), and several others disagreed with
                // the host the client actually talks to. Since this probe sends
                // the user's freshly-entered API key as a bearer token, a wrong
                // hostname here is not a broken check — it is a credential
                // handed to whoever controls that name.
                let Some(validation_url) =
                    crate::doctor::checks::provider::resolve_endpoint(provider_name, None)
                else {
                    // No known endpoint: accept the key unvalidated rather than
                    // send it somewhere guessed. Setup continues; `doctor`
                    // reports the same gap later.
                    send(
                        &events,
                        ProvisionEvent::Message {
                            severity: Severity::Info,
                            text: format!(
                                "No validation endpoint known for {provider_name} — saving the key unchecked."
                            ),
                        },
                    )
                    .await?;
                    break;
                };

                // Anthropic rejects Bearer for a real API key — build the probe
                // headers through the shared helper so setup agrees with doctor
                // and a valid `sk-ant-api…` key is not reported rejected.
                let probe_headers =
                    crate::doctor::checks::provider::probe_auth_headers(provider_name, &api_key);
                let header_refs: Vec<(&str, &str)> = probe_headers
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str()))
                    .collect();
                let probe = probe_get(&validation_url, &header_refs).await;

                match probe {
                    Ok(result) if result.status == 401 || result.status == 403 => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Warn,
                                text: format!(
                                    "API key rejected ({}). Re-enter the key, continue with it anyway, or abort setup.",
                                    result.status
                                ),
                            },
                        )
                        .await?;
                        send(
                            &events,
                            ProvisionEvent::Choose {
                                id: "api_key_retry".into(),
                                label: "What would you like to do?".into(),
                                options: vec![
                                    "Re-enter the API key".into(),
                                    "Continue anyway (proceed with this key)".into(),
                                    "Abort setup".into(),
                                ],
                                multi: false,
                            },
                        )
                        .await?;
                        let choice = recv_selection(&mut responses).await?;
                        match choice.first().copied() {
                            Some(0) => continue, // re-prompt for the key
                            Some(1) => break,    // keep the rejected key
                            Some(2) | None => {
                                anyhow::bail!("setup aborted by user after invalid API key");
                            }
                            Some(_) => continue, // unknown index — safest is to re-prompt
                        }
                    }
                    Err(e) => {
                        send(&events, ProvisionEvent::Message {
                            severity: Severity::Warn,
                            text: format!("Could not validate key (network error): {e}. Continuing anyway…"),
                        }).await?;
                        break;
                    }
                    Ok(_) => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Success,
                                text: "API key validated successfully.".into(),
                            },
                        )
                        .await?;
                        break;
                    }
                }
            }
        } else {
            // Ollama / llamacpp — no key needed typically
            if provider_name == "ollama" {
                send(
                    &events,
                    ProvisionEvent::Prompt {
                        id: "ollama_url".into(),
                        label: "Ollama endpoint URL (Enter for default http://localhost:11434)"
                            .into(),
                        default: Some("http://localhost:11434".into()),
                        secret: false,
                    },
                )
                .await?;
                let url = recv_text(&mut responses).await?;
                let url = url.trim().trim_end_matches('/').to_string();
                if !url.is_empty() && url != "http://localhost:11434" {
                    provider_api_url = Some(url);
                }
            }
        }

        // ── Model selection ─────────────────────────────────────────
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Step 4/4 — default model".into(),
            },
        )
        .await?;

        // Read the same catalog every other surface uses (`/model` picker,
        // gateway, channel routing): cached live models when `models
        // refresh`/the wizard has written them, curated otherwise. This
        // used to read the curated list alone — ~10 rows for openrouter
        // while the same binary's /model picker offered 400 from the cache.
        // Capped like the CLI wizard's Select: the setup Choose overlay has
        // no filter box, and the full openrouter list is 400 rows.
        let catalog =
            crate::onboard::wizard::provider_model_catalog(&config.workspace_dir, provider_name);
        let source = catalog.source;
        let curated = crate::onboard::wizard::curated_models_for_provider(provider_name);
        let describe = |id: &str| {
            curated
                .iter()
                .find(|(curated_id, _)| curated_id == id)
                .map(|(_, description)| description.clone())
        };
        let (model_ids, model_labels): (Vec<String>, Vec<String>) = if catalog.models.is_empty() {
            // No cache and no curated list — fall back to a single
            // "default" option so the user still has something to pick.
            let fallback = crate::onboard::wizard::default_model_for_provider(provider_name);
            (
                vec![fallback.clone()],
                vec![format!("{fallback} (default)")],
            )
        } else {
            catalog
                .models
                .into_iter()
                .take(crate::onboard::wizard::LIVE_MODEL_MAX_OPTIONS)
                .map(|id| {
                    let label = match describe(&id) {
                        Some(desc) => format!("{id}  —  {desc}"),
                        None => format!("{id}  ({source})"),
                    };
                    (id, label)
                })
                .unzip()
        };

        send(
            &events,
            ProvisionEvent::Choose {
                id: "model".into(),
                label: "Select default model".into(),
                options: model_labels,
                multi: false,
            },
        )
        .await?;

        let model_sel = recv_selection(&mut responses).await?;
        let model_idx = model_sel.first().copied().unwrap_or(0);
        let model = model_ids
            .get(model_idx)
            .cloned()
            .unwrap_or_else(|| crate::onboard::wizard::default_model_for_provider(provider_name));

        // ── Write config ────────────────────────────────────────────
        config.default_provider = Some(provider_name.to_string());
        config.api_key = if api_key.trim().is_empty() {
            None
        } else {
            Some(api_key)
        };
        config.default_model = Some(model);
        if let Some(url) = provider_api_url {
            config.api_url = Some(url);
        }

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!(
                    "Provider configured: {} with model {}",
                    config.default_provider.as_deref().unwrap_or("?"),
                    config.default_model.as_deref().unwrap_or("?")
                ),
            },
        )
        .await?;

        Ok(ProvisionOutcome::Configured)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::onboard::provision::test_support::{drive, scratch_profile, Answer};

    /// Save an env var's previous value and restore it on drop, so a
    /// panicking assert doesn't leak state into the next test. Only
    /// meaningful while `test_env::ENV_LOCK` is held.
    struct VarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl VarGuard {
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prev }
        }

        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prev }
        }
    }

    impl Drop for VarGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    // Tier 0 picker index, and provider rows derived from the shared table —
    // a hardcoded row index silently repoints these tests at a different
    // provider whenever the table gains or reorders an entry.
    const PICK_TIER_RECOMMENDED: usize = 0;
    fn pick_provider(id: &str) -> usize {
        crate::onboard::wizard::PROVIDER_SETUP_TIERS[PICK_TIER_RECOMMENDED]
            .providers
            .iter()
            .position(|(pid, _)| *pid == id)
            .unwrap_or_else(|| panic!("{id} missing from tier 0 of the shared table"))
    }

    /// The lockout producer. An empty key for a provider that cannot
    /// construct without one (`openai` routes through rig, which fails at
    /// construction) used to be saved silently — and every later launch,
    /// including `rantaiclaw setup`, then died with
    /// "openai: OPENAI_API_KEY required" before any UI existed.
    #[tokio::test]
    async fn empty_key_for_key_required_provider_prompts_and_aborts() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _key = VarGuard::unset("OPENAI_API_KEY");

        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &ProviderProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Pick(PICK_TIER_RECOMMENDED),
                Answer::Pick(pick_provider("openai")),
                Answer::Text(""),
                Answer::Pick(1), // Abort setup
            ],
        )
        .await;

        assert!(
            t.aborted(),
            "an empty key for openai must abort, got {:?}",
            t.outcome
        );
        assert!(
            t.events.iter().any(|e| matches!(
                e,
                super::ProvisionEvent::Choose { id, .. } if id == "empty_key_retry"
            )),
            "the operator must be offered the re-enter/abort choice"
        );
        assert_eq!(
            config.default_provider.as_deref(),
            Some("openrouter"),
            "an aborted provisioner must not overwrite default_provider"
        );
        assert!(config.api_key.is_none(), "no key must be written");
    }

    /// "Re-enter the API key" loops back to the prompt instead of aborting.
    #[tokio::test]
    async fn empty_key_reenter_choice_prompts_again() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _key = VarGuard::unset("OPENAI_API_KEY");

        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &ProviderProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Pick(PICK_TIER_RECOMMENDED),
                Answer::Pick(pick_provider("openai")),
                Answer::Text(""),
                Answer::Pick(0), // Re-enter the API key
                Answer::Text(""),
                Answer::Pick(1), // Abort setup
            ],
        )
        .await;

        let key_prompts = t.prompts().iter().filter(|l| l.contains("API key")).count();
        assert_eq!(key_prompts, 2, "re-enter must re-open the key prompt");
        assert!(t.aborted(), "second abort must still abort");
    }

    /// The capability split: openrouter constructs keyless
    /// (`factory_openrouter` pins it), so an empty key sails through
    /// exactly as before the gate.
    #[tokio::test]
    async fn empty_key_for_keyless_capable_provider_saves() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &ProviderProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Pick(PICK_TIER_RECOMMENDED),
                Answer::Pick(pick_provider("openrouter")),
                Answer::Text(""),
                Answer::Pick(0), // default model
            ],
        )
        .await;

        assert!(
            t.configured(),
            "openrouter with an empty key must configure, got {:?}",
            t.outcome
        );
        assert_eq!(config.default_provider.as_deref(), Some("openrouter"));
        assert!(config.api_key.is_none());
    }

    /// The model step must offer what `models refresh` cached — the
    /// curated-only regression showed ~10 openrouter rows while /model
    /// offered 400 from the same cache on the same box.
    #[tokio::test]
    async fn model_step_offers_cached_models_not_just_curated() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();
        config.workspace_dir = tmp.path().to_path_buf();

        let cache_only = "openrouter/some-model-only-the-live-catalog-has";
        assert!(
            !crate::onboard::wizard::curated_models_for_provider("openrouter")
                .iter()
                .any(|(id, _)| id == cache_only),
            "fixture must not collide with the curated list"
        );
        crate::onboard::wizard::cache_live_models_for_provider(
            tmp.path(),
            "openrouter",
            &[cache_only.to_string()],
        )
        .expect("write cache");

        let t = drive(
            &ProviderProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Pick(PICK_TIER_RECOMMENDED),
                Answer::Pick(pick_provider("openrouter")),
                Answer::Text(""), // keyless — openrouter builds without a key
                Answer::Pick(0),  // the only model row is the cached one
            ],
        )
        .await;

        assert!(
            t.events.iter().any(|e| matches!(
                e,
                super::ProvisionEvent::Choose { id, options, .. }
                    if id == "model" && options.iter().any(|o| o.contains(cache_only))
            )),
            "the model Choose must surface what `models refresh` wrote"
        );
        assert!(t.configured(), "flow must configure, got {:?}", t.outcome);
        assert_eq!(config.default_model.as_deref(), Some(cache_only));
    }

    /// The gate consults the same oracle boot uses — including the
    /// per-provider env-var fallback. An exported OPENAI_API_KEY means an
    /// empty config key is a working setup, so no prompt appears.
    #[tokio::test]
    async fn exported_env_key_lets_empty_config_key_through() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _key = VarGuard::set("OPENAI_API_KEY", "sk-test-env-key");

        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &ProviderProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Pick(PICK_TIER_RECOMMENDED),
                Answer::Pick(pick_provider("openai")),
                Answer::Text(""),
                Answer::Pick(0), // default model
            ],
        )
        .await;

        assert!(
            t.configured(),
            "an exported env key must let the empty config key through, got {:?}",
            t.outcome
        );
        assert_eq!(config.default_provider.as_deref(), Some("openai"));
        assert!(config.api_key.is_none());
    }
}
