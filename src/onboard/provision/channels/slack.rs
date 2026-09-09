//! Slack provisioner — implements [`TuiProvisioner`] for in-TUI Slack bot setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::SlackConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_text, send};
use crate::onboard::provision::validate::http::probe_post;
use crate::onboard::provision::validate::verdict;
use crate::onboard::provision::ProvisionerCategory;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const SLACK_NAME: &str = "slack";
pub const SLACK_DESC: &str =
    "Slack bot — bot token (xoxb), app-level token (xapp), channel/user restrictions";

#[derive(Debug, Clone)]
pub struct SlackProvisioner;

impl SlackProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SlackProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for SlackProvisioner {
    fn name(&self) -> &'static str {
        SLACK_NAME
    }

    fn description(&self) -> &'static str {
        SLACK_DESC
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
                text: "Let's configure your Slack bot.".into(),
            },
        )
        .await?;

        // Bot token
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "bot_token".into(),
                label: "Bot token (xoxb-...)".into(),
                default: None,
                secret: true,
            },
        )
        .await?;

        let bot_token = recv_text(&mut responses).await?;
        if bot_token.trim().is_empty() {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "Bot token is required.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted("Bot token is required.".into()));
        }

        // App-level token, optional. Asked here rather than after the
        // `auth.test` round-trip below because both tokens come off the same
        // Slack app page, so the operator pastes them together.
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: format!(
                    "Socket Mode is optional. {}",
                    crate::channels::slack::NO_APP_TOKEN_CONSEQUENCE
                ),
            },
        )
        .await?;
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "app_token".into(),
                label: format!(
                    "App-level token ({}..., needs `connections:write`; Enter to skip)",
                    crate::channels::slack::APP_TOKEN_PREFIX
                ),
                default: None,
                secret: true,
            },
        )
        .await?;

        let app_token = app_token_from_answer(&recv_text(&mut responses).await?);
        if let Some(ref t) = app_token {
            if !looks_like_app_token(t) {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Warn,
                        text: format!(
                            "That does not look like an app-level token (they start `{}`).                              Saving it anyway — run `doctor` to check it.",
                            crate::channels::slack::APP_TOKEN_PREFIX
                        ),
                    },
                )
                .await?;
            }
        }

        // Validate bot token
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Validating bot token…".into(),
            },
        )
        .await?;

        let probe = probe_post(
            "https://slack.com/api/auth.test",
            &[("Authorization", &format!("Bearer {}", bot_token.trim()))],
            "",
        )
        .await;
        // Slack answers 200 even when it rejects the token, so the status says
        // nothing and `classify_status` cannot be used here — `ok` in the body
        // is the only signal. Anything that is neither `ok:true` nor `ok:false`
        // is an unrecognised response, not evidence against the token.
        let verdict = match &probe {
            Ok(r) if r.body.contains("\"ok\":true") => verdict::ProbeVerdict::Accepted,
            Ok(r) if r.body.contains("\"ok\":false") => {
                verdict::ProbeVerdict::Rejected(slack_error(&r.body))
            }
            Ok(_) => verdict::ProbeVerdict::Inconclusive("unrecognised response".into()),
            Err(e) => verdict::ProbeVerdict::Inconclusive(format!("{e}")),
        };
        if !verdict::resolve(&events, &mut responses, verdict, "bot token")
            .await?
            .should_persist()
        {
            send(
                &events,
                ProvisionEvent::Failed {
                    error: "The bot token was not saved — Slack is not configured.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Aborted(
                "bot token failed validation and was not saved".into(),
            ));
        }

        // Optional channel ID
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "channel_id".into(),
                label: "Channel ID to restrict bot to (Enter to skip)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let channel_id = recv_text(&mut responses).await?;
        let channel_id = if channel_id.trim().is_empty() {
            None
        } else {
            Some(channel_id.trim().to_string())
        };

        // Allowed users
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "allowed_users".into(),
                label: "Allowed user IDs (comma-separated, empty = deny all)".into(),
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
        config.channels_config.slack = Some(SlackConfig {
            bot_token: bot_token.trim().to_string(),
            app_token,
            channel_id,
            allowed_users,
        });

        send(
            &events,
            ProvisionEvent::Done {
                summary: "Slack bot configured.".into(),
            },
        )
        .await?;

        Ok(ProvisionOutcome::Configured)
    }
}

