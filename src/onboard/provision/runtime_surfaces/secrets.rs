//! Secrets provisioner — implements [`TuiProvisioner`] for in-TUI secrets encryption setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::SecretsConfig;
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const SECRETS_NAME: &str = "secrets";
pub const SECRETS_DESC: &str = "Secrets encryption — encrypt API keys and tokens in config.toml";

#[derive(Debug, Clone)]
pub struct SecretsProvisioner;

impl SecretsProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SecretsProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for SecretsProvisioner {
    fn name(&self) -> &'static str {
        SECRETS_NAME
    }

    fn description(&self) -> &'static str {
        SECRETS_DESC
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
                text: "Secrets encryption protects API keys and tokens stored in config.toml."
                    .into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Choose {
                id: "encrypt".into(),
                label: "Enable secrets encryption?".into(),
                options: vec![
                    "Yes — encrypt secrets (recommended)".to_string(),
                    "No — store in plain text".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let encrypt = sel.first().copied() != Some(1);

        config.secrets = SecretsConfig { encrypt };

        send(
            &events,
            ProvisionEvent::Done {
                summary: if encrypt {
                    "Secrets encryption enabled.".into()
                } else {
                    "Secrets encryption disabled.".into()
                },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_secrets() {
        assert_eq!(SecretsProvisioner::new().name(), "secrets");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!SecretsProvisioner::new().description().is_empty());
    }
}
