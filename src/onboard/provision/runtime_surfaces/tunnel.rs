//! Tunnel provisioner — implements [`TuiProvisioner`] for in-TUI tunnel/public exposure setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::{
    CloudflareTunnelConfig, CustomTunnelConfig, NgrokTunnelConfig, TunnelConfig,
};
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::validate::process::validate_command_on_path;
use crate::onboard::provision::ProvisionerCategory;
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

        // Seed from the existing tunnel config so an empty answer keeps the stored
        // credential instead of wiping it — only the provider selection and any
        // freshly-entered secret change.
        let mut tunnel_cfg = config.tunnel.clone();
        tunnel_cfg.provider = match provider {
            1 => "cloudflare",
            2 => "tailscale",
            3 => "ngrok",
            4 => "custom",
            _ => "none",
        }
        .to_string();

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
                        .await?;
                    }
                    Err(e) => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Warn,
                                text: format!("cloudflared not found: {e}"),
                            },
                        )
                        .await?;
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
                        .await?;
                    }
                    Err(e) => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Warn,
                                text: format!("tailscale not found: {e}"),
                            },
                        )
                        .await?;
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
                        .await?;
                    }
                    Err(e) => {
                        send(
                            &events,
                            ProvisionEvent::Message {
                                severity: Severity::Warn,
                                text: format!("ngrok not found: {e}"),
                            },
                        )
                        .await?;
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

        // Never persist an impossible state — a credential-requiring provider with
        // no backing config. Fall back to "none" instead. (Tailscale can run on
        // ambient daemon auth, so it is not required to carry a token.)
        let has_backing = match tunnel_cfg.provider.as_str() {
            "cloudflare" => tunnel_cfg.cloudflare.is_some(),
            "ngrok" => tunnel_cfg.ngrok.is_some(),
            "custom" => tunnel_cfg.custom.is_some(),
            _ => true,
        };
        if !has_backing {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Warn,
                    text: format!(
                        "No credential provided for {}; leaving the tunnel disabled.",
                        tunnel_cfg.provider
                    ),
                },
            )
            .await?;
            tunnel_cfg.provider = "none".to_string();
        }

        config.tunnel = tunnel_cfg;

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Tunnel provider set.".to_string(),
            },
        )
        .await?;

        Ok(ProvisionOutcome::Configured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::traits::ProvisionResponse;

    #[tokio::test]
    async fn tunnel_preserves_existing_token_on_empty_answer() {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(32);
        let (resp_tx, resp_rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move { while events_rx.recv().await.is_some() {} });
        resp_tx
            .send(ProvisionResponse::Selection(vec![1]))
            .await
            .unwrap(); // Cloudflare
        resp_tx
            .send(ProvisionResponse::Text(String::new()))
            .await
            .unwrap(); // empty token — keep existing

        let mut config = Config::default();
        config.tunnel.provider = "cloudflare".into();
        config.tunnel.cloudflare = Some(CloudflareTunnelConfig {
            token: "existing-token".into(),
        });
        let profile = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp"),
        };
        TunnelProvisioner::new()
            .run(
                &mut config,
                &profile,
                ProvisionIo {
                    events: events_tx,
                    responses: resp_rx,
                },
            )
            .await
            .unwrap();

        assert_eq!(config.tunnel.provider, "cloudflare");
        assert_eq!(
            config.tunnel.cloudflare.as_ref().map(|c| c.token.as_str()),
            Some("existing-token"),
            "an empty token answer must keep the stored credential, not wipe it"
        );
    }
}
