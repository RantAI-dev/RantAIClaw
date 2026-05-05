//! Embedding Routes provisioner — implements [`TuiProvisioner`] for in-TUI embedding routing rules setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::EmbeddingRouteConfig;
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const EMBEDDING_ROUTES_NAME: &str = "embedding-routes";
pub const EMBEDDING_ROUTES_DESC: &str =
    "Embedding routing rules — route hint:<name> to specific embedding provider+model";

#[derive(Debug, Clone)]
pub struct EmbeddingRoutesProvisioner;

impl EmbeddingRoutesProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EmbeddingRoutesProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for EmbeddingRoutesProvisioner {
    fn name(&self) -> &'static str {
        EMBEDDING_ROUTES_NAME
    }

    fn description(&self) -> &'static str {
        EMBEDDING_ROUTES_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Routing
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
                text: "Let's configure embedding routes.".into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Embedding routes let you route embedding requests to specific providers."
                    .into(),
            },
        )
        .await?;

        let mut routes = config.embedding_routes.clone();

        loop {
            send(
                &events,
                ProvisionEvent::Choose {
                    id: "action".into(),
                    label: "Embedding routes".into(),
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
                    label: "Route hint name (e.g. semantic, archive, faq)".into(),
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
                    label: "Target provider (e.g. openai, cohere)".into(),
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
                    label: "Embedding model".into(),
                    default: None,
                    secret: false,
                },
            )
            .await?;

            let model = recv_text(&mut responses).await?;
            if model.trim().is_empty() {
                continue;
            }

            routes.push(EmbeddingRouteConfig {
                hint: hint.trim().to_string(),
                provider: provider.trim().to_string(),
                model: model.trim().to_string(),
                dimensions: None,
                api_key: None,
            });

            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Success,
                    text: format!(
                        "Embedding route added: {} → {}:{}",
                        hint.trim(),
                        provider.trim(),
                        model.trim()
                    ),
                },
            )
            .await?;
        }

        config.embedding_routes = routes;

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!(
                    "Embedding routes: {} rules configured.",
                    config.embedding_routes.len()
                ),
            },
        )
        .await?;

        Ok(())
    }
}

use crate::onboard::provision::ProvisionerCategory;

async fn send(
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    ev: ProvisionEvent,
) -> Result<()> {
    events
        .send(ev)
        .await
        .map_err(|e| anyhow::anyhow!("send failed: {e}"))
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
    fn provisioner_name_is_embedding_routes() {
        assert_eq!(EmbeddingRoutesProvisioner::new().name(), "embedding-routes");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!EmbeddingRoutesProvisioner::new().description().is_empty());
    }
}
