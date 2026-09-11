//! In-chat runtime commands: `/model`, `/provider`, `/models`, `/providers`.
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
    let base_command = verb.split('@').next().unwrap_or(verb).to_ascii_lowercase();
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
        _ => None,
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
}
