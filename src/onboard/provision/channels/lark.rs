//! Lark provisioner — implements [`TuiProvisioner`] for in-TUI Lark/Feishu setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::{LarkConfig, LarkReceiveMode};
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::validate::http::probe_post;
use crate::onboard::provision::validate::numeric;
use crate::onboard::provision::validate::verdict;
use crate::onboard::provision::ProvisionerCategory;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const LARK_NAME: &str = "lark";
pub const LARK_DESC: &str =
    "Lark/Feishu — app ID, app secret, encrypt key, websocket or webhook mode";

#[derive(Debug, Clone)]
pub struct LarkProvisioner;

impl LarkProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LarkProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for LarkProvisioner {
    fn name(&self) -> &'static str {
        LARK_NAME
    }

    fn description(&self) -> &'static str {
        LARK_DESC
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
                text: "Let's configure Lark/Feishu.".into(),
            },
        )
        .await?;

        // App ID
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "app_id".into(),
                label: "App ID (from Lark/Feishu developer console)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let app_id = recv_text(&mut responses).await?;
        if app_id.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "App ID is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted("App ID is required.".into()));
        }

        // App secret
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "app_secret".into(),
                label: "App Secret".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let app_secret = recv_text(&mut responses).await?;
        if app_secret.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "App Secret is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted("App Secret is required.".into()));
        }

        // Region. Asked before the probe because it decides which host the app
        // credentials are sent to: Feishu and Lark are separate deployments and
        // a credential from one is not valid on the other. The branch that
        // picked the host was hardcoded to never take the Feishu arm, and
        // `use_feishu` was written as a literal false, so a Feishu tenant could
        // not be configured from the TUI at all — the CLI wizard asked properly.
        send(
            &events,
            ProvisionEvent::Choose {
                id: "region".into(),
                label: "Region".into(),
                options: vec![
                    "Feishu (CN)".to_string(),
                    "Lark (International)".to_string(),
                ],
                multi: false,
            },
        )
        .await?;
        let use_feishu = recv_selection(&mut responses)
            .await?
            .first()
            .copied()
            .unwrap_or(0)
            == 0;

        // Validate by getting tenant access token
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Validating credentials…".into(),
            },
        )
        .await?;

        let token_url = tenant_token_url(use_feishu);

        let body = serde_json::json!({
            "app_id": app_id.trim(),
            "app_secret": app_secret.trim()
        });

        let probe = probe_post(
            token_url,
            &[],
            &serde_json::to_string(&body).unwrap_or_default(),
        )
        .await;
        // Lark answers 200 with a non-zero `code` when it rejects the app
        // credentials, so the status alone proves nothing. A non-zero `code`
        // is the platform saying no; anything else unrecognised is not.
        let verdict = match &probe {
            Ok(r)
                if r.body.contains("\"code\":0") || r.body.contains("\"tenant_access_token\"") =>
            {
                verdict::ProbeVerdict::Accepted
            }
            Ok(r) if r.body.contains("\"code\":") => {
                verdict::ProbeVerdict::Rejected("the app credentials were refused".into())
            }
            Ok(_) => verdict::ProbeVerdict::Inconclusive("unrecognised response".into()),
            Err(e) => verdict::ProbeVerdict::Inconclusive(format!("{e}")),
        };
        if !verdict::resolve(&events, &mut responses, verdict, "app credentials")
            .await?
            .should_persist()
        {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "The app credentials were not saved — Lark is not configured.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "app credentials failed validation and were not saved".into(),
            ));
        }

        // Optional encrypt key
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "encrypt_key".into(),
                label: "Encrypt key for webhook (Enter to skip)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let encrypt_key = recv_text(&mut responses).await?;
        let encrypt_key = if encrypt_key.trim().is_empty() {
            None
        } else {
            Some(encrypt_key.trim().to_string())
        };

        // Optional verification token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "verification_token".into(),
                label: "Verification token for webhook (Enter to skip)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let verification_token = recv_text(&mut responses).await?;
        let verification_token = if verification_token.trim().is_empty() {
            None
        } else {
            Some(verification_token.trim().to_string())
        };

        // Receive mode
        send(
            &events,
            ProvisionEvent::Choose {
                id: "receive_mode".into(),
                label: "Event receive mode".into(),
                options: vec![
                    "WebSocket (persistent, recommended)".to_string(),
                    "Webhook (requires public HTTPS URL)".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let receive_mode = {
            let sel = recv_selection(&mut responses).await?;
            match sel.first().copied() {
                Some(1) => LarkReceiveMode::Webhook,
                _ => LarkReceiveMode::Websocket,
            }
        };

        // Port for webhook mode. An unparseable answer used to yield `None`,
        // which left webhook mode configured with no port — a failure deferred
        // from setup, where it can be corrected, to runtime, where it cannot.
        let port = if receive_mode == LarkReceiveMode::Webhook {
            Some(
                numeric::prompt_number(
                    &events,
                    &mut responses,
                    "port",
                    "HTTP port for webhook (e.g. 8080)",
                    8080u16,
                )
                .await?,
            )
        } else {
            None
        };

        // Allowed users
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_users".into(),
                label: "Allowed user IDs (comma-separated, empty = deny all, * = allow all)".into(),
                default: Some(String::new()),
                secret: false,
            },
        )
        .await?;

        let allowed_users: Vec<String> = recv_text(&mut responses)
            .await?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Write config
        config.channels_config.lark = Some(LarkConfig {
            app_id: app_id.trim().to_string(),
            app_secret: app_secret.trim().to_string(),
            encrypt_key,
            verification_token,
            allowed_users,
            use_feishu,
            receive_mode,
            port,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Lark/Feishu configured.".into(),
            },
        )
        .await?;

        Ok(ProvisionOutcome::Configured)
    }
}

/// The tenant-token endpoint for the selected region.
///
/// Feishu and Lark are separate deployments; a credential issued by one is not
/// valid on the other, so probing the wrong one can only ever fail.
fn tenant_token_url(use_feishu: bool) -> &'static str {
    if use_feishu {
        "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal"
    } else {
        "https://open.larksuite.com/open-apis/auth/v3/tenant_access_token/internal"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_lark() {
        assert_eq!(LarkProvisioner::new().name(), "lark");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!LarkProvisioner::new().description().is_empty());
    }

    /// The region selection used to be a branch that could not take its Feishu
    /// arm, so a Feishu tenant's credentials were always sent to the Lark
    /// International host — where they can never be valid.
    ///
    /// Deliberately a pure check: driving the provisioner through this point
    /// would make a real request to Lark or Feishu, which is neither
    /// deterministic nor available on a CI runner without egress.
    #[test]
    fn lark_feishu_selection_picks_the_feishu_probe_host() {
        let feishu = reqwest::Url::parse(tenant_token_url(true)).expect("feishu url parses");
        let intl = reqwest::Url::parse(tenant_token_url(false)).expect("lark url parses");

        assert_eq!(feishu.host_str(), Some("open.feishu.cn"));
        assert_eq!(intl.host_str(), Some("open.larksuite.com"));
        assert_ne!(
            feishu.host_str(),
            intl.host_str(),
            "the two regions must not collapse onto one host"
        );
    }
}
