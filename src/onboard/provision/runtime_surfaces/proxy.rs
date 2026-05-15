//! Proxy provisioner — implements [`TuiProvisioner`] for in-TUI proxy setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::{ProxyConfig, ProxyScope};
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const PROXY_NAME: &str = "proxy";
pub const PROXY_DESC: &str = "Proxy — HTTP/HTTPS/SOCKS proxy for providers, channels, MCP, skills";

#[derive(Debug, Clone)]
pub struct ProxyProvisioner;

impl ProxyProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ProxyProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for ProxyProvisioner {
    fn name(&self) -> &'static str {
        PROXY_NAME
    }

    fn description(&self) -> &'static str {
        PROXY_DESC
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
                text: "Let's configure proxy settings.".into(),
            },
        )
        .await?;

        // Enable/disable
        send(
            &events,
            ProvisionEvent::Choose {
                id: "enabled".into(),
                label: "Enable proxy?".into(),
                options: vec![
                    "No".to_string(),
                    "Yes — for all connections".to_string(),
                    "Yes — for selected services".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let (enabled, scope) = match sel.first().copied() {
            Some(1) => (true, ProxyScope::Rantaiclaw),
            Some(2) => (true, ProxyScope::Services),
            _ => (false, ProxyScope::Rantaiclaw),
        };

        let mut http_proxy = None;
        let mut https_proxy = None;
        let all_proxy = None;

        if enabled {
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "http_proxy".into(),
                    label: "HTTP proxy URL (e.g. http://proxy:8080, Enter to skip)".into(),
                    default: None,
                    secret: false,
                },
            )
            .await?;

            let v = recv_text(&mut responses).await?;
            http_proxy = if v.trim().is_empty() {
                None
            } else {
                Some(v.trim().to_string())
            };

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "https_proxy".into(),
                    label: "HTTPS proxy URL (Enter to same as HTTP)".into(),
                    default: None,
                    secret: false,
                },
            )
            .await?;

            let v = recv_text(&mut responses).await?;
            https_proxy = if v.trim().is_empty() {
                http_proxy.clone()
            } else {
                Some(v.trim().to_string())
            };

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "no_proxy".into(),
                    label: "No-proxy bypass list (comma-separated, Enter to skip)".into(),
                    default: None,
                    secret: false,
                },
            )
            .await?;

            let no_proxy_raw = recv_text(&mut responses).await?;
            let no_proxy: Vec<String> = if no_proxy_raw.trim().is_empty() {
                vec![]
            } else {
                no_proxy_raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };

            let services = if scope == ProxyScope::Services {
                send(
                    &events,
                    ProvisionEvent::Choose {
                        id: "services".into(),
                        label: "Which services to proxy?".into(),
                        options: vec![
                            "providers".to_string(),
                            "channels".to_string(),
                            "mcp".to_string(),
                            "skills".to_string(),
                        ],
                        multi: true,
                    },
                )
                .await?;

                let sel = recv_selection_multi(&mut responses).await?;
                let labels = ["providers", "channels", "mcp", "skills"];
                sel.iter()
                    .filter_map(|&i| labels.get(i).map(|s| s.to_string()))
                    .collect()
            } else {
                vec![]
            };

            config.proxy = ProxyConfig {
                enabled,
                http_proxy,
                https_proxy,
                all_proxy,
                no_proxy,
                scope,
                services,
            };
        } else {
            config.proxy = ProxyConfig::default();
        }

        send(
            &events,
            ProvisionEvent::Done {
                summary: if enabled {
                    "Proxy configured.".into()
                } else {
                    "Proxy disabled.".into()
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

async fn recv_selection_multi(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<Vec<usize>> {
    recv_selection(responses).await
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
    fn provisioner_name_is_proxy() {
        assert_eq!(ProxyProvisioner::new().name(), "proxy");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!ProxyProvisioner::new().description().is_empty());
    }
}
