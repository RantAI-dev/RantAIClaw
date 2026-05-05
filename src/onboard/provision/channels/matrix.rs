//! Matrix provisioner — implements [`TuiProvisioner`] for in-TUI Matrix setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::MatrixConfig;
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_get;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const MATRIX_NAME: &str = "matrix";
pub const MATRIX_DESC: &str = "Matrix — homeserver URL, access token, room ID, allowed users";

#[derive(Debug, Clone)]
pub struct MatrixProvisioner;

impl MatrixProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MatrixProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for MatrixProvisioner {
    fn name(&self) -> &'static str {
        MATRIX_NAME
    }

    fn description(&self) -> &'static str {
        MATRIX_DESC
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
                text: "Let's configure Matrix.".into(),
            },
        )
        .await?;

        // Homeserver URL
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "homeserver".into(),
                label: "Homeserver URL (e.g. https://matrix.org)".into(),
                default: Some("https://matrix.org".into()),
                secret: false,
            },
        )
        .await?;

        let homeserver = recv_text(&mut responses).await?;
        let homeserver = homeserver.trim().trim_end_matches('/').to_string();
        if homeserver.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Homeserver URL is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Access token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "access_token".into(),
                label: "Access token for bot account".into(),
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

        // Validate token
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Validating access token…".into(),
            },
        )
        .await?;

        let whoami_url = format!("{}/_matrix/client/r0/account/whoami", homeserver);
        match probe_get(
            &whoami_url,
            &[("Authorization", &format!("Bearer {}", access_token.trim()))],
        )
        .await
        {
            Ok(result) if result.status == 200 => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Success,
                        text: "Access token validated.".into(),
                    },
                )
                .await?;
            }
            Ok(_) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: "Token may be invalid.".into(),
                    },
                )
                .await?;
            }
            Err(e) => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: format!("Could not validate token: {e}. Continuing…"),
                    },
                )
                .await?;
            }
        }

        // Optional user ID
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "user_id".into(),
                label: "Bot user ID (e.g. @bot:matrix.org, Enter to skip)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let user_id = recv_text(&mut responses).await?;
        let user_id = if user_id.trim().is_empty() {
            None
        } else {
            Some(user_id.trim().to_string())
        };

        // Optional device ID
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "device_id".into(),
                label: "Device ID (Enter to skip)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let device_id = recv_text(&mut responses).await?;
        let device_id = if device_id.trim().is_empty() {
            None
        } else {
            Some(device_id.trim().to_string())
        };

        // Room ID
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "room_id".into(),
                label: "Room ID to listen in (e.g. !abc123:matrix.org)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let room_id = recv_text(&mut responses).await?;
        let room_id = room_id.trim().to_string();
        if room_id.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Room ID is required.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Allowed users
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_users".into(),
                label: "Allowed user IDs (comma-separated, empty = deny all)".into(),
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
        config.channels_config.matrix = Some(MatrixConfig {
            homeserver,
            access_token: access_token.trim().to_string(),
            user_id,
            device_id,
            room_id,
            allowed_users,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Matrix configured.".into(),
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
    fn provisioner_name_is_matrix() {
        assert_eq!(MatrixProvisioner::new().name(), "matrix");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!MatrixProvisioner::new().description().is_empty());
    }
}
