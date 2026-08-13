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
            if provider.trim().is_empty() {
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
