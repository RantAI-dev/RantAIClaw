//! Agents provisioner — implements [`TuiProvisioner`] for in-TUI delegate agent setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::DelegateAgentConfig;
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const AGENTS_NAME: &str = "agents";
pub const AGENTS_DESC: &str = "Delegate agents — researcher, coder, planner, reviewer, debugger";

#[derive(Debug, Clone)]
pub struct AgentsProvisioner;

impl AgentsProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AgentsProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for AgentsProvisioner {
    fn name(&self) -> &'static str {
        AGENTS_NAME
    }

    fn description(&self) -> &'static str {
        AGENTS_DESC
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
                text: "Let's configure delegate agents.".into(),
            },
        )
        .await?;

        let built_in = vec!["researcher", "coder", "planner", "reviewer", "debugger"];

        send(
            &events,
            ProvisionEvent::Choose {
                id: "built_in".into(),
                label: "Enable built-in delegate agents (multi-select)".into(),
                options: built_in.iter().map(|s| s.to_string()).collect(),
                multi: true,
            },
        )
        .await?;

        let picks = recv_selection_multi(&mut responses).await?;

        for (i, name) in built_in.iter().enumerate() {
            if picks.contains(&i) {
                config.agents.insert(
                    name.to_string(),
                    DelegateAgentConfig {
                        provider: "openrouter".to_string(),
                        model: "openrouter/anthropic/claude-3-haiku".to_string(),
                        system_prompt: None,
                        api_key: None,
                        temperature: None,
                        max_depth: 3,
                        agentic: true,
                        allowed_tools: vec![],
                        max_iterations: 10,
                    },
                );
            }
        }

        send(
            &events,
            ProvisionEvent::Choose {
                id: "add_custom".into(),
                label: "Add a custom delegate agent?".into(),
                options: vec!["No".to_string(), "Yes".to_string()],
                multi: false,
            },
        )
        .await?;

        let add_custom = {
            let s = recv_selection(&mut responses).await?;
            s.first().copied() == Some(1)
        };

        if add_custom {
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "custom_name".into(),
                    label: "Custom agent name (slug, e.g. my-agent)".into(),
                    default: None,
                    secret: false,
                },
            )
            .await?;

            let name = recv_text(&mut responses).await?;
            if !name.trim().is_empty() {
                send(
                    &events,
                    ProvisionEvent::Prompt {
                        id: "custom_prompt".into(),
                        label: "System prompt for custom agent".into(),
                        default: None,
                        secret: false,
                    },
                )
                .await?;

                let prompt = recv_text(&mut responses).await?;

                config.agents.insert(
                    name.trim().to_string(),
                    DelegateAgentConfig {
                        provider: "openrouter".to_string(),
                        model: "openrouter/anthropic/claude-3-haiku".to_string(),
                        system_prompt: if prompt.trim().is_empty() {
                            None
                        } else {
                            Some(prompt.trim().to_string())
                        },
                        api_key: None,
                        temperature: None,
                        max_depth: 3,
                        agentic: true,
                        allowed_tools: vec![],
                        max_iterations: 10,
                    },
                );
            }
        }

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!("Agents configured: {} total.", config.agents.len()),
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

async fn recv_selection_multi(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<Vec<usize>> {
    recv_selection(responses).await
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
    fn provisioner_name_is_agents() {
        assert_eq!(AgentsProvisioner::new().name(), "agents");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!AgentsProvisioner::new().description().is_empty());
    }
}
