//! Console login provisioner — implements [`TuiProvisioner`] for in-TUI setup of
//! the optional single-operator login (`config.gateway.login`) that gates the
//! web console (claw-ui) and the TUI.
//!
//! Steps:
//!   1. Enable / disable (skip)
//!   2. Username
//!   3. Password + confirmation (argon2-hashed)
//!   4. Idle auto-lock window (see [`IDLE_PRESETS`]; defaults to off)
//!
//! Mirrors [`super::knowledge`]. The provisioner only mutates
//! `config.gateway.login.*`; the driver persists the config afterward. This is
//! the TUI counterpart of the dialoguer `LoginSection`
//! (`crate::onboard::section::login`), so `rantaiclaw setup login` works in the
//! interactive terminal path too.

use super::traits::{ProvisionEvent, ProvisionIo, Severity, TuiProvisioner};
use crate::config::Config;
use crate::profile::Profile;
use crate::security::login::IDLE_PRESETS;
use anyhow::Result;
use async_trait::async_trait;

pub const LOGIN_NAME: &str = "login";
pub const LOGIN_DESC: &str = "Console login — username + password gate for the web console & TUI";

#[derive(Debug, Clone)]
pub struct LoginProvisioner;

impl LoginProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LoginProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for LoginProvisioner {
    fn name(&self) -> &'static str {
        LOGIN_NAME
    }

    fn description(&self) -> &'static str {
        LOGIN_DESC
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
                text: "Let's set up console login (web console + TUI).".into(),
            },
        )
        .await?;

        // Step 1 — enable / disable
        send(
            &events,
            ProvisionEvent::Choose {
                id: "enable".into(),
                label: "Enable console login (username + password)?".into(),
                options: vec!["Enable".into(), "Skip / disable".into()],
                multi: false,
            },
        )
        .await?;
        let selection = recv_selection(&mut responses).await?;
        if selection.first().copied().unwrap_or(0) == 1 {
            // Disable: clear any stored credential so the gate turns off, and
            // drop the auto-lock window with it — it is meaningless with no
            // credential to unlock.
            config.gateway.login.username = None;
            config.gateway.login.password_hash = None;
            config.gateway.login.idle_timeout_secs = 0;
            send(
                &events,
                ProvisionEvent::Done {
                    summary: "Console login left disabled.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Step 2 — username
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "username".into(),
                label: "Console username".into(),
                default: config.gateway.login.username.clone(),
                secret: false,
            },
        )
        .await?;
        let username = recv_text(&mut responses).await?.trim().to_string();
        if username.is_empty() {
            return leave_disabled(&events, "Empty username — console login left disabled.").await;
        }

        // Step 3 — password + confirmation
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "password".into(),
                label: "Console password".into(),
                default: None,
                secret: true,
            },
        )
        .await?;
        let password = recv_text(&mut responses).await?;
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "password_confirm".into(),
                label: "Confirm password".into(),
                default: None,
                secret: true,
            },
        )
        .await?;
        let confirm = recv_text(&mut responses).await?;
        if password.trim().is_empty() || password != confirm {
            return leave_disabled(
                &events,
                "Passwords were empty or did not match — console login left disabled.",
            )
            .await;
        }

        // Step 4 — idle auto-lock window. Offered here rather than defaulted on,
        // so operators opt in knowingly; the shortest offer is 15 minutes
        // because a single long turn generates no input of its own and would
        // otherwise lock mid-answer.
        send(
            &events,
            ProvisionEvent::Choose {
                id: "idle_timeout".into(),
                label: "Lock automatically after a stretch of inactivity?".into(),
                options: IDLE_PRESETS.iter().map(|(l, _)| (*l).to_string()).collect(),
                multi: false,
            },
        )
        .await?;
        let choice = recv_selection(&mut responses).await?;
        let idle_secs = IDLE_PRESETS
            .get(choice.first().copied().unwrap_or(0))
            .map_or(0, |(_, secs)| *secs);

        config.gateway.login.username = Some(username);
        config.gateway.login.password_hash =
            Some(crate::security::login::hash_password(&password)?);
        config.gateway.login.idle_timeout_secs = idle_secs;
        let lock_note = if idle_secs == 0 {
            "no auto-lock".to_string()
        } else {
            format!("auto-lock after {} min idle", idle_secs / 60)
        };
        send(
            &events,
            ProvisionEvent::Done {
                summary: format!(
                    "Console login configured ({lock_note}); \
                     requires a claw-ui build with the login page."
                ),
            },
        )
        .await?;
        Ok(())
    }
}

/// Emit a warning + terminal `Done` and return, leaving login disabled.
async fn leave_disabled(
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    text: &str,
) -> Result<()> {
    send(
        events,
        ProvisionEvent::Message {
            severity: Severity::Info,
            text: text.into(),
        },
    )
    .await?;
    send(
        events,
        ProvisionEvent::Done {
            summary: "Console login left disabled.".into(),
        },
    )
    .await?;
    Ok(())
}

async fn send(
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    ev: ProvisionEvent,
) -> Result<()> {
    events
        .send(ev)
        .await
        .map_err(|e| anyhow::anyhow!("failed to send provision event: {e}"))
}

async fn recv_selection(
    responses: &mut tokio::sync::mpsc::Receiver<super::traits::ProvisionResponse>,
) -> Result<Vec<usize>> {
    match responses.recv().await {
        Some(super::traits::ProvisionResponse::Selection(indices)) => Ok(indices),
        Some(super::traits::ProvisionResponse::Cancelled) => {
            anyhow::bail!("login setup cancelled")
        }
        Some(_) => anyhow::bail!("unexpected response type"),
        None => anyhow::bail!("response channel closed unexpectedly"),
    }
}

async fn recv_text(
    responses: &mut tokio::sync::mpsc::Receiver<super::traits::ProvisionResponse>,
) -> Result<String> {
    match responses.recv().await {
        Some(super::traits::ProvisionResponse::Text(t)) => Ok(t),
        Some(super::traits::ProvisionResponse::Cancelled) => {
            anyhow::bail!("login setup cancelled")
        }
        Some(_) => anyhow::bail!("unexpected response type"),
        None => anyhow::bail!("response channel closed unexpectedly"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_login() {
        assert_eq!(LoginProvisioner::new().name(), "login");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!LoginProvisioner::new().description().is_empty());
    }
}
