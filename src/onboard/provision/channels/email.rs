//! Email provisioner — implements [`TuiProvisioner`] for in-TUI Email setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::channels::email_channel::EmailConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_text, send};
use crate::onboard::provision::validate::allowlist;
use crate::onboard::provision::validate::numeric;
use crate::onboard::provision::ProvisionerCategory;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const EMAIL_NAME: &str = "email";
pub const EMAIL_DESC: &str = "Email — IMAP/SMTP server, credentials, from address, IDLE timeout";

#[derive(Debug, Clone)]
pub struct EmailProvisioner;

impl EmailProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EmailProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for EmailProvisioner {
    fn name(&self) -> &'static str {
        EMAIL_NAME
    }

    fn description(&self) -> &'static str {
        EMAIL_DESC
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
                text: "Let's configure Email.".into(),
            },
        )
        .await?;

        // IMAP host
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "imap_host".into(),
                label: "IMAP host (e.g. imap.gmail.com)".into(),
                default: Some("imap.gmail.com".into()),
                secret: false,
            },
        )
        .await?;

        let imap_host = recv_text(&mut responses).await?;
        let imap_host = imap_host.trim().to_string();
        if imap_host.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "IMAP host is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted("IMAP host is required.".into()));
        }

        // IMAP port
        let imap_port: u16 = numeric::prompt_number(
            &events,
            &mut responses,
            "imap_port",
            "IMAP port (Enter for default 993)",
            993u16,
        )
        .await?;

        // IMAP folder
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "imap_folder".into(),
                label: "IMAP folder to poll (Enter for default INBOX)".into(),
                default: Some("INBOX".into()),
                secret: false,
            },
        )
        .await?;

        let imap_folder = recv_text(&mut responses).await?;
        let imap_folder = if imap_folder.trim().is_empty() {
            "INBOX".to_string()
        } else {
            imap_folder.trim().to_string()
        };

        // SMTP host
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "smtp_host".into(),
                label: "SMTP host (e.g. smtp.gmail.com)".into(),
                default: Some("smtp.gmail.com".into()),
                secret: false,
            },
        )
        .await?;

        let smtp_host = recv_text(&mut responses).await?;
        let smtp_host = smtp_host.trim().to_string();
        if smtp_host.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "SMTP host is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted("SMTP host is required.".into()));
        }

        // SMTP port
        let smtp_port: u16 = numeric::prompt_number(
            &events,
            &mut responses,
            "smtp_port",
            "SMTP port (Enter for default 587)",
            587u16,
        )
        .await?;

        // From address
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "from_address".into(),
                label: "From address for outgoing emails".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let from_address = recv_text(&mut responses).await?;
        let from_address = from_address.trim().to_string();
        if from_address.is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "From address is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "From address is required.".into(),
            ));
        }

        // Username
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "username".into(),
                label: "Email username (usually same as from address)".into(),
                default: Some(from_address.clone()),
                secret: false,
            },
        )
        .await?;

        let username = recv_text(&mut responses).await?;
        let username = if username.trim().is_empty() {
            from_address.clone()
        } else {
            username.trim().to_string()
        };

        // Password
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "password".into(),
                label: "Email password or app password".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let password = recv_text(&mut responses).await?;
        if password.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Password is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted("Password is required.".into()));
        }

        // Allowed senders
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_senders".into(),
                label:
                    "Allowed sender addresses (comma-separated, empty = deny all, * = allow all)"
                        .into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let allowed_raw = recv_text(&mut responses).await?;
        // An empty answer means empty. It used to mean `*` — allow anyone —
        // under a label that says "empty = deny all". A typed `*` still yields
        // `["*"]`, so the explicit branch that did it was never needed.
        let allowed_senders: Vec<String> = allowed_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        allowlist::warn_on_reach(&events, &allowed_senders, "Allowed sender addresses").await?;

        // IDLE timeout
        let idle_timeout_secs: u64 = numeric::prompt_number(
            &events,
            &mut responses,
            "idle_timeout",
            "IDLE timeout in seconds (Enter for default 1740 = 29 min)",
            1740u64,
        )
        .await?;

        // Write config
        config.channels_config.email = Some(EmailConfig {
            imap_host,
            imap_port,
            imap_folder,
            smtp_host,
            smtp_port,
            smtp_tls: true,
            username,
            password: password.trim().to_string(),
            from_address,
            idle_timeout_secs,
            allowed_senders,
            // Left off by default: a relay that strips Authentication-Results
            // would otherwise silence a working mailbox on first run. Mail
            // claiming to be from an approval owner is refused when
            // unauthenticated regardless of this flag.
            require_authenticated_sender: false,
            // Owner recognition over email stays off until the operator names
            // the authserv-id their own mail server writes; a sender can put an
            // `Authentication-Results` header in the message too, and without a
            // trusted verifier the two cannot be told apart.
            trusted_authserv_id: None,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Email configured.".into(),
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

    /// An empty answer used to be mapped to `vec!["*"]` — allow every sender —
    /// under a prompt whose own label reads "empty = deny all".
    #[tokio::test]
    async fn empty_allowlist_answer_yields_an_empty_list() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &EmailProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Text("imap.example.com"),
                Answer::Text(""), // imap port -> default
                Answer::Text(""), // folder -> INBOX
                Answer::Text("smtp.example.com"),
                Answer::Text(""), // smtp port -> default
                Answer::Text("bot@example.com"),
                Answer::Text(""), // username -> from address
                Answer::Text("placeholder-app-password"),
                Answer::Text(""), // allowed senders -> EMPTY
                Answer::Text(""), // idle timeout -> default
            ],
        )
        .await;

        assert!(
            t.configured(),
            "expected a configured run, got {:?}",
            t.outcome
        );
        let email = config
            .channels_config
            .email
            .as_ref()
            .expect("email config written");
        assert!(
            email.allowed_senders.is_empty(),
            "an empty answer must stay empty, got {:?}",
            email.allowed_senders
        );
        assert!(
            t.messages().iter().any(|m| m.contains("EVERY sender")),
            "the operator must be told the channel now ignores everyone: {:?}",
            t.messages()
        );
    }

    /// The other end of the same prompt: `*` still works, and still warns.
    #[tokio::test]
    async fn wildcard_allowlist_warns() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &EmailProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Text("imap.example.com"),
                Answer::Text(""),
                Answer::Text(""),
                Answer::Text("smtp.example.com"),
                Answer::Text(""),
                Answer::Text("bot@example.com"),
                Answer::Text(""),
                Answer::Text("placeholder-app-password"),
                Answer::Text("*"),
                Answer::Text(""),
            ],
        )
        .await;

        assert!(t.configured());
        assert_eq!(
            config
                .channels_config
                .email
                .as_ref()
                .expect("email config written")
                .allowed_senders,
            vec!["*".to_string()],
            "a typed `*` must still be honoured"
        );
        assert!(
            t.messages().iter().any(|m| m.contains("ANYONE")),
            "a wildcard must warn: {:?}",
            t.messages()
        );
    }
}
