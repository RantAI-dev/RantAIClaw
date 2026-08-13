//! Linq provisioner — implements [`TuiProvisioner`] for in-TUI Linq setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::LinqConfig;
use crate::config::Config;
use crate::onboard::provision::validate::http::probe_get;
use crate::onboard::provision::validate::verdict;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const LINQ_NAME: &str = "linq";
pub const LINQ_DESC: &str = "Linq Partner API — API token, sender phone, webhook signing secret";

#[derive(Debug, Clone)]
pub struct LinqProvisioner;

impl LinqProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LinqProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for LinqProvisioner {
    fn name(&self) -> &'static str {
        LINQ_NAME
    }

    fn description(&self) -> &'static str {
        LINQ_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Channel
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
                text: "Let's configure Linq.".into(),
            },
        )
        .await?;

        // API token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "api_token".into(),
                label: "Linq Partner API token".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let api_token = recv_text(&mut responses).await?;
        if api_token.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "API token is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted("API token is required.".into()));
        }

        // Validate token
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Validating API token…".into(),
            },
        )
        .await?;

        let probe = probe_get(
            &linq_probe_url(),
            &[("Authorization", &format!("Bearer {}", api_token.trim()))],
        )
        .await;
        if !verdict::resolve(
            &events,
            &mut responses,
            verdict::classify_status(&probe),
            "API token",
        )
        .await?
        .should_persist()
        {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "The API token was not saved — Linq is not configured.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "API token failed validation and was not saved".into(),
            ));
        }

        // Sender phone
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "from_phone".into(),
                label: "Sender phone number (E.164, e.g. +12025551234)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let from_phone = recv_text(&mut responses).await?;
        let from_phone = from_phone.trim().to_string();
        if from_phone.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Sender phone is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "Sender phone is required.".into(),
            ));
        }

        // Optional webhook signing secret
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "signing_secret".into(),
                label: "Webhook signing secret (Enter to skip)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let signing_secret = recv_text(&mut responses).await?;
        let signing_secret = if signing_secret.trim().is_empty() {
            None
        } else {
            Some(signing_secret.trim().to_string())
        };

        // Allowed senders
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_senders".into(),
                label: "Allowed sender handles (comma-separated, empty = deny all, * = allow all)"
                    .into(),
                default: Some("*".into()),
                secret: false,
            },
        )
        .await?;

        let allowed_senders: Vec<String> = recv_text(&mut responses)
            .await?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Write config
        config.channels_config.linq = Some(LinqConfig {
            api_token: api_token.trim().to_string(),
            from_phone,
            signing_secret,
            allowed_senders,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Linq configured.".into(),
            },
        )
        .await?;

        Ok(ProvisionOutcome::Configured)
    }
}

use crate::onboard::provision::ProvisionerCategory;

/// The endpoint the token probe hits, derived from the base the channel itself
/// talks to so the two cannot drift apart. `/phonenumbers` is what
/// `LinqChannel::health_check` already uses: it needs the Partner token and
/// returns 200 only when that token is valid.
fn linq_probe_url() -> String {
    format!("{}/phonenumbers", crate::channels::linq::LINQ_API_BASE)
}

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
    fn provisioner_name_is_linq() {
        assert_eq!(LinqProvisioner::new().name(), "linq");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!LinqProvisioner::new().description().is_empty());
    }

    /// The probe must hit the host the channel talks to. It used to hit
    /// `api.linq.com` while the runtime used `api.linqapp.com`, so setup shipped
    /// the operator's Partner API token to a domain this project does not own —
    /// and warned "Token may be invalid" for a perfectly valid token.
    ///
    /// `scripts/ci/check_provisioner_probe_hosts.sh` enforces this across every
    /// provisioner; this test pins the one that regressed.
    #[test]
    fn linq_probe_host_matches_the_channel_base_host() {
        let probe_url = linq_probe_url();
        assert!(
            probe_url.starts_with(crate::channels::linq::LINQ_API_BASE),
            "probe url {probe_url} is not derived from LINQ_API_BASE"
        );

        let probe = reqwest::Url::parse(&probe_url).expect("probe url parses");
        let base =
            reqwest::Url::parse(crate::channels::linq::LINQ_API_BASE).expect("base url parses");
        assert_eq!(
            probe.host_str(),
            base.host_str(),
            "provisioner probes a different host than the channel"
        );
    }
}
