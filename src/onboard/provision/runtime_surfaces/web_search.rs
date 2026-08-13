//! Web Search provisioner — implements [`TuiProvisioner`] for in-TUI web search setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::WebSearchConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::ProvisionerCategory;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const WEB_SEARCH_NAME: &str = "web-search";
pub const WEB_SEARCH_DESC: &str =
    "Web search — DuckDuckGo (no key), Tavily, Serper, Brave, or disabled";

#[derive(Debug, Clone)]
pub struct WebSearchProvisioner;

impl WebSearchProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WebSearchProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for WebSearchProvisioner {
    fn name(&self) -> &'static str {
        WEB_SEARCH_NAME
    }

    fn description(&self) -> &'static str {
        WEB_SEARCH_DESC
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
                text: "Let's configure web search.".into(),
            },
        )
        .await?;

        // Provider selection
        send(
            &events,
            ProvisionEvent::Choose {
                id: "provider".into(),
                label: "Web search provider".into(),
                options: vec![
                    "DuckDuckGo (no API key required)".to_string(),
                    "Tavily".to_string(),
                    "Serper".to_string(),
                    "Brave Search".to_string(),
                    "Disabled".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let (provider, provider_key) = match sel.first().copied().unwrap_or(0) {
            0 => ("duckduckgo".to_string(), None),
            1 => ("tavily".to_string(), Some("tavily".to_string())),
            2 => ("serper".to_string(), Some("serper".to_string())),
            3 => ("brave".to_string(), Some("brave".to_string())),
            _ => ("none".to_string(), None),
        };

        let mut api_key: Option<String> = None;

        if provider_key.is_some() {
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "api_key".into(),
                    label: format!("{} API key", provider.to_uppercase()),
                    default: None,
                    secret: true,
                },
            )
            .await?;

            let v = recv_text(&mut responses).await?;
            api_key = if v.trim().is_empty() {
                None
            } else {
                Some(v.trim().to_string())
            };
        }

        send(
            &events,
            ProvisionEvent::Prompt {
                id: "max_results".into(),
                label: "Max results per query (Enter for default 10)".into(),
                default: Some("10".into()),
                secret: false,
            },
        )
        .await?;

        let max_str = recv_text(&mut responses).await?;
        let max_results: usize = max_str.trim().parse().unwrap_or(10);

        config.web_search = WebSearchConfig {
            enabled: true,
            provider: provider.clone(),
            brave_api_key: api_key,
            searxng_url: None,
            max_results,
            timeout_secs: 15,
        };

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!("Web search configured: {}.", provider),
            },
        )
        .await?;

        Ok(ProvisionOutcome::Configured)
    }
}
