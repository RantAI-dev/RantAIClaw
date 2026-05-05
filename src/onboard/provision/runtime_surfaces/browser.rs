//! Browser provisioner — implements [`TuiProvisioner`] for in-TUI browser automation setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::{BrowserComputerUseConfig, BrowserConfig};
use crate::config::Config;
use crate::onboard::provision::validate::process::validate_command_on_path;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const BROWSER_NAME: &str = "browser";
pub const BROWSER_DESC: &str =
    "Browser automation — Chromium, Agent Browser, or Computer Use (Anthropic)";

#[derive(Debug, Clone)]
pub struct BrowserProvisioner;

impl BrowserProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BrowserProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for BrowserProvisioner {
    fn name(&self) -> &'static str {
        BROWSER_NAME
    }

    fn description(&self) -> &'static str {
        BROWSER_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Integration
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
                text: "Let's configure browser automation.".into(),
            },
        )
        .await?;

        // Backend selection
        send(
            &events,
            ProvisionEvent::Choose {
                id: "backend".into(),
                label: "Browser backend".into(),
                options: vec![
                    "None (disable browser automation)".to_string(),
                    "Agent Browser (headless Chromium)".to_string(),
                    "Computer Use (Anthropic)".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let (enabled, backend) = match sel.first().copied().unwrap_or(0) {
            1 => (true, "agent_browser".to_string()),
            2 => (true, "computer_use".to_string()),
            _ => (false, "agent_browser".to_string()),
        };

        let mut browser_cfg = BrowserConfig {
            enabled,
            allowed_domains: vec![],
            session_name: None,
            backend: backend.clone(),
            native_headless: true,
            native_webdriver_url: "http://127.0.0.1:9515".to_string(),
            native_chrome_path: None,
            computer_use: BrowserComputerUseConfig::default(),
        };

        if enabled && backend == "agent_browser" {
            // Check if chromium is available
            match validate_command_on_path("chromium")
                .or_else(|_| validate_command_on_path("chromium-browser"))
                .or_else(|_| validate_command_on_path("google-chrome"))
            {
                Ok(path) => {
                    send(
                        &events,
                        ProvisionEvent::Message {
                            severity: Severity::Success,
                            text: format!("Found browser at {}", path.display()),
                        },
                    )
                    .await?;
                }
                Err(_) => {
                    send(&events, ProvisionEvent::Message {
                        severity: Severity::Info,
                        text: "No system Chromium detected — browser automation may not work until installed.".into(),
                    }).await?;
                }
            }

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "chrome_path".into(),
                    label: "Chrome/Chromium path (Enter to auto-detect, or type 'none' to skip)"
                        .into(),
                    default: Some("auto-detect".into()),
                    secret: false,
                },
            )
            .await?;

            let path = recv_text(&mut responses).await?;
            browser_cfg.native_chrome_path =
                if path.trim().is_empty() || path.trim() == "auto-detect" {
                    None
                } else {
                    Some(path.trim().to_string())
                };
        }

        if enabled && backend == "computer_use" {
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "viewport_width".into(),
                    label: "Viewport width (Enter for default 1024)".into(),
                    default: Some("1024".into()),
                    secret: false,
                },
            )
            .await?;

            let _w = recv_text(&mut responses).await?;

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "viewport_height".into(),
                    label: "Viewport height (Enter for default 768)".into(),
                    default: Some("768".into()),
                    secret: false,
                },
            )
            .await?;

            let _h = recv_text(&mut responses).await?;

            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "quality".into(),
                    label: "Screenshot quality 1-100 (Enter for default 80)".into(),
                    default: Some("80".into()),
                    secret: false,
                },
            )
            .await?;

            let _q = recv_text(&mut responses).await?;
        }

        config.browser = browser_cfg;

        send(
            &events,
            ProvisionEvent::Done {
                summary: if enabled {
                    format!("Browser configured: {}.", backend)
                } else {
                    "Browser automation disabled.".into()
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
    fn provisioner_name_is_browser() {
        assert_eq!(BrowserProvisioner::new().name(), "browser");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!BrowserProvisioner::new().description().is_empty());
    }
}
