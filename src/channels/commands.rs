//! In-chat runtime commands: `/model`, `/models`, `/new` and `/clear`, `/start`
//! and `/help`, and the answer to a slash command that does not exist.
//!
//! Moved out of `mod.rs` verbatim (plan 121, row 6). No behaviour change; the
//! tests stayed with the dispatch fixtures they share, so the moved items are
//! `pub(crate)`.

use super::traits::{self, Channel};
use super::{history, routing, ChannelRouteSelection, ChannelRuntimeContext};
use crate::providers;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChannelRuntimeCommand {
    ShowProviders,
    SetProvider(String),
    ShowModel,
    SetModel(String),
    /// `/start` or `/help`: a welcome with the command list.
    Welcome,
    /// `/new` or `/clear`: clear this conversation's history.
    Reset,
    /// A slash command the runtime does not know, as it was typed.
    UnknownCommand(String),
    /// An unknown command carrying `@<name>`. In a Telegram group it may be
    /// meant for another bot, and the runtime cannot tell, so it says nothing.
    AddressedElsewhere,
}

/// The prefix a command verb carries on `channel_name`, for runtime commands
/// and approval replies alike.
///
/// Slack treats every message starting with `/` as a slash command and answers
/// "/approve is not a valid command" without ever delivering it, so a prompt
/// telling a Slack owner to type `/approve` names a reply that cannot arrive.
/// The approval parser has always accepted the slash-less form, so for
/// approvals this only changes what is printed; runtime commands parse with it
/// as well. It lives here, read by both, because two copies of this rule is how
/// one surface learns the Slack exception and the other goes on printing a
/// slash.
///
/// An explicit list, not a guess: a channel is added here after its platform is
/// shown to intercept the prefix.
pub(crate) fn command_prefix(channel_name: &str) -> &'static str {
    match channel_name {
        "slack" => "",
        _ => "/",
    }
}

/// The channels that answer runtime commands: the four supported tier
/// channels. WhatsApp Web registers as `whatsapp`, a name it shares with
/// WhatsApp Cloud; the two are configured one at a time, so the name does not
/// need to tell them apart.
pub(crate) fn supports_runtime_model_switch(channel_name: &str) -> bool {
    matches!(channel_name, "telegram" | "discord" | "slack" | "whatsapp")
}

/// What a reset does **not** reach: the messages the chat app itself still
/// shows.
///
/// `/new` clears the conversation on our side and the app goes on displaying
/// every earlier message, so the reply left out the one thing the reader is
/// looking at. Each hint names its own channel, the way each module already
/// names itself to the model in `delivery_instructions_for`, and claims only
/// what that platform actually offers: Telegram and WhatsApp have a per-chat
/// clear, a Discord DM does not. A channel nobody has checked gets `None`,
/// because saying nothing beats inventing an app's behaviour.
///
/// No hint names a menu item. A label nobody verified is the kind of small lie
/// this effort keeps finding; a drive can confirm the real wording, and then the
/// docs may name it.
pub(crate) fn manual_clear_hint(channel_name: &str) -> Option<&'static str> {
    match channel_name {
        "telegram" => Some(
            "Telegram still shows the earlier messages. Clearing them is done in Telegram, from \
             this chat's menu.",
        ),
        // One arm for both WhatsApp types: they share `Channel::name()`.
        "whatsapp" => Some(
            "WhatsApp still shows the earlier messages. Clearing them is done in WhatsApp, from \
             this chat's menu.",
        ),
        // No clear-chat in a DM, so Telegram's wording would be a promise
        // Discord cannot keep.
        "discord" => {
            Some("Discord still shows the earlier messages, and the bot cannot remove them.")
        }
        _ => None,
    }
}

