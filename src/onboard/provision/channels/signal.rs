//! Signal provisioner — implements [`TuiProvisioner`] for in-TUI Signal setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::SignalConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_text, send};
use crate::onboard::provision::validate::allowlist;
use crate::onboard::provision::validate::http::probe_get;
use crate::onboard::provision::validate::verdict;
use crate::onboard::provision::ProvisionerCategory;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const SIGNAL_NAME: &str = "signal";
pub const SIGNAL_DESC: &str = "Signal messenger — signal-cli daemon socket + account";

#[derive(Debug, Clone)]
pub struct SignalProvisioner;

impl SignalProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SignalProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for SignalProvisioner {
    fn name(&self) -> &'static str {
        SIGNAL_NAME
    }

    fn description(&self) -> &'static str {
        SIGNAL_DESC
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
                text: "Let's configure Signal.".into(),
            },
        )
        .await?;

        // HTTP URL for signal-cli daemon
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "http_url".into(),
                label: "signal-cli HTTP daemon URL".into(),
                default: Some("http://127.0.0.1:8686".into()),
                secret: false,
            },
        )
        .await?;

        let http_url = recv_text(&mut responses).await?;
        let http_url = http_url.trim().to_string();
        if http_url.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "HTTP URL is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted("HTTP URL is required.".into()));
        }

        // Account phone number
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "account".into(),
                label: "Your Signal phone number (E.164, e.g. +12025551234)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let account = recv_text(&mut responses).await?;
        let account = account.trim().to_string();
        if account.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Account phone number is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "Account phone number is required.".into(),
            ));
        }

        // This used to print "Checking signal-cli daemon at …" and check
        // nothing — the module did not even import a probe helper. It now hits
        // the same endpoint `SignalChannel::health_check` uses, so the claim is
        // true. A daemon the operator has not started yet is a transport
        // failure, which is inconclusive and still lets setup finish.
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: format!("Checking signal-cli daemon at {http_url}…"),
            },
        )
        .await?;

        let probe = probe_get(&format!("{http_url}/api/v1/check"), &[]).await;
        if !verdict::resolve(
            &events,
            &mut responses,
            verdict::classify_status(&probe),
            "signal-cli daemon",
        )
        .await?
        .should_persist()
        {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Signal was not configured.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "the signal-cli daemon check failed and nothing was saved".into(),
            ));
        }

        // Allowed senders
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_from".into(),
                label: "Allowed sender numbers (comma-separated E.164, or * for all)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let allowed_raw = recv_text(&mut responses).await?;
        // An empty answer means empty, not "allow anyone". A typed `*` still
        // yields `["*"]` through the same split.
        let allowed_from: Vec<String> = allowed_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        allowlist::warn_on_reach(&events, &allowed_from, "Allowed sender numbers").await?;

        // `group_id` is an inclusion filter: the runtime keeps only messages
        // from the group whose id matches. There is no DM-only predicate for it
        // to express, so the old "Direct messages only" option wrote the
        // literal `"dm"` and the channel then dropped everything that was not
        // from a group actually called `dm` — silencing the bot completely.
        // Until a real DM-only filter exists, the honest value is None.
        let group_id: Option<String> = None;

        // Write config
        config.channels_config.signal = Some(SignalConfig {
            http_url,
            account,
            group_id,
            allowed_from,
            ignore_attachments: false,
            ignore_stories: true,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Signal configured.".into(),
            },
        )
        .await?;

        Ok(ProvisionOutcome::Configured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::test_support::{drive, scratch_profile, Answer};

    #[test]
    fn provisioner_name_is_signal() {
        assert_eq!(SignalProvisioner::new().name(), "signal");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!SignalProvisioner::new().description().is_empty());
    }

    /// Declining the confirmation must leave the config untouched. Before this
    /// plan there was no confirmation at all: a failed probe warned and the
    /// write went ahead regardless, so `config.toml` ended up holding a
    /// credential the platform had already refused.
    #[tokio::test]
    async fn a_declined_probe_does_not_persist_the_credential() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &SignalProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Text("http://127.0.0.1:1"),
                Answer::Text("+15550001111"),
                Answer::Pick(1), // "No — let me correct it"
            ],
        )
        .await;

        assert!(t.aborted(), "expected an abort, got {:?}", t.outcome);
        assert!(
            config.channels_config.signal.is_none(),
            "a declined probe must write nothing"
        );
    }

    /// The old "Direct messages only" option wrote `group_id = Some("dm")`.
    /// `SignalChannel` reads `group_id` as an *inclusion* filter, so it then
    /// dropped every message not from a group literally named `dm` — choosing
    /// the option silenced the bot. There is no DM-only predicate to express.
    ///
    /// Port 1 is reserved and closed, so the daemon check fails instantly
    /// without DNS: an inconclusive verdict, whose headless default is to
    /// carry on. That is the air-gapped path, exercised here for free.
    #[tokio::test]
    async fn signal_dm_option_does_not_write_a_group_id() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &SignalProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Text("http://127.0.0.1:1"),
                Answer::Text("+15550001111"),
                Answer::Pick(0), // daemon unreachable -> "save anyway"
                Answer::Text("+15550002222"),
            ],
        )
        .await;

        assert!(t.configured(), "expected configured, got {:?}", t.outcome);
        let signal = config
            .channels_config
            .signal
            .as_ref()
            .expect("signal config written");
        assert_eq!(
            signal.group_id, None,
            "no answer may produce a group id the runtime would filter on"
        );
        assert!(
            !t.prompts().iter().any(|p| p.contains("Which messages")),
            "the DM-only choice must be gone: {:?}",
            t.prompts()
        );
    }
}
