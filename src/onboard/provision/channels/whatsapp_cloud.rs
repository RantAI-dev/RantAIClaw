//! WhatsApp Cloud (Meta Cloud API) provisioner — implements [`TuiProvisioner`].
//!
//! Distinct from `whatsapp-web` (QR code pairing). This is for the Meta Cloud API
//! webhook-based integration.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::WhatsAppConfig;
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_get;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const WHATSAPP_CLOUD_NAME: &str = "whatsapp-cloud";
pub const WHATSAPP_CLOUD_DESC: &str =
    "WhatsApp Cloud API — access token, phone ID, webhook verify token";

#[derive(Debug, Clone)]
pub struct WhatsAppCloudProvisioner;

impl WhatsAppCloudProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WhatsAppCloudProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for WhatsAppCloudProvisioner {
    fn name(&self) -> &'static str {
        WHATSAPP_CLOUD_NAME
    }

    fn description(&self) -> &'static str {
        WHATSAPP_CLOUD_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Channel
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
                text: "Let's configure WhatsApp Cloud API.".into(),
            },
        )
        .await?;

        // Access token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "access_token".into(),
                label: "Access token (from Meta Business Suite)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let access_token = recv_text(&mut responses).await?;
        if access_token.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Access token is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Phone number ID
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "phone_number_id".into(),
                label: "Phone number ID (from Meta Business API)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let phone_number_id = recv_text(&mut responses).await?;
        if phone_number_id.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Phone number ID is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Validate with probe
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Validating credentials…".into(),
            },
        )
        .await?;

        let graph_url = format!(
            "https://graph.facebook.com/v19.0/{}",
            phone_number_id.trim()
        );
        match probe_get(
            &graph_url,
            &[("Authorization", &format!("Bearer {}", access_token.trim()))],
        )
        .await
        {
            Ok(result) if result.status == 200 => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Success,
                        text: "Credentials validated.".into(),
                    },
                )
                .await?;
            }
            Ok(_) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: "Credentials may be invalid.".into(),
                    },
                )
                .await?;
            }
            Err(e) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: format!("Could not validate: {e}. Continuing…"),
                    },
                )
                .await?;
            }
        }

        // Webhook verify token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "verify_token".into(),
                label: "Webhook verify token (your custom token, Meta will echo it back)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let verify_token = recv_text(&mut responses).await?;

        // Optional app secret
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "app_secret".into(),
                label: "App secret for webhook signature verification (Enter to skip)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let app_secret = recv_text(&mut responses).await?;
        let app_secret = if app_secret.trim().is_empty() {
            None
        } else {
            Some(app_secret.trim().to_string())
        };

        // Allowed numbers
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_numbers".into(),
                label: "Allowed phone numbers (comma-separated E.164, or * for all)".into(),
                default: Some("*".into()),
                secret: false,
            },
        )
        .await?;

        let allowed_raw = recv_text(&mut responses).await?;
        let allowed_numbers: Vec<String> =
            if allowed_raw.trim().is_empty() || allowed_raw.trim() == "*" {
                vec!["*".to_string()]
            } else {
                allowed_raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };

        // Write config (preserve any existing web-mode fields)
        let existing = config.channels_config.whatsapp.clone();
        config.channels_config.whatsapp = Some(WhatsAppConfig {
            access_token: Some(access_token.trim().to_string()),
            phone_number_id: Some(phone_number_id.trim().to_string()),
            verify_token: if verify_token.trim().is_empty() {
                None
            } else {
                Some(verify_token.trim().to_string())
            },
            app_secret,
            session_path: existing.as_ref().and_then(|c| c.session_path.clone()),
            pair_phone: existing.as_ref().and_then(|c| c.pair_phone.clone()),
            pair_code: existing.as_ref().and_then(|c| c.pair_code.clone()),
            allowed_numbers,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "WhatsApp Cloud API configured.".into(),
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
    fn provisioner_name_is_whatsapp_cloud() {
        assert_eq!(WhatsAppCloudProvisioner::new().name(), "whatsapp-cloud");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!WhatsAppCloudProvisioner::new().description().is_empty());
    }
}
