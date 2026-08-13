//! iMessage provisioner — implements [`TuiProvisioner`] for in-TUI iMessage setup.
//!
//! macOS only. Checks for Full Disk Access before proceeding.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::IMessageConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::ProvisionerCategory;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const IMESSAGE_NAME: &str = "imessage";
pub const IMESSAGE_DESC: &str = "iMessage — macOS only, requires Full Disk Access for Terminal";

#[derive(Debug, Clone)]
pub struct IMessageProvisioner;

impl IMessageProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for IMessageProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for IMessageProvisioner {
    fn name(&self) -> &'static str {
        IMESSAGE_NAME
    }

    fn description(&self) -> &'static str {
        IMESSAGE_DESC
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

        // macOS check
        if !cfg!(target_os = "macos") {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "iMessage is macOS-only.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted("iMessage is macOS-only.".into()));
        }

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Let's configure iMessage.".into(),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Warn,
                text: "iMessage requires macOS with Full Disk Access for Terminal.".into(),
            },
        )
        .await?;

        send(&events, ProvisionEvent::Message {
            severity: Severity::Info,
            text: "System Settings → Privacy & Security → Full Disk Access → add Terminal (or iTerm).".into(),
        }).await?;

        // Confirm prerequisites
        send(
            &events,
            ProvisionEvent::Choose {
                id: "prereq_confirm".into(),
                label: "Have you granted Full Disk Access?".into(),
                options: vec!["Yes — continue".to_string(), "No — cancel".to_string()],
                multi: false,
            },
        )
        .await?;

        let confirmed = {
            let sel = recv_selection(&mut responses).await?;
            sel.first().copied() == Some(0)
        };

        if !confirmed {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "iMessage setup cancelled — prerequisites not met.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "iMessage setup cancelled — prerequisites not met.".into(),
            ));
        }

        // Resolve the same path `IMessageChannel::listen` opens. The old check
        // looked at `/Users/Library/Messages/chat.db` — a path that exists on
        // no macOS system, because the username between `/Users` and `Library`
        // is missing — so the Full Disk Access check could only ever fail.
        let chat_db = chat_db_path();
        if chat_db.exists() {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Success,
                    text: "chat.db is accessible — Full Disk Access is working.".into(),
                },
            )
            .await?;
        } else {
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Warn,
                    text:
                        "chat.db not found at expected path. Full Disk Access may not be granted."
                            .into(),
                },
            )
            .await?;
        }

        // Allowed contacts
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_contacts".into(),
                label:
                    "Allowed contacts (comma-separated phone numbers or emails, empty = deny all)"
                        .into(),
                default: Some(String::new()),
                secret: false,
            },
        )
        .await?;

        let allowed_contacts: Vec<String> = recv_text(&mut responses)
            .await?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Write config
        config.channels_config.imessage = Some(IMessageConfig { allowed_contacts });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "iMessage configured.".into(),
            },
        )
        .await?;

        Ok(ProvisionOutcome::Configured)
    }
}

/// The Messages database this machine actually has.
///
/// Must stay the path `IMessageChannel::listen` opens. It used to be the
/// literal `/Users/Library/Messages/chat.db` — no username between the two
/// segments — so the Full Disk Access check could only ever report failure.
fn chat_db_path() -> std::path::PathBuf {
    directories::UserDirs::new()
        .map(|u| u.home_dir().join("Library/Messages/chat.db"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe must look where the channel looks. The old literal
    /// `/Users/Library/Messages/chat.db` is missing the username, so it names
    /// no file on any macOS system and the check could only ever fail.
    #[test]
    fn imessage_probe_path_matches_the_channel_path() {
        let probe = chat_db_path();
        let channel_path = directories::UserDirs::new()
            .map(|u| u.home_dir().join("Library/Messages/chat.db"))
            .unwrap_or_default();

        assert_eq!(
            probe, channel_path,
            "the provisioner must resolve the same chat.db the channel opens"
        );
        assert_ne!(
            probe,
            std::path::PathBuf::from("/Users/Library/Messages/chat.db"),
            "that path has no username segment and exists nowhere"
        );
        assert!(
            probe.ends_with("Library/Messages/chat.db"),
            "unexpected shape: {}",
            probe.display()
        );
    }
}
