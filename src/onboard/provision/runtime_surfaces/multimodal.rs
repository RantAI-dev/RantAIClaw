//! Multimodal provisioner — implements [`TuiProvisioner`] for in-TUI vision/multimodal setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::MultimodalConfig;
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const MULTIMODAL_NAME: &str = "multimodal";
pub const MULTIMODAL_DESC: &str =
    "Multimodal — image attachment limits and remote image fetching for vision models";

#[derive(Debug, Clone)]
pub struct MultimodalProvisioner;

impl MultimodalProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MultimodalProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for MultimodalProvisioner {
    fn name(&self) -> &'static str {
        MULTIMODAL_NAME
    }

    fn description(&self) -> &'static str {
        MULTIMODAL_DESC
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
                text: "Multimodal settings control image processing limits.".into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Prompt {
                id: "max_images".into(),
                label: "Max images per request (Enter for default 4)".into(),
                default: Some("4".into()),
                secret: false,
            },
        )
        .await?;

        let max_str = recv_text(&mut responses).await?;
        let max_images: usize = max_str.trim().parse().unwrap_or(4);

        send(
            &events,
            ProvisionEvent::Prompt {
                id: "max_image_size_mb".into(),
                label: "Max image size in MiB (Enter for default 5)".into(),
                default: Some("5".into()),
                secret: false,
            },
        )
        .await?;

        let max_size_str = recv_text(&mut responses).await?;
        let max_image_size_mb: usize = max_size_str.trim().parse().unwrap_or(5);

        send(
            &events,
            ProvisionEvent::Choose {
                id: "allow_remote_fetch".into(),
                label: "Allow fetching remote images via HTTP/HTTPS?".into(),
                options: vec![
                    "No — images only from attachments".to_string(),
                    "Yes — allow remote URLs".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let allow_remote_fetch = sel.first().copied() == Some(1);

        config.multimodal = MultimodalConfig {
            max_images,
            max_image_size_mb,
            allow_remote_fetch,
        };

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!(
                    "Multimodal: max {} images, {} MiB each, remote fetch {}.",
                    max_images,
                    max_image_size_mb,
                    if allow_remote_fetch {
                        "allowed"
                    } else {
                        "disabled"
                    }
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
    fn provisioner_name_is_multimodal() {
        assert_eq!(MultimodalProvisioner::new().name(), "multimodal");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!MultimodalProvisioner::new().description().is_empty());
    }
}