/// Turn the operator's app-token answer into what the config stores.
///
/// Empty stays empty: Socket Mode is optional, and an operator who skips it
/// gets polling, which is what every install got before this prompt existed.
/// Shared by both supported setup paths so neither can go back to hardcoding
/// the answer, which is how `app_token` became a key that `doctor` demanded
/// and no setup path could write.
pub(crate) fn app_token_from_answer(answer: &str) -> Option<String> {
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Does this look like a Slack app-level token?
///
/// A warning only. `apps.connections.open` is the real check and it opens a
/// live socket, so setup must not call it just to validate a string; `doctor`
/// owns that verdict.
pub(crate) fn looks_like_app_token(token: &str) -> bool {
    token
        .trim()
        .starts_with(crate::channels::slack::APP_TOKEN_PREFIX)
}

/// Slack reports the reason in `error` alongside `"ok": false`. Surfacing it
/// turns "may be invalid" into something the operator can act on —
/// `invalid_auth` and `account_inactive` need different fixes.
fn slack_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str().map(|s| s.to_string()))
        })
        .map_or_else(
            || "Slack rejected it".to_string(),
            |e| format!("Slack returned `{e}`"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::smoke::OfflineProbes;
    use crate::onboard::provision::test_support::{drive, scratch_profile, Answer};

    #[test]
    fn app_token_from_answer_keeps_a_token_and_drops_an_empty_answer() {
        assert_eq!(app_token_from_answer(""), None);
        assert_eq!(app_token_from_answer("   "), None);
        assert_eq!(
            app_token_from_answer("  xapp-1-A-placeholder  ").as_deref(),
            Some("xapp-1-A-placeholder"),
            "the answer must be stored trimmed, not dropped"
        );
    }

    #[test]
    fn only_an_xapp_prefixed_answer_looks_like_an_app_token() {
        assert!(looks_like_app_token("xapp-1-A-placeholder"));
        assert!(looks_like_app_token("  xapp-1-A-placeholder "));
        // A bot token pasted into the app-token prompt is the likely slip.
        assert!(!looks_like_app_token("xoxb-placeholder"));
    }

    /// The value the operator types has to reach the config. Until this fix it
    /// could not: both setup paths bound `app_token` to a constant, so `doctor`
    /// told operators to set a key no supported path could write.
    ///
    /// Driven to completion, which works because the probe has no egress here
    /// and resolves Inconclusive — the same assumption `provision::smoke`
    /// already makes for this provisioner.
    #[tokio::test]
    async fn an_app_token_the_operator_types_reaches_the_config() {
        // Same guarantee `provision::smoke` relies on: the probe must fail at
        // the transport layer, or a runner with egress gets `Rejected` (whose
        // safe default is to discard) and one without gets `Inconclusive`
        // (whose safe default is to persist), and the answer script below can
        // only be right for one of them.
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _offline = OfflineProbes::engage();
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &SlackProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Text("placeholder-slack-bot-token"),
                Answer::Text("xapp-1-A-placeholder"),
                Answer::Pick(0), // probe inconclusive -> save anyway
                Answer::Text(""),
                Answer::Text("rantaiclaw_user"),
            ],
        )
        .await;

        assert!(t.configured(), "expected configured, got {:?}", t.outcome);
        assert_eq!(
            config
                .channels_config
                .slack
                .as_ref()
                .expect("slack config written")
                .app_token
                .as_deref(),
            Some("xapp-1-A-placeholder"),
            "the answer must reach the config, not be dropped for a hardcoded None"
        );

        let prompts = t.prompts();
        let app_prompt = prompts
            .iter()
            .find(|p| p.contains("App-level token"))
            .unwrap_or_else(|| panic!("no app-token prompt was offered: {prompts:?}"));
        assert!(
            app_prompt.contains("xapp-") && app_prompt.contains("connections:write"),
            "the prompt must name the token shape and the scope it needs: {app_prompt}"
        );
        assert!(
            t.messages()
                .iter()
                .any(|m| m.contains("will not see replies inside a thread")),
            "skipping it must state the cost, in the same words the runtime warning uses: {:?}",
            t.messages()
        );
    }

    /// Skipping stays valid: an empty answer means polling, which is what every
    /// install got before the prompt existed.
    #[tokio::test]
    async fn skipping_the_app_token_still_configures_slack_for_polling() {
        // Same guarantee `provision::smoke` relies on: the probe must fail at
        // the transport layer, or a runner with egress gets `Rejected` (whose
        // safe default is to discard) and one without gets `Inconclusive`
        // (whose safe default is to persist), and the answer script below can
        // only be right for one of them.
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _offline = OfflineProbes::engage();
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();

        let t = drive(
            &SlackProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Text("placeholder-slack-bot-token"),
                Answer::Text(""),
                Answer::Pick(0),
                Answer::Text(""),
                Answer::Text("rantaiclaw_user"),
            ],
        )
        .await;

        assert!(t.configured(), "expected configured, got {:?}", t.outcome);
        assert_eq!(
            config
                .channels_config
                .slack
                .as_ref()
                .expect("slack config written")
                .app_token,
            None,
            "an empty answer must stay empty, not become an empty-string token"
        );
    }

    /// Both supported setup paths bound `app_token` to a constant, so the
    /// prompt could be deleted from either one and every behaviour test here
    /// would still pass. This binds the pair: neither file may go back to
    /// deciding the value for the operator.
    #[test]
    fn neither_setup_path_hardcodes_the_app_token() {
        // Assembled at runtime so this test does not match itself.
        let bind = format!("let app_{}", "token");
        let field_const = format!("app_{}: None", "token");
        for (path, src) in [
            ("provision/channels/slack.rs", include_str!("slack.rs")),
            ("wizard.rs", include_str!("../../wizard.rs")),
        ] {
            assert!(
                src.contains("app_token_from_answer"),
                "{path} must take the app token from the operator's answer"
            );
            assert!(
                !src.contains(field_const.as_str()),
                "{path} writes a constant None straight into the config"
            );
            // Every binding of the token must come from what the operator
            // typed. The shape this replaced annotated the type
            // (`let app_token: Option<String> = None`), so matching on the
            // assignment alone reads as green while the defect is back.
            for line in src.lines().filter(|l| l.trim_start().starts_with(&bind)) {
                assert!(
                    !line.contains("None") && !line.contains("String::new()"),
                    "{path} binds the app token to a constant instead of the \
                     operator's answer: {line}"
                );
            }
        }
    }
}
