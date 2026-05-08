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

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner};
use crate::config::Config;
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

    async fn run(&self, config: &mut Config, _profile: &Profile, io: ProvisionIo) -> Result<()> {
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
        let tiers = vec![
            "Recommended (OpenRouter, Venice, Anthropic, OpenAI, Gemini)".to_string(),
            "Fast inference (Groq, Fireworks, Together AI, NVIDIA NIM)".to_string(),
            "Gateway / proxy (Vercel AI, Cloudflare AI, Amazon Bedrock)".to_string(),
            "Specialized (Moonshot/Kimi, GLM/Zhipu, MiniMax, Qwen/DashScope, Qianfan, Z.AI, Cohere)".to_string(),
            "Local / private (Ollama, llama.cpp server — no API key needed)".to_string(),
            "Custom — bring your own OpenAI-compatible API".to_string(),
        ];

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
        let providers: Vec<(&str, &str)> = match tier_idx {
            0 => vec![
                ("openrouter", "OpenRouter — 200+ models, 1 API key"),
                ("venice", "Venice AI — privacy-first"),
                ("anthropic", "Anthropic — Claude direct"),
                ("openai", "OpenAI — GPT-4o, o1, o3"),
                ("deepseek", "DeepSeek — V3 & R1"),
                ("mistral", "Mistral — Large & Codestral"),
                ("xai", "xAI — Grok 3"),
                ("perplexity", "Perplexity — search-augmented AI"),
                ("gemini", "Google Gemini — Gemini 2.0 Flash & Pro"),
            ],
            1 => vec![
                ("groq", "Groq — ultra-fast LPU inference"),
                ("fireworks", "Fireworks AI — fast open-source"),
                ("together-ai", "Together AI — open-source hosting"),
                ("nvidia", "NVIDIA NIM — DeepSeek, Llama, & more"),
            ],
            2 => vec![
                ("vercel", "Vercel AI Gateway"),
                ("cloudflare", "Cloudflare AI Gateway"),
                ("bedrock", "Amazon Bedrock — AWS managed models"),
            ],
            3 => vec![
                ("moonshot", "Moonshot — Kimi API (China)"),
                ("moonshot-intl", "Moonshot — Kimi API (international)"),
                ("glm", "GLM — ChatGLM / Zhipu"),
                ("minimax", "MiniMax — international"),
                ("qwen", "Qwen — DashScope"),
                ("qianfan", "Qianfan — Baidu AI"),
                ("zai", "Z.AI — coding endpoint"),
                ("cohere", "Cohere — Command R+"),
            ],
            4 => vec![
                ("ollama", "Ollama — local models"),
                ("llamacpp", "llama.cpp server — local OpenAI-compatible"),
            ],
            _ => vec![],
        };

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
                return Ok(());
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
            return Ok(());
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

            let prompt_label = if provider_name == "gemini" {
                "Gemini API key (Enter to skip if using CLI auth)".into()
            } else {
                "API key".into()
            };

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "api_key".into(),
                    label: prompt_label,
                    default: None,
                    secret: true,
                },
            )
            .await?;

            api_key = recv_text(&mut responses).await?;

            // Validate against /v1/models if key provided
            if !api_key.trim().is_empty() {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Info,
                        text: "Validating API key…".into(),
                    },
                )
                .await?;

                // Build a `/v1/models` URL from the provider's base.
                // Some bases already include `/v1` (e.g. openai →
                // `api.openai.com/v1`) and some don't (e.g. minimax →
                // `api.minimax.io`). The previous logic
                // unconditionally appended `/v1/models` AND added
                // another `/v1` for non-openrouter providers, producing
                // `api.minimax.io/v1/v1/models` and
                // `api.openai.com/v1/v1/v1/models`. Detect what's
                // already present and only append the missing segments.
                let base = provider_base_url(provider_name);
                let validation_url = if base.contains("/v1") {
                    format!("https://{base}/models")
                } else {
                    format!("https://{base}/v1/models")
                };

                let probe = probe_get(
                    &validation_url,
                    &[("Authorization", &format!("Bearer {api_key}"))],
                )
                .await;

                match probe {
                    Ok(result) if result.status == 401 || result.status == 403 => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Warn,
                                text: "API key appears invalid (401/403). You can still continue."
                                    .into(),
                            },
                        )
                        .await?;
                    }
                    Err(e) => {
                        send(&events, ProvisionEvent::Message {
                            severity: Severity::Warn,
                            text: format!("Could not validate key (network error): {e}. Continuing anyway…"),
                        }).await?;
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

        // Use the curated model list shared with the legacy wizard so
        // the names stay in sync across both setup paths. Returns
        // `(model_id, description)` tuples; we render description as
        // the Choose option, then map back to the id via `model_ids`.
        let curated = crate::onboard::wizard::curated_models_for_provider(provider_name);
        let (model_ids, model_labels): (Vec<String>, Vec<String>) = if curated.is_empty() {
            // Provider has no curated list — fall back to a single
            // "default" option so the user still has something to pick.
            let fallback = default_model_for_provider(provider_name);
            (
                vec![fallback.clone()],
                vec![format!("{fallback} (default)")],
            )
        } else {
            curated
                .into_iter()
                .map(|(id, desc)| {
                    let label = format!("{id}  —  {desc}");
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
            .unwrap_or_else(|| default_model_for_provider(provider_name));

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

        Ok(())
    }
}

fn provider_base_url(provider: &str) -> &'static str {
    match provider {
        "openrouter" => "openrouter.ai/api",
        "anthropic" => "api.anthropic.com",
        "openai" => "api.openai.com/v1",
        "deepseek" => "api.deepseek.com",
        "mistral" => "api.mistral.ai",
        "xai" => "api.x.ai",
        "perplexity" => "api.perplexity.ai",
        "gemini" => "generativelanguage.googleapis.com",
        "groq" => "api.groq.com/openai/v1",
        "fireworks" => "api.fireworks.ai/inference",
        "together-ai" => "api.together.xyz/v1",
        "nvidia" => "integrate.api.nvidia.com/v1",
        "vercel" => "api.vercel.ai",
        "cloudflare" => "gateway.ai.cloudflare.com/v1",
        "bedrock" => "",
        "moonshot" => "api.moonshot.cn",
        "moonshot-intl" => "api.moonshot.io",
        "glm" => "open.bigmodel.cn",
        "minimax" => "api.minimax.io",
        "qwen" => "dashscope.aliyuncs.com",
        "qianfan" => "qianfan.baidubce.com",
        "zai" => "api.zPUmlw.com",
        "cohere" => "api.cohere.ai",
        _ => "",
    }
}

fn default_model_for_provider(provider: &str) -> String {
    match provider {
        "openrouter" => "anthropic/claude-sonnet-4-20250514".to_string(),
        "anthropic" => "claude-sonnet-4-20250514".to_string(),
        "openai" => "gpt-4o".to_string(),
        "deepseek" => "deepseek-chat".to_string(),
        "mistral" => "mistral-large-latest".to_string(),
        "xai" => "grok-3".to_string(),
        "perplexity" => "sonar".to_string(),
        "gemini" => "gemini-2.0-flash".to_string(),
        "groq" => "llama-3.3-70b-versatile".to_string(),
        "fireworks" => "accounts/fireworks/models/llama-v3.3-70b-instruct".to_string(),
        "together-ai" => "meta-llama/Llama-3.3-70B-Instruct-Turbo".to_string(),
        "nvidia" => "deepseek-ai/DeepSeek-V3".to_string(),
        "vercel" => "gpt-4o".to_string(),
        "cloudflare" => "@cf/meta/llama-3.1-8b-instruct".to_string(),
        "bedrock" => "anthropic.claude-sonnet-4-20250514".to_string(),
        "moonshot" => "moonshot-v1-8k".to_string(),
        "moonshot-intl" => "moonshot-v1-8k".to_string(),
        "glm" => "glm-4".to_string(),
        "minimax" => "abab6.5s-chat".to_string(),
        "qwen" => "qwen-turbo".to_string(),
        "qianfan" => "ernie-4.0-8k".to_string(),
        "zai" => "glm-4".to_string(),
        "cohere" => "command-r-plus".to_string(),
        "ollama" => "llama3".to_string(),
        "llamacpp" => "llama3".to_string(),
        _ => "default".to_string(),
    }
}

async fn send(
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    ev: ProvisionEvent,
) -> Result<()> {
    events
        .send(ev)
        .await
        .map_err(|e| anyhow!("send failed: {e}"))
}

async fn recv_selection(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<Vec<usize>> {
    match responses.recv().await {
        Some(ProvisionResponse::Selection(indices)) => Ok(indices),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

async fn recv_text(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<String> {
    match responses.recv().await {
        Some(ProvisionResponse::Text(t)) => Ok(t),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_provider() {
        let p = ProviderProvisioner::new();
        assert_eq!(p.name(), "provider");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        let p = ProviderProvisioner::new();
        assert!(!p.description().is_empty());
    }
}
