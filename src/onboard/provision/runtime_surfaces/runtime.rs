//! Runtime provisioner — implements [`TuiProvisioner`] for in-TUI runtime (native/docker) setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::{DockerRuntimeConfig, RuntimeConfig};
use crate::config::Config;
use crate::onboard::provision::validate::process::validate_command_on_path;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const RUNTIME_NAME: &str = "runtime";
pub const RUNTIME_DESC: &str =
    "Runtime — native vs Docker execution, Docker image, resource limits";

#[derive(Debug, Clone)]
pub struct RuntimeProvisioner;

impl RuntimeProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RuntimeProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for RuntimeProvisioner {
    fn name(&self) -> &'static str {
        RUNTIME_NAME
    }

    fn description(&self) -> &'static str {
        RUNTIME_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Runtime
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
                text: "Let's configure the agent runtime.".into(),
            },
        )
        .await?;

        // Kind selection
        send(
            &events,
            ProvisionEvent::Choose {
                id: "kind".into(),
                label: "Runtime kind".into(),
                options: vec![
                    "native (default, recommended)".to_string(),
                    "docker".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let kind = if sel.first().copied() == Some(1) {
            "docker"
        } else {
            "native"
        };

        let mut docker_cfg = DockerRuntimeConfig::default();

        if kind == "docker" {
            // Validate docker is available
            match validate_command_on_path("docker") {
                Ok(_) => {
                    send(
                        &events,
                        ProvisionEvent::Message {
                            severity: Severity::Success,
                            text: "docker found on PATH.".into(),
                        },
                    )
                    .await?;
                }
                Err(e) => {
                    send(
                        &events,
                        ProvisionEvent::Message {
                            severity: Severity::Warn,
                            text: format!(
                                "docker not found: {e}. Config will be saved but runtime may fail."
                            ),
                        },
                    )
                    .await?;
                }
            }

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "image".into(),
                    label: "Docker image (Enter for default rantaiclaw/runtime:latest)".into(),
                    default: Some("rantaiclaw/runtime:latest".into()),
                    secret: false,
                },
            )
            .await?;

            let image = recv_text(&mut responses).await?;
            docker_cfg.image = if image.trim().is_empty() {
                "rantaiclaw/runtime:latest".to_string()
            } else {
                image.trim().to_string()
            };

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "memory_limit".into(),
                    label: "Memory limit in MB (Enter for default 512, 0 = no limit)".into(),
                    default: Some("512".into()),
                    secret: false,
                },
            )
            .await?;

            let mem_str = recv_text(&mut responses).await?;
            docker_cfg.memory_limit_mb = mem_str.trim().parse().ok().filter(|m| *m > 0);

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "cpu_limit".into(),
                    label: "CPU limit (Enter for default 1.0, 0 = no limit)".into(),
                    default: Some("1.0".into()),
                    secret: false,
                },
            )
            .await?;

            let cpu_str = recv_text(&mut responses).await?;
            docker_cfg.cpu_limit = cpu_str.trim().parse().ok().filter(|c| *c > 0.0);

            send(
                &events,
                ProvisionEvent::Choose {
                    id: "mount_workspace".into(),
                    label: "Mount workspace into container?".into(),
                    options: vec!["Yes (recommended)".to_string(), "No".to_string()],
                    multi: false,
                },
            )
            .await?;

            let mount_ws = {
                let s = recv_selection(&mut responses).await?;
                s.first().copied() != Some(1)
            };
            docker_cfg.mount_workspace = mount_ws;
        }

        config.runtime = RuntimeConfig {
            kind: kind.to_string(),
            docker: docker_cfg,
            reasoning_enabled: None,
        };

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!("Runtime configured: {}.", kind),
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
    fn provisioner_name_is_runtime() {
        assert_eq!(RuntimeProvisioner::new().name(), "runtime");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!RuntimeProvisioner::new().description().is_empty());
    }
}
