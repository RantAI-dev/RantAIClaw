//! Gateway provisioner — implements [`TuiProvisioner`] for in-TUI webhook gateway setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::GatewayConfig;
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const GATEWAY_NAME: &str = "gateway";
pub const GATEWAY_DESC: &str =
    "Webhook gateway — port, host, pairing, rate limits, request timeouts";

#[derive(Debug, Clone)]
pub struct GatewayProvisioner;

impl GatewayProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GatewayProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for GatewayProvisioner {
    fn name(&self) -> &'static str {
        GATEWAY_NAME
    }

    fn description(&self) -> &'static str {
        GATEWAY_DESC
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
                text: "Let's configure the webhook gateway.".into(),
            },
        )
        .await?;

        // Enable/disable
        send(
            &events,
            ProvisionEvent::Choose {
                id: "enabled".into(),
                label: "Enable webhook gateway?".into(),
                options: vec!["No".to_string(), "Yes".to_string()],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let enabled = sel.first().copied() == Some(1);

        if !enabled {
            send(
                &events,
                ProvisionEvent::Done {
                    summary: "Gateway disabled.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Port
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "port".into(),
                label: "Gateway port (Enter for default 3000)".into(),
                default: Some("3000".into()),
                secret: false,
            },
        )
        .await?;

        let port_str = recv_text(&mut responses).await?;
        let port: u16 = port_str.trim().parse().unwrap_or(3000);

        // Host
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "host".into(),
                label: "Gateway host (Enter for default 127.0.0.1, use 0.0.0.0 for public)".into(),
                default: Some("127.0.0.1".into()),
                secret: false,
            },
        )
        .await?;

        let host = recv_text(&mut responses).await?;
        let host = if host.trim().is_empty() {
            "127.0.0.1".to_string()
        } else {
            host.trim().to_string()
        };

        if host == "0.0.0.0" {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Warn,
                    text:
                        "WARNING: Binding to 0.0.0.0 exposes the gateway on all network interfaces."
                            .into(),
                },
            )
            .await?;
        }

        // Require pairing
        send(
            &events,
            ProvisionEvent::Choose {
                id: "require_pairing".into(),
                label: "Require device pairing before accepting requests?".into(),
                options: vec!["Yes (recommended)".to_string(), "No — open".to_string()],
                multi: false,
            },
        )
        .await?;

        let require_pairing = {
            let s = recv_selection(&mut responses).await?;
            s.first().copied() != Some(1)
        };

        // Note: webhook signing secret prompt was removed. The current
        // `GatewayConfig` schema has no `webhook_secret` field — gateway
        // pairing uses `paired_tokens` (managed automatically by the
        // /pair flow). If a user-managed webhook signing secret is added
        // to the schema later, re-introduce the prompt here.

        let host_for_summary = host.clone();

        config.gateway = GatewayConfig {
            port,
            host,
            require_pairing,
            allow_public_bind: false,
            paired_tokens: vec![],
            pair_rate_limit_per_minute: 10,
            webhook_rate_limit_per_minute: 60,
            trust_forwarded_headers: false,
            rate_limit_max_keys: 10000,
            idempotency_ttl_secs: 3600,
            idempotency_max_keys: 10000,
            request_timeout_secs: 300,
        };

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!("Gateway configured: {}:{}", host_for_summary, port),
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
    fn provisioner_name_is_gateway() {
        assert_eq!(GatewayProvisioner::new().name(), "gateway");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!GatewayProvisioner::new().description().is_empty());
    }
}