/// Whether `name` has the shape of a command name: ASCII letters, digits and
/// underscores, 1 to 32 of them, as Telegram defines one. A path such as
/// `/etc/hosts` also starts with a slash and is not a command.
fn is_command_name(name: &str) -> bool {
    (1..=32).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(crate) fn parse_runtime_command(
    channel_name: &str,
    content: &str,
) -> Option<ChannelRuntimeCommand> {
    if !supports_runtime_model_switch(channel_name) {
        return None;
    }

    let prefix = command_prefix(channel_name);
    let mut parts = content.split_whitespace();
    let command_token = parts.next()?;
    let verb = command_token.strip_prefix(prefix)?;
    let (base, addressed_to) = match verb.split_once('@') {
        Some((base, name)) => (base, Some(name)),
        None => (verb, None),
    };
    let base_command = base.to_ascii_lowercase();
    let arguments: Vec<&str> = parts.collect();

    // Without a prefix the verb also starts ordinary sentences ("model apa yang
    // kamu pakai?"), so a bare-verb channel takes the verb alone or the verb and
    // one token, and leaves anything longer to the model.
    if prefix.is_empty() && arguments.len() > 1 {
        return None;
    }

    match base_command.as_str() {
        "models" => Some(match arguments.first() {
            Some(provider) => ChannelRuntimeCommand::SetProvider(provider.trim().to_string()),
            None => ChannelRuntimeCommand::ShowProviders,
        }),
        "model" => {
            let model = arguments.join(" ").trim().to_string();
            if model.is_empty() {
                Some(ChannelRuntimeCommand::ShowModel)
            } else {
                Some(ChannelRuntimeCommand::SetModel(model))
            }
        }
        // Everything below is a slash command. On a channel without a prefix
        // these words are ordinary chat, and a new top-level message already
        // starts a new conversation there.
        _ if prefix.is_empty() => None,
        "start" | "help" => Some(ChannelRuntimeCommand::Welcome),
        "new" | "clear" => Some(ChannelRuntimeCommand::Reset),
        // Approval replies and pairing codes are consumed before a message
        // reaches the runtime commands. Claiming one here would break that
        // order, so the stages that own them are asked, not copied.
        _ if super::approval_relay::is_approval_reply(content)
            || super::pairing::parse_pairing_command(content).is_some() =>
        {
            None
        }
        _ if !is_command_name(&base_command) => None,
        _ if addressed_to.is_some() => Some(ChannelRuntimeCommand::AddressedElsewhere),
        _ => Some(ChannelRuntimeCommand::UnknownCommand(format!(
            "{prefix}{base}"
        ))),
    }
}

pub(crate) fn build_models_help_response(
    current: &ChannelRouteSelection,
    workspace_dir: &Path,
    prefix: &str,
) -> String {
    let mut response = String::new();
    let _ = writeln!(
        response,
        "Current provider: `{}`\nCurrent model: `{}`",
        current.provider, current.model
    );
    let _ = writeln!(response, "\nSwitch model with `{prefix}model <model-id>`.");

    let cached_models = routing::load_cached_model_preview(workspace_dir, &current.provider);
    if cached_models.is_empty() {
        let _ = writeln!(
            response,
            "\nNo cached model list found for `{}`. Ask the operator to run `rantaiclaw models refresh --provider {}`.",
            current.provider, current.provider
        );
    } else {
        let _ = writeln!(
            response,
            "\nCached model IDs (top {}):",
            cached_models.len()
        );
        for model in cached_models {
            let _ = writeln!(response, "- `{model}`");
        }
    }

    response
}

pub(crate) fn build_providers_help_response(
    current: &ChannelRouteSelection,
    prefix: &str,
) -> String {
    let mut response = String::new();
    let _ = writeln!(
        response,
        "Current provider: `{}`\nCurrent model: `{}`",
        current.provider, current.model
    );
    let _ = writeln!(
        response,
        "\nSwitch provider with `{prefix}models <provider>`."
    );
    let _ = writeln!(response, "Switch model with `{prefix}model <model-id>`.\n");
    response.push_str("Available providers:\n");
    for provider in providers::list_providers() {
        if provider.aliases.is_empty() {
            let _ = writeln!(response, "- {}", provider.name);
        } else {
            let _ = writeln!(
                response,
                "- {} (aliases: {})",
                provider.name,
                provider.aliases.join(", ")
            );
        }
    }
    response
}

/// The reply to a successful `/models <provider>`.
///
/// The scope in this sentence is not decoration. Route overrides are keyed by
/// `conversation_history_key`, so the switch follows the conversation and not
/// the person, which `channels/dispatch.rs` documents as a deliberate choice.
/// The old wording said "for this sender session", and there is no such thing:
/// in a group one member typed `/model`, the model changed for everybody, and
/// the bot then told them the change was theirs alone.
fn provider_switched_message(provider: &str, model: &str, prefix: &str) -> String {
    format!(
        "Provider switched to `{provider}` for this conversation. In a group chat that applies \
         to everyone here. Current model is `{model}`.\nUse `{prefix}model <model-id>` to set a \
         provider-compatible model."
    )
}

/// The reply to a successful `/model <model-id>`. Same scope, same reason as
/// [`provider_switched_message`].
fn model_switched_message(model: &str, provider: &str) -> String {
    format!(
        "Model switched to `{model}` for provider `{provider}` in this conversation. In a group \
         chat that applies to everyone here."
    )
}

/// The runtime commands a slash channel answers, one line each. Every reply
/// that lists commands uses this, so a new command is listed everywhere or
/// nowhere.
fn command_list(prefix: &str) -> String {
    format!(
        "- `{prefix}model` shows the model; `{prefix}model <model-id>` switches it for this conversation.\n\
         - `{prefix}models` lists providers; `{prefix}models <provider>` switches the provider.\n\
         - `{prefix}new` or `{prefix}clear` clears this conversation's history. Long-term memory stays.\n"
    )
}

/// The reply to `/start`, which Telegram sends when someone first opens the
/// bot, and to `/help`.
fn welcome_message(prefix: &str) -> String {
    format!(
        "Hi. Send a message and I will answer it. Commands:\n{}",
        command_list(prefix)
    )
}

/// The reply to a slash command that does not exist. It used to reach the
/// model, which invented a result: `/clear` was answered "session cleared"
/// while the conversation kept every turn.
fn unknown_command_message(command: &str, prefix: &str) -> String {
    format!(
        "`{command}` is not a command here, so nothing ran. Commands:\n{}",
        command_list(prefix)
    )
}

/// The reply to `/new` and `/clear`: what was cleared, what was kept, and how
/// to remove the rest.
const RESET_MESSAGE: &str =
    "Cleared this conversation's history. The model chosen here stays until the daemon restarts. \
     Long-term memory stays too: facts the bot saved with its memory tools remain available in \
     every conversation. To remove those, the operator runs `rantaiclaw memory list` and then \
     `rantaiclaw memory clear --key <key>` on the host.";

/// [`RESET_MESSAGE`], plus what the chat app still shows when we know the
/// answer for this channel.
fn reset_message(channel_name: &str) -> String {
    match manual_clear_hint(channel_name) {
        Some(hint) => format!("{RESET_MESSAGE} {hint}"),
        None => RESET_MESSAGE.to_string(),
    }
}

pub(crate) async fn handle_runtime_command_if_needed(
    ctx: &ChannelRuntimeContext,
    msg: &traits::ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
) -> bool {
    let Some(command) = parse_runtime_command(&msg.channel, &msg.content) else {
        return false;
    };

    let Some(channel) = target_channel else {
        return true;
    };

    let sender_key = super::dispatch::conversation_history_key(msg);
    let mut current = routing::get_route_selection(ctx, &sender_key);
    // Every command a reply names is spelled the way this channel can send it.
    let prefix = command_prefix(&msg.channel);

    let response = match command {
        ChannelRuntimeCommand::ShowProviders => build_providers_help_response(&current, prefix),
        ChannelRuntimeCommand::SetProvider(raw_provider) => {
            match routing::resolve_provider_alias(&raw_provider) {
                Some(provider_name) => {
                    match routing::get_or_create_provider(ctx, &provider_name).await {
                        Ok(_) => {
                            if provider_name != current.provider {
                                current.provider = provider_name.clone();
                                routing::set_route_selection(ctx, &sender_key, current.clone());
                                history::clear_sender_history(ctx, &sender_key);
                            }

                            provider_switched_message(&provider_name, &current.model, prefix)
                        }
                        Err(err) => {
                            let safe_err = providers::sanitize_api_error(&err.to_string());
                            format!(
                            "Failed to initialize provider `{provider_name}`. Route unchanged.\nDetails: {safe_err}"
                        )
                        }
                    }
                }
                None => format!(
                    "Unknown provider `{raw_provider}`. Use `{prefix}models` to list valid providers."
                ),
            }
        }
        ChannelRuntimeCommand::ShowModel => {
            build_models_help_response(&current, ctx.workspace_dir.as_path(), prefix)
        }
        ChannelRuntimeCommand::SetModel(raw_model) => {
            let model = raw_model.trim().trim_matches('`').to_string();
            if model.is_empty() {
                format!("Model ID cannot be empty. Use `{prefix}model <model-id>`.")
            } else {
                current.model = model.clone();
                routing::set_route_selection(ctx, &sender_key, current.clone());
                history::clear_sender_history(ctx, &sender_key);

                model_switched_message(&model, &current.provider)
            }
        }
        ChannelRuntimeCommand::Welcome => welcome_message(prefix),
        ChannelRuntimeCommand::Reset => {
            // This conversation's key only: another chat, topic or thread
            // keeps its history.
            history::clear_sender_history(ctx, &sender_key);
            reset_message(&msg.channel)
        }
        ChannelRuntimeCommand::UnknownCommand(command) => unknown_command_message(&command, prefix),
        ChannelRuntimeCommand::AddressedElsewhere => return true,
    };

    if let Err(err) = channel.send(&msg.reply(response)).await {
        tracing::warn!(
            "Failed to send runtime command response on {}: {err}",
            channel.name()
        );
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both replies used to say the switch was "for this sender session". No
    /// such scope exists: route overrides are keyed by
    /// `dispatch::conversation_history_key`, so in a group one member's
    /// `/model` moves the model for everyone and the bot then described the
    /// change as that member's own.
    #[test]
    fn a_switch_reply_names_the_conversation_and_never_a_sender_session() {
        // Assembled at runtime so this test does not match itself.
        let stale = format!("{} session", "sender");
        for reply in [
            provider_switched_message("openai", "gpt-5", "/"),
            model_switched_message("gpt-5", "openai"),
        ] {
            assert!(
                !reply.contains(stale.as_str()),
                "the reply claims a scope that does not exist: {reply}"
            );
            assert!(
                reply.contains("this conversation"),
                "the reply must say what the switch actually applies to: {reply}"
            );
            assert!(
                reply.contains("everyone here"),
                "a group member must be told the switch is not theirs alone: {reply}"
            );
        }
    }

    /// The replies still have to carry the values they are given, so the
    /// scope-wording assertions above cannot be satisfied by a constant.
    #[test]
    fn a_switch_reply_carries_the_provider_and_model_it_was_given() {
        let provider = provider_switched_message("anthropic", "claude-opus-5", "/");
        assert!(provider.contains("`anthropic`") && provider.contains("`claude-opus-5`"));

        let model = model_switched_message("claude-opus-5", "anthropic");
        assert!(model.contains("`claude-opus-5`") && model.contains("`anthropic`"));
    }

    /// Guards the file rather than the two functions: a third command added
    /// tomorrow must not reintroduce the phrase in a reply. Doc comments are
    /// skipped so the note explaining the old wording does not trip this.
    #[test]
    fn no_reply_string_in_this_file_claims_a_sender_session() {
        let stale = format!("{} session", "sender");
        for (n, line) in include_str!("commands.rs").lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            assert!(
                !line.contains(stale.as_str()),
                "line {} reintroduces the phrase: {line}",
                n + 1
            );
        }
    }

    /// The command grammar on each tier channel, as data. Slack cannot send a
    /// leading slash, so it takes the bare verb; the other tier channels take
    /// the slash form and treat a bare `model` as chat. On a bare-verb channel
    /// the verb also starts ordinary sentences, so only the verb alone or the
    /// verb and one token is a command. Every mismatch is collected before the
    /// assert, so two broken rows read as two broken rows.
    #[test]
    fn runtime_command_grammar_per_tier_channel() {
        use ChannelRuntimeCommand::{SetModel, SetProvider, ShowModel, ShowProviders};

        let inputs = [
            "/model",
            "/model X",
            "/models",
            "/models X",
            "model",
            "model X",
            "model apa yang kamu pakai?",
            "hello there",
        ];
        let slash = [
            Some(ShowModel),
            Some(SetModel("X".into())),
            Some(ShowProviders),
            Some(SetProvider("X".into())),
            None,
            None,
            None,
            None,
        ];
        let bare = [
            None,
            None,
            None,
            None,
            Some(ShowModel),
            Some(SetModel("X".into())),
            None,
            None,
        ];
        let not_a_tier_channel = [None, None, None, None, None, None, None, None];
        let table = [
            ("telegram", &slash),
            ("discord", &slash),
            ("whatsapp", &slash),
            ("slack", &bare),
            ("mattermost", &not_a_tier_channel),
        ];

        let mut mismatches = Vec::new();
        for (channel, expected) in table {
            for (input, want) in inputs.iter().zip(expected.iter()) {
                let got = parse_runtime_command(channel, input);
                if &got != want {
                    mismatches.push(format!("{channel} {input:?}: got {got:?}, want {want:?}"));
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "grammar mismatches:\n{}",
            mismatches.join("\n")
        );
    }

    /// F-30: `route_overrides` is built empty on every runtime start
    /// (`mod.rs:1293`) and is never written to disk, so a restart returns every
    /// conversation to the configured model. The reply promised "The model
    /// chosen here stays", which is true of a reset and false of a restart, and
    /// a reader could not tell the difference from the sentence.
    #[test]
    fn the_reset_reply_says_how_long_a_model_choice_lasts() {
        assert!(
            RESET_MESSAGE.to_lowercase().contains("restart"),
            "the reply must say the choice dies with the daemon: {RESET_MESSAGE}"
        );
    }

    /// DECIDED 2026-09-12: Slack stays as it is. With threading on, the default,
    /// a new top-level message is already a new conversation, so the command was
    /// never wired there. Pinned so the decision cannot be undone by accident:
    /// the bare-verb arm at `:110` has to keep stopping these before `:112`.
    #[test]
    fn slack_never_answers_new_or_clear() {
        for input in ["new", "clear", "/new", "/clear"] {
            assert_eq!(
                parse_runtime_command("slack", input),
                None,
                "Slack must not answer {input:?}"
            );
        }
    }

    /// The owner read `/new` in a live chat and could not tell what it had done,
    /// because the app still showed everything. One test per channel, so a
    /// mutation that breaks one hint cannot hide behind another's assertion.
    #[test]
    fn telegram_reset_reply_points_at_the_chat_menu() {
        let reply = reset_message("telegram");
        assert!(
            reply.starts_with(RESET_MESSAGE),
            "the hint is added to the reply, never in place of it: {reply}"
        );
        assert!(
            reply.contains("Telegram still shows the earlier messages"),
            "{reply}"
        );
        assert!(reply.contains("this chat's menu"), "{reply}");
    }

    /// Both WhatsApp types share `Channel::name()`, so one arm covers them.
    #[test]
    fn whatsapp_reset_reply_points_at_the_chat_menu() {
        let reply = reset_message("whatsapp");
        assert!(reply.starts_with(RESET_MESSAGE), "{reply}");
        assert!(
            reply.contains("WhatsApp still shows the earlier messages"),
            "{reply}"
        );
        assert!(reply.contains("this chat's menu"), "{reply}");
    }

    /// A Discord DM has no clear-chat, so Telegram's wording would be a promise
    /// the bot cannot keep.
    #[test]
    fn discord_reset_reply_says_the_messages_stay() {
        let reply = reset_message("discord");
        assert!(reply.starts_with(RESET_MESSAGE), "{reply}");
        assert!(
            reply.contains("Discord still shows the earlier messages")
                && reply.contains("cannot remove them"),
            "{reply}"
        );
        assert!(
            !reply.contains("menu"),
            "a DM has no chat menu, so the reply must not point at one: {reply}"
        );
    }

    /// Saying nothing beats inventing an app's behaviour, so a channel nobody
    /// has checked gets today's message byte for byte.
    #[test]
    fn a_channel_without_a_hint_gets_the_unchanged_reply() {
        for channel in ["mattermost", "irc", "signal", "slack"] {
            assert_eq!(
                reset_message(channel),
                RESET_MESSAGE,
                "{channel} must get the unchanged reply"
            );
        }
    }

    /// The honesty guard. A hint may name the app and say where the control
    /// lives; it may not quote a label nobody has verified. Asserted as a closed
    /// list of spellings plus the quote characters that would wrap one, rather
    /// than as a pattern: three hints are short enough for a reviewer to read,
    /// and a fourth channel's hint has to pass here before it ships.
    ///
    /// The ASCII apostrophe is deliberately absent. English writes a possessive
    /// with it, and forbidding it flagged "this chat's menu" on the first run:
    /// the guard is about a quoted label, not about punctuation.
    #[test]
    fn no_hint_quotes_a_menu_label() {
        let forbidden = [
            "Delete Chat",
            "Delete chat",
            "Clear Chat",
            "Clear chat",
            "\"",
            "\u{201C}",
            "\u{201D}",
        ];
        for channel in ["telegram", "whatsapp", "discord"] {
            let hint = manual_clear_hint(channel).expect("this channel has a hint");
            for label in forbidden {
                assert!(
                    !hint.contains(label),
                    "{channel}'s hint quotes a label nobody verified: {hint}"
                );
            }
        }
        assert!(
            manual_clear_hint("mattermost").is_none() && manual_clear_hint("slack").is_none(),
            "a channel nobody checked must get no hint at all"
        );
    }

    /// A message that starts with a path also starts with a slash. It is not a
    /// command, and it must still reach the model rather than be answered with
    /// the command list.
    #[test]
    fn a_path_that_starts_with_a_slash_is_not_a_command() {
        for text in [
            "/etc/hosts",
            "/home/rantaiclaw_user/notes.txt please read it",
        ] {
            assert_eq!(parse_runtime_command("telegram", text), None, "{text}");
        }
    }
}
