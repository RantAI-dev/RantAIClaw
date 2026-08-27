//! Browser provisioner — implements [`TuiProvisioner`] for in-TUI browser automation setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::{BrowserComputerUseConfig, BrowserConfig};
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::validate::process::validate_command_on_path;
use crate::onboard::provision::ProvisionerCategory;
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

        // Seed from the existing browser config so re-running `setup browser` only
        // changes what it prompts for. Building a fresh struct wiped the curated
        // `allowed_domains` (which `tools::browser` treats as a hard error state
        // when empty on an enabled tool), the session name, and any tuned
        // computer_use settings.
        let mut browser_cfg = config.browser.clone();
        browser_cfg.enabled = enabled;
        browser_cfg.backend = backend.clone();

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

            let width = recv_text(&mut responses).await?;
            if let Ok(v) = width.trim().parse::<i64>() {
                browser_cfg.computer_use.max_coordinate_x = Some(v);
            }

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

            let height = recv_text(&mut responses).await?;
            if let Ok(v) = height.trim().parse::<i64>() {
                browser_cfg.computer_use.max_coordinate_y = Some(v);
            }
            // No screenshot-quality prompt: `BrowserComputerUseConfig` has no field
            // for it, so asking collected a value with nowhere to store it.
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

        Ok(ProvisionOutcome::Configured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::traits::ProvisionResponse;

    #[tokio::test]
    async fn browser_preserves_allowed_domains_and_session() {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(32);
        let (resp_tx, resp_rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move { while events_rx.recv().await.is_some() {} });
        resp_tx
            .send(ProvisionResponse::Selection(vec![0]))
            .await
            .unwrap(); // backend = None (disable)

        let mut config = Config::default();
        config.browser.allowed_domains = vec!["example.com".into()];
        config.browser.session_name = Some("kept".into());
        let profile = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp"),
        };
        BrowserProvisioner::new()
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

        assert!(!config.browser.enabled, "prompted field applied");
        assert_eq!(
            config.browser.allowed_domains,
            vec!["example.com".to_string()],
            "the curated allowlist must survive a setup re-run"
        );
        assert_eq!(config.browser.session_name.as_deref(), Some("kept"));
    }
}
