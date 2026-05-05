//! Tunnel provisioner — implements [`TuiProvisioner`] for in-TUI tunnel/public exposure setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::{
    CloudflareTunnelConfig, CustomTunnelConfig, NgrokTunnelConfig, TunnelConfig,
};
use crate::config::Config;
use crate::onboard::provision::validate::process::validate_command_on_path;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const TUNNEL_NAME: &str = "tunnel";
pub const TUNNEL_DESC: &str =
    "Tunnel — Cloudflare Tunnel, Tailscale Funnel, ngrok, or custom command";

#[derive(Debug, Clone)]
pub struct TunnelProvisioner;

impl TunnelProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TunnelProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for TunnelProvisioner {
    fn name(&self) -> &'static str {
        TUNNEL_NAME
    }

    fn description(&self) -> &'static str {
        TUNNEL_DESC
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
                text: "Let's configure tunnel/public exposure.".into(),
            },
        )
        .await?;

        // Provider selection
        send(
            &events,
            ProvisionEvent::Choose {
                id: "provider".into(),
                label: "Tunnel provider".into(),
                options: vec![
                    "None".to_string(),
                    "Cloudflare Tunnel".to_string(),
                    "Tailscale Funnel".to_string(),
                    "ngrok".to_string(),
                    "Custom command".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let provider = sel.first().copied().unwrap_or(0);

        let mut tunnel_cfg = TunnelConfig {
            provider: match provider {
                1 => "cloudflare",
                2 => "tailscale",
                3 => "ngrok",
                4 => "custom",
                _ => "none",
            }
            .to_string(),
            cloudflare: None,
            tailscale: None,
            ngrok: None,
            custom: None,
        };

        match provider {
            1 => {
                // Cloudflare
                match validate_command_on_path("cloudflared") {
                    Ok(_) => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Success,
                                text: "cloudflared found.".into(),
                            },
                        )
                        .await?
                    }
                    Err(e) => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Warn,
                                text: format!("cloudflared not found: {e}"),
                            },
                        )
                        .await?
                    }
                }

                send(
                    &events,
                    ProvisionEvent::Prompt {
                        id: "token".into(),
                        label: "Cloudflare Tunnel token".into(),
                        default: None,
                        secret: true,
                    },
                )
                .await?;

                let token = recv_text(&mut responses).await?;
                if !token.trim().is_empty() {
                    tunnel_cfg.cloudflare = Some(CloudflareTunnelConfig {
                        token: token.trim().to_string(),
                    });
                }
            }
            2 => {
                // Tailscale
                match validate_command_on_path("tailscale") {
                    Ok(_) => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Success,
                                text: "tailscale found.".into(),
                            },
                        )
                        .await?
                    }
                    Err(e) => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Warn,
                                text: format!("tailscale not found: {e}"),
                            },
                        )
                        .await?
                    }
                }

                send(
                    &events,
                    ProvisionEvent::Choose {
                        id: "funnel".into(),
                        label: "Tailscale mode".into(),
                        options: vec![
                            "Funnel (public internet)".to_string(),
                            "Serve (tailnet only)".to_string(),
                        ],
                        multi: false,
                    },
                )
                .await?;

                let funnel = {
                    let s = recv_selection(&mut responses).await?;
                    s.first().copied() == Some(0)
                };

                send(
                    &events,
                    ProvisionEvent::Prompt {
                        id: "hostname".into(),
                        label: "Optional hostname (Enter to skip)".into(),
                        default: None,
                        secret: false,
                    },
                )
                .await?;

                let hostname = recv_text(&mut responses).await?;
                tunnel_cfg.tailscale = Some(crate::config::schema::TailscaleTunnelConfig {
                    funnel,
                    hostname: if hostname.trim().is_empty() {
                        None
                    } else {
                        Some(hostname.trim().to_string())
                    },
                });
            }
            3 => {
                // ngrok
                match validate_command_on_path("ngrok") {
                    Ok(_) => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Success,
                                text: "ngrok found.".into(),
                            },
                        )
                        .await?
                    }
                    Err(e) => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Warn,
                                text: format!("ngrok not found: {e}"),
                            },
                        )
                        .await?
                    }
                }

                send(
                    &events,
                    ProvisionEvent::Prompt {
                        id: "auth_token".into(),
                        label: "ngrok auth token".into(),
                        default: None,
                        secret: true,
                    },
                )
                .await?;

                let token = recv_text(&mut responses).await?;
                if !token.trim().is_empty() {
                    tunnel_cfg.ngrok = Some(NgrokTunnelConfig {
                        auth_token: token.trim().to_string(),
                        domain: None,
                    });
                }
            }
            4 => {
                // Custom command
                send(
                    &events,
                    ProvisionEvent::Prompt {
                        id: "command".into(),
                        label: "Custom tunnel command template (use {port} placeholder)".into(),
                        default: Some("bore local {port} --to bore.pub".into()),
                        secret: false,
                    },
                )
                .await?;

                let cmd = recv_text(&mut responses).await?;
                if !cmd.trim().is_empty() {
                    tunnel_cfg.custom = Some(CustomTunnelConfig {
                        start_command: cmd.trim().to_string(),
                        health_url: None,
                        url_pattern: None,
                    });
                }
            }
            _ => {}
        }

        config.tunnel = tunnel_cfg;

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!("Tunnel provider set."),
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
    fn provisioner_name_is_tunnel() {
        assert_eq!(TunnelProvisioner::new().name(), "tunnel");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!TunnelProvisioner::new().description().is_empty());
    }
}
