//! Model Routes provisioner — implements [`TuiProvisioner`] for in-TUI model routing rules setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::ModelRouteConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::ProvisionerCategory;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const MODEL_ROUTES_NAME: &str = "model-routes";
pub const MODEL_ROUTES_DESC: &str =
    "Model routing rules — route hint:<name> to specific provider+model combos";

#[derive(Debug, Clone)]
pub struct ModelRoutesProvisioner;

impl ModelRoutesProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ModelRoutesProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for ModelRoutesProvisioner {
    fn name(&self) -> &'static str {
        MODEL_ROUTES_NAME
    }

    fn description(&self) -> &'static str {
        MODEL_ROUTES_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Routing
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
                text: "Let's configure model routes.".into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text:
                    "Routes let you route messages matching a regex to a specific provider+model."
                        .into(),
            },
        )
        .await?;

        let mut routes = config.model_routes.clone();

        loop {
            send(
                &events,
                ProvisionEvent::Choose {
                    id: "action".into(),
                    label: "Model routes".into(),
                    options: vec!["Add a route".to_string(), "Done".to_string()],
                    multi: false,
                    filterable: false,
                },
            )
            .await?;

            let sel = recv_selection(&mut responses).await?;
            if sel.first().copied() != Some(0) {
                break;
            }

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "hint".into(),
                    label: "Route hint name (e.g. reasoning, fast, code, summarize)".into(),
                    default: None,
                    secret: false,
                },
            )
            .await?;

            let hint = recv_text(&mut responses).await?;
            if hint.trim().is_empty() {
                continue;
            }

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "provider".into(),
                    label: "Target provider (e.g. openrouter, anthropic)".into(),
                    default: None,
                    secret: false,
                },
            )
            .await?;

            let provider = recv_text(&mut responses).await?;
            let provider = provider.trim().to_string();
            if provider.is_empty() {
                continue;
            }
            // A removed login is asked first: the factory still builds a
            // provider for those names (so the daemon stays up), which would
            // let this route through and fail on its first request.
            if let Some(refusal) = crate::providers::removed_login_refusal(&provider) {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: format!(
                            "Provider \"{provider}\" cannot be used for a route: {refusal} \
                             Route skipped."
                        ),
                    },
                )
                .await?;
                continue;
            }
            // Reject an unknown provider here rather than accepting a typo that
            // is valid nowhere and fails only at routing time. `create_provider`
            // is the same oracle the doctor `config.schema` check uses.
            if crate::providers::create_provider(&provider, None).is_err() {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: format!(
                            "Unknown provider \"{provider}\" — route skipped. \
                             Use a registered provider (e.g. openrouter, anthropic, openai)."
                        ),
                    },
                )
                .await?;
                continue;
            }

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "model".into(),
                    label: "Target model".into(),
                    default: None,
                    secret: false,
                },
            )
            .await?;

            let model = recv_text(&mut responses).await?;
            if model.trim().is_empty() {
                continue;
            }

            routes.push(ModelRouteConfig {
                hint: hint.trim().to_string(),
                provider: provider.trim().to_string(),
                model: model.trim().to_string(),
                api_key: None,
            });

            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Success,
                    text: format!(
                        "Route added: {} → {}:{}",
                        hint.trim(),
                        provider.trim(),
                        model.trim()
                    ),
                },
            )
            .await?;
        }

        config.model_routes = routes;

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!(
                    "Model routes: {} rules configured.",
                    config.model_routes.len()
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
    use crate::onboard::provision::traits::ProvisionResponse;

    /// Drive the provisioner through one "Add a route" round and then "Done",
    /// and return the resulting config with every message it sent.
    async fn add_one_route(provider: &str, model: Option<&str>) -> (Config, Vec<String>) {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
        let (resp_tx, resp_rx) = tokio::sync::mpsc::channel(8);
        resp_tx
            .send(ProvisionResponse::Selection(vec![0]))
            .await
            .unwrap(); // Add a route
        resp_tx
            .send(ProvisionResponse::Text("fast".into()))
            .await
            .unwrap(); // hint
        resp_tx
            .send(ProvisionResponse::Text(provider.into()))
            .await
            .unwrap(); // provider
        if let Some(model) = model {
            resp_tx
                .send(ProvisionResponse::Text(model.into()))
                .await
                .unwrap();
        }
        resp_tx
            .send(ProvisionResponse::Selection(vec![1]))
            .await
            .unwrap(); // Done

        let mut config = Config::default();
        let profile = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp"),
        };
        ModelRoutesProvisioner::new()
            .run(
                &mut config,
                &profile,
                ProvisionIo {
                    events: events_tx,
                    responses: resp_rx,
                },
            )
            .await
            .unwrap();

        let mut messages = Vec::new();
        while let Ok(event) = events_rx.try_recv() {
            if let ProvisionEvent::Message { text, .. } = event {
                messages.push(text);
            }
        }
        (config, messages)
    }

    /// The factory still builds a provider for a removed login, so a route to
    /// one passed the `create_provider` check and was saved without a word.
    /// The wizard now skips it and names the provider and the replacement.
    #[tokio::test]
    async fn a_route_to_a_removed_login_is_skipped_with_its_refusal() {
        let (config, messages) = add_one_route("copilot", None).await;

        assert!(
            config.model_routes.is_empty(),
            "a route to a removed login must not be saved: {:?}",
            config.model_routes
        );
        let warning = messages
            .iter()
            .find(|text| text.contains("Route skipped"))
            .unwrap_or_else(|| panic!("the wizard must say the route was skipped: {messages:?}"));
        assert!(warning.contains("copilot"), "{warning}");
        assert!(warning.contains("removed"), "{warning}");
    }

    /// The control: a route to a provider that is still a provider is saved,
    /// so the removed-login check cannot be satisfied by refusing everything.
    #[tokio::test]
    async fn a_route_to_a_registered_provider_is_saved() {
        let (config, _messages) = add_one_route("openrouter", Some("example/model")).await;

        assert_eq!(config.model_routes.len(), 1);
        assert_eq!(config.model_routes[0].provider, "openrouter");
        assert_eq!(config.model_routes[0].model, "example/model");
    }
}
