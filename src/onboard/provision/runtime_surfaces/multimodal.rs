//! Multimodal provisioner — implements [`TuiProvisioner`] for in-TUI vision/multimodal setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::MultimodalConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::ProvisionerCategory;
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
            ..Default::default()
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

        Ok(ProvisionOutcome::Configured)
    }
}
