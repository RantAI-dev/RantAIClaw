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

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner};
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
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
                filterable: false,
            },
        )
        .await?;
        let selection = recv_selection(&mut responses).await?;
        // An empty answer picks no row, so it decides nothing. Console login is
        // the exposure boundary in front of the console and this TUI, and the
        // operator never answered about it: clearing a stored credential on an
        // unanswered prompt is not this step's call. Abort as the sibling
        // single-select write sites do, which also leaves the config
        // unwritten (an `Aborted` outcome is not persisted).
        let Some(&answer) = selection.first() else {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Console login was not answered, so nothing was changed.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "console login was not answered, so nothing was changed".to_string(),
            ));
        };
        // Only an explicit "Enable" (row 0) continues; the explicit
        // "Skip / disable" row turns the gate off.
        if answer != 0 {
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
            return Ok(ProvisionOutcome::Configured);
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
            return leave_disabled(
                &events,
                &mut config.gateway.login,
                "Empty username — console login left disabled.",
            )
            .await;
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
                &mut config.gateway.login,
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
                filterable: false,
            },
        )
        .await?;
        let choice = recv_selection(&mut responses).await?;
        let Some(&choice_idx) = choice.first() else {
            return Ok(ProvisionOutcome::Aborted(
                "no idle timeout was selected".to_string(),
            ));
        };
        let idle_secs = IDLE_PRESETS.get(choice_idx).map_or(0, |(_, secs)| *secs);

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
        Ok(ProvisionOutcome::Configured)
    }
}

/// Emit a warning + terminal `Done` and return, leaving login disabled.
/// Leaving console login disabled is a completed outcome, not an abort: the
/// caller has already written the disabled state onto the config, and that
/// write has to reach disk.
async fn leave_disabled(
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    login: &mut crate::config::GatewayLoginConfig,
    text: &str,
) -> Result<ProvisionOutcome> {
    // Actually turn the gate off. Previously this only printed "left disabled"
    // while an existing username/password_hash stayed in config — so the old
    // password gate remained armed despite the message. Mirror the explicit
    // "Skip / disable" branch.
    login.username = None;
    login.password_hash = None;
    login.idle_timeout_secs = 0;
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
    Ok(ProvisionOutcome::Configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty answer at "Enable console login?" answers nothing, so this
    /// step must not decide: the console login is an exposure boundary, and an
    /// operator who never picked a row has not asked for it to be turned off.
    /// The provisioner aborts without asking for credentials, and the stored
    /// credential is exactly as it was.
    #[tokio::test]
    async fn empty_enable_answer_aborts_and_leaves_console_login_as_it_was() {
        use crate::onboard::provision::test_support::{drive, scratch_profile, Answer};
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = crate::config::Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        config.gateway.login.username = Some("stale-op".into());
        config.gateway.login.password_hash = Some("stale-hash".into());
        config.gateway.login.idle_timeout_secs = 900;

        let t = drive(
            &LoginProvisioner,
            &mut config,
            &profile,
            vec![Answer::Empty],
        )
        .await;

        let asked_for_credentials = t.events.iter().any(|e| {
            matches!(e, ProvisionEvent::Prompt { id, .. } if id == "username" || id == "password")
        });
        assert!(
            !asked_for_credentials,
            "an empty enable answer must not reach the credential prompts"
        );
        assert!(
            t.aborted(),
            "an unanswered gate is an abort, not a decision: {:?}",
            t.outcome
        );
        assert_eq!(
            config.gateway.login.username.as_deref(),
            Some("stale-op"),
            "the stored operator is not cleared"
        );
        assert_eq!(
            config.gateway.login.password_hash.as_deref(),
            Some("stale-hash"),
            "the stored password hash is not cleared"
        );
        assert_eq!(config.gateway.login.idle_timeout_secs, 900);
    }

    /// An explicit "Skip / disable" still turns the gate off: it is an answer.
    #[tokio::test]
    async fn an_explicit_skip_answer_leaves_console_login_disabled() {
        use crate::onboard::provision::test_support::{drive, scratch_profile, Answer};
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = crate::config::Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        config.gateway.login.username = Some("stale-op".into());
        config.gateway.login.password_hash = Some("stale-hash".into());
        config.gateway.login.idle_timeout_secs = 900;

        let t = drive(
            &LoginProvisioner,
            &mut config,
            &profile,
            vec![Answer::Pick(1)],
        )
        .await;

        assert!(
            t.configured(),
            "the disable branch completes: {:?}",
            t.outcome
        );
        assert_eq!(config.gateway.login.username, None, "gate stays off");
        assert_eq!(config.gateway.login.password_hash, None, "gate stays off");
        assert_eq!(config.gateway.login.idle_timeout_secs, 0);
    }

    #[tokio::test]
    async fn leave_disabled_clears_the_login_gate() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        // Drain events so the bounded channel never blocks the sender.
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let mut login = crate::config::GatewayLoginConfig::default();
        login.username = Some("op".into());
        login.password_hash = Some("argon2-hash".into());
        login.idle_timeout_secs = 900;

        let outcome = leave_disabled(&tx, &mut login, "test").await.unwrap();
        drop(tx);
        drain.await.unwrap();

        assert!(matches!(outcome, ProvisionOutcome::Configured));
        // The message says "left disabled" — the state must actually be off.
        assert!(login.username.is_none(), "username must be cleared");
        assert!(
            login.password_hash.is_none(),
            "password_hash must be cleared"
        );
        assert_eq!(login.idle_timeout_secs, 0, "idle window must be cleared");
    }
}
