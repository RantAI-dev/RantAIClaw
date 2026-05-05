//! Lark provisioner — implements [`TuiProvisioner`] for in-TUI Lark/Feishu setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::{LarkConfig, LarkReceiveMode};
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_post;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const LARK_NAME: &str = "lark";
pub const LARK_DESC: &str =
    "Lark/Feishu — app ID, app secret, encrypt key, websocket or webhook mode";

#[derive(Debug, Clone)]
pub struct LarkProvisioner;

impl LarkProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LarkProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for LarkProvisioner {
    fn name(&self) -> &'static str {
        LARK_NAME
    }

    fn description(&self) -> &'static str {
        LARK_DESC
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
                text: "Let's configure Lark/Feishu.".into(),
            },
        )
        .await?;

        // App ID
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "app_id".into(),
                label: "App ID (from Lark/Feishu developer console)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let app_id = recv_text(&mut responses).await?;
        if app_id.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "App ID is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // App secret
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "app_secret".into(),
                label: "App Secret".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let app_secret = recv_text(&mut responses).await?;
        if app_secret.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "App Secret is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Validate by getting tenant access token
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Validating credentials…".into(),
            },
        )
        .await?;

        let token_url = if false {
            "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal"
        } else {
            "https://open.larksuite.com/open-apis/auth/v3/tenant_access_token/internal"
        };

        let body = serde_json::json!({
            "app_id": app_id.trim(),
            "app_secret": app_secret.trim()
        });

        match probe_post(
            token_url,
            &[],
            &serde_json::to_string(&body).unwrap_or_default(),
        )
        .await
        {
            Ok(result)
                if result.body.contains("\"code\":0")
                    || result.body.contains("\"tenant_access_token\"") =>
            {
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

        // Optional encrypt key
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "encrypt_key".into(),
                label: "Encrypt key for webhook (Enter to skip)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let encrypt_key = recv_text(&mut responses).await?;
        let encrypt_key = if encrypt_key.trim().is_empty() {
            None
        } else {
            Some(encrypt_key.trim().to_string())
        };

        // Optional verification token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "verification_token".into(),
                label: "Verification token for webhook (Enter to skip)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let verification_token = recv_text(&mut responses).await?;
        let verification_token = if verification_token.trim().is_empty() {
            None
        } else {
            Some(verification_token.trim().to_string())
        };

        // Receive mode
        send(
            &events,
            ProvisionEvent::Choose {
                id: "receive_mode".into(),
                label: "Event receive mode".into(),
                options: vec![
                    "WebSocket (persistent, recommended)".to_string(),
                    "Webhook (requires public HTTPS URL)".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let receive_mode = {
            let sel = recv_selection(&mut responses).await?;
            match sel.first().copied() {
                Some(1) => LarkReceiveMode::Webhook,
                _ => LarkReceiveMode::Websocket,
            }
        };

        // Port for webhook mode
        let port = if receive_mode == LarkReceiveMode::Webhook {
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "port".into(),
                    label: "HTTP port for webhook (e.g. 8080)".into(),
                    default: Some("8080".into()),
                    secret: false,
                },
            )
            .await?;
            let p = recv_text(&mut responses).await?;
            let parsed = p.trim().parse::<u16>().ok();
            parsed
        } else {
            None
        };

        // Allowed users
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_users".into(),
                label: "Allowed user IDs (comma-separated, empty = deny all, * = allow all)".into(),
                default: Some(String::new()),
                secret: false,
            },
        )
        .await?;

        let allowed_users: Vec<String> = recv_text(&mut responses)
            .await?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Write config
        config.channels_config.lark = Some(LarkConfig {
            app_id: app_id.trim().to_string(),
            app_secret: app_secret.trim().to_string(),
            encrypt_key,
            verification_token,
            allowed_users,
            use_feishu: false,
            receive_mode,
            port,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Lark/Feishu configured.".into(),
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
    fn provisioner_name_is_lark() {
        assert_eq!(LarkProvisioner::new().name(), "lark");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!LarkProvisioner::new().description().is_empty());
    }
}
