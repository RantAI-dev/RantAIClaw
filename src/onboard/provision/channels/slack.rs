//! Slack provisioner — implements [`TuiProvisioner`] for in-TUI Slack bot setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::SlackConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_text, send};
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

        // Setup checklist — same list the legacy wizard prints and the docs
        // §4.3 enumerates. Rendered before the bot-token prompt because
        // installing the app is what issues the `xoxb-` token, and the scopes
        // and events on this list have to be set first; pasting a token that
        // lacks `im:history` or `files:write` would otherwise leave the
        // operator on the wrong side of a reinstall.
        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: crate::channels::slack::SLACK_SETUP_CHECKLIST.to_string(),
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

        // The probe and its classification now live in `channels::slack`, so the
        // console and this wizard ask Slack the same question and read the
        // answer by the same rule.
        let verdict = crate::channels::slack::validate_bot_token(bot_token.trim()).await;
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

        // The setup checklist must reach the operator: every scope and event
        // Slack requires for the bot to see DMs and upload files. Drop one
        // literal from the provisioner and this fails.
        let messages = t.messages();
        let checklist_bullet = messages
            .iter()
            .find(|m| m.contains("Bot Token Scopes") || m.contains("chat:write"))
            .unwrap_or_else(|| panic!("no bot-scope checklist bullet was offered: {messages:?}"));
        for literal in [
            "chat:write",
            "channels:history",
            "im:history",
            "groups:history",
            "mpim:history",
            "files:read",
            "files:write",
        ] {
            assert!(
                checklist_bullet.contains(literal),
                "the checklist bullet is missing `{literal}`: {checklist_bullet}"
            );
        }
        for literal in [
            "Socket Mode",
            "message.im",
            "message.channels",
            "message.groups",
            "message.mpim",
            "App Home",
        ] {
            assert!(
                checklist_bullet.contains(literal),
                "the checklist bullet is missing setting `{literal}`: {checklist_bullet}"
            );
        }
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

    /// Both setup paths must render the same Slack setup checklist, not a
    /// copy of one. Three scopes (DMs / private channels / group DMs) and
    /// `files:write` were missing from both the wizard's bullet and the docs;
    /// the provisioner listed no scopes at all. Each is required for the bot
    /// to see DMs and to upload files, which is what every operator following
    /// either setup path was promised. Slicing the test module off the source
    /// first so this test cannot accidentally match itself.
    ///
    /// Mirrors the existing `NO_APP_TOKEN_CONSEQUENCE` pattern: the literal
    /// text lives once in `src/channels/slack.rs`, and both setup paths
    /// reference the constant. So the test iterates over all three sources
    /// and checks the role each one plays: the constant carries every
    /// literal; the wizard and the provisioner both render the constant.
    #[test]
    fn slack_setup_checklist_renders_in_both_setup_paths() {
        // Slice the `#[cfg(test)]` module off the source so this test cannot
        // accidentally match itself. Mirrors the helper in
        // `provision/whatsapp_web.rs` and keeps the test order-independent.
        const TEST_MODULE_MARKER: &str = "\n#[cfg(test)]\nmod ";
        fn production_half(src: &str) -> &str {
            let cut = [TEST_MODULE_MARKER]
                .iter()
                .flat_map(|marker| src.match_indices(marker))
                .map(|(at, _)| at)
                .min()
                .unwrap_or(src.len());
            &src[..cut]
        }
        let required_scopes = [
            "chat:write",
            "channels:history",
            "im:history",
            "groups:history",
            "mpim:history",
            "files:read",
            "files:write",
        ];
        let required_settings = [
            "Socket Mode",
            "message.im",
            "message.channels",
            "message.groups",
            "message.mpim",
            "App Home",
        ];
        // The constant's value must carry every literal. Asserted against
        // the constant directly (not the surrounding source), so dropping a
        // scope from the `&str` body is what makes the assertion fall —
        // rather than a stale match in a doc comment. `chat:write` shows up
        // in a doc comment further down the file, so a substring search on
        // the whole file would not exercise the constant itself.
        for scope in required_scopes {
            assert!(
                crate::channels::slack::SLACK_SETUP_CHECKLIST.contains(scope),
                "SLACK_SETUP_CHECKLIST must list `{scope}`"
            );
        }
        for setting in required_settings {
            assert!(
                crate::channels::slack::SLACK_SETUP_CHECKLIST.contains(setting),
                "SLACK_SETUP_CHECKLIST must instruct the operator on `{setting}`"
            );
        }
        // Both setup paths render THAT constant, not a copy. Drop the
        // reference from either one and the substring assertion falls — the
        // path goes back to a private literal the constant cannot catch.
        for (path, src) in [
            ("provision/channels/slack.rs", include_str!("slack.rs")),
            ("wizard.rs", include_str!("../../wizard.rs")),
        ] {
            let prod = production_half(src);
            assert!(
                prod.contains("crate::channels::slack::SLACK_SETUP_CHECKLIST"),
                "{path} must reference the shared constant `crate::channels::slack::SLACK_SETUP_CHECKLIST` \
                 so the wizard and provisioner cannot drift"
            );
        }
    }

    /// The checklist must explain what `files:read` does and what happens
    /// without it. An operator following either setup path was told to add
    /// the scope but never told why; the wizard's `print_bullet` only
    /// indented the first line of the body, so the multi-line checklist
    /// dropped the consequence sentence. Drop the consequence from the
    /// constant and this falls.
    #[test]
    fn slack_setup_checklist_explains_the_files_read_consequence() {
        assert!(
            crate::channels::slack::SLACK_SETUP_CHECKLIST.contains("fetch failed"),
            "SLACK_SETUP_CHECKLIST must explain what happens without `files:read`; \
             got: {:?}",
            crate::channels::slack::SLACK_SETUP_CHECKLIST
        );
    }

    /// The Slack setup checklist must reach the operator before the bot-token
    /// prompt, because installing the app is what issues the `xoxb-` token and
    /// the scopes/events on the checklist have to be set first. The legacy
    /// wizard and the console card both render the list above the token
    /// input; the provisioner was the only one that asked for the token first.
    ///
    /// Matches the prompt by `id == "bot_token"` rather than by label or
    /// position, because `app_token` is also a prompt and a position-based
    /// assertion would pass on the original ordering (checklist-before-
    /// `app_token` already holds).
    #[tokio::test]
    async fn the_setup_checklist_message_precedes_the_bot_token_prompt() {
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
                Answer::Pick(0),
                Answer::Text(""),
                Answer::Text("rantaiclaw_user"),
            ],
        )
        .await;

        let checklist_text = crate::channels::slack::SLACK_SETUP_CHECKLIST;
        let checklist_idx = t
            .events
            .iter()
            .position(
                |e| matches!(e, ProvisionEvent::Message { text, .. } if text == checklist_text),
            )
            .unwrap_or_else(|| panic!("no checklist Message was offered: {:?}", t.events));
        let bot_token_idx = t
            .events
            .iter()
            .position(|e| matches!(e, ProvisionEvent::Prompt { id, .. } if id == "bot_token"))
            .unwrap_or_else(|| panic!("no bot_token Prompt was offered: {:?}", t.events));

        assert!(
            checklist_idx < bot_token_idx,
            "the Slack setup checklist must arrive before the bot-token prompt, \
             so the operator sees the scopes and events to set before installing \
             the app; got checklist at event {checklist_idx} and bot_token prompt \
             at event {bot_token_idx}: {:?}",
            t.events
        );
    }
}
