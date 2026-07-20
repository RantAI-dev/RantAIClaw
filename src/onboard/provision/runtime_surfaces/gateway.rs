//! Gateway provisioner — implements [`TuiProvisioner`] for in-TUI webhook gateway setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
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
                label: "Gateway port (Enter for default 9393)".into(),
                default: Some("9393".into()),
                secret: false,
            },
        )
        .await?;

        let port_str = recv_text(&mut responses).await?;
        let port: u16 = port_str.trim().parse().unwrap_or(9393);

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

        // Assign only what this provisioner actually asked about.
        //
        // It used to replace the whole `GatewayConfig` with a struct literal,
        // which silently discarded every field it does not prompt for:
        //
        //   * `login` — reset to default, wiping the console username, password
        //     hash, and idle timeout. Running `setup gateway` turned the login
        //     gate off without saying so.
        //   * `paired_tokens` — emptied, unpairing every device.
        //   * the rate limits, key caps, and timeouts — reset to hardcoded
        //     numbers, discarding any the operator had tuned.
        //   * `allow_public_bind` — forced to `false`. Combined with choosing
        //     `0.0.0.0` at the host prompt above, that leaves a config the
        //     gateway then refuses to start from.
        config.gateway.port = port;
        config.gateway.host = host;
        config.gateway.require_pairing = require_pairing;

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

    /// Drive the provisioner through its four prompts with scripted answers.
    async fn run_with(config: &mut Config, host: &str) -> Result<()> {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(32);
        let (resp_tx, resp_rx) = tokio::sync::mpsc::channel(32);
        // Drain events so the provisioner's sends never block.
        tokio::spawn(async move { while events_rx.recv().await.is_some() {} });
        resp_tx
            .send(ProvisionResponse::Selection(vec![1]))
            .await
            .unwrap(); // enable = Yes
        resp_tx
            .send(ProvisionResponse::Text("9999".into()))
            .await
            .unwrap(); // port
        resp_tx
            .send(ProvisionResponse::Text(host.into()))
            .await
            .unwrap(); // host
        resp_tx
            .send(ProvisionResponse::Selection(vec![0]))
            .await
            .unwrap(); // pairing = Yes
        let profile = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp"),
        };
        GatewayProvisioner::new()
            .run(
                config,
                &profile,
                ProvisionIo {
                    events: events_tx,
                    responses: resp_rx,
                },
            )
            .await
    }

    #[tokio::test]
    async fn setup_gateway_preserves_everything_it_does_not_prompt_for() {
        // Running `setup gateway` used to replace the whole GatewayConfig,
        // silently turning off console login and unpairing every device.
        let mut config = Config::default();
        config.gateway.login.username = Some("rantaiclaw_operator".into());
        config.gateway.login.password_hash = Some("$argon2id$v=19$m=1,t=1,p=1$a$b".into());
        config.gateway.login.idle_timeout_secs = 900;
        config.gateway.paired_tokens = vec!["hashed-token-a".into(), "hashed-token-b".into()];
        config.gateway.pair_rate_limit_per_minute = 42;
        config.gateway.allow_public_bind = true;

        run_with(&mut config, "127.0.0.1").await.unwrap();

        assert_eq!(config.gateway.port, 9999, "prompted field is applied");
        assert_eq!(
            config.gateway.login.username.as_deref(),
            Some("rantaiclaw_operator"),
            "console login survives"
        );
        assert!(config.gateway.login.password_hash.is_some());
        assert_eq!(config.gateway.login.idle_timeout_secs, 900);
        assert_eq!(config.gateway.paired_tokens.len(), 2, "devices stay paired");
        assert_eq!(
            config.gateway.pair_rate_limit_per_minute, 42,
            "tuned limit kept"
        );
        assert!(config.gateway.allow_public_bind, "public-bind opt-in kept");
    }

    #[tokio::test]
    async fn setup_gateway_applies_the_answers_it_did_prompt_for() {
        let mut config = Config::default();
        run_with(&mut config, "0.0.0.0").await.unwrap();
        assert_eq!(config.gateway.host, "0.0.0.0");
        assert_eq!(config.gateway.port, 9999);
        assert!(config.gateway.require_pairing);
    }
}
