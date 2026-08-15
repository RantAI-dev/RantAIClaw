//! The single channel construction table.
//!
//! Moved out of `mod.rs` verbatim (plan 121, row 8). No behaviour change — this
//! is the table plan 120 unified, in a file named after what it does.

use super::irc;
use super::traits::Channel;
use super::{
    DingTalkChannel, DiscordChannel, EmailChannel, IMessageChannel, IrcChannel, LinqChannel,
    MattermostChannel, NextcloudTalkChannel, QQChannel, SignalChannel, SlackChannel,
    TelegramChannel, WhatsAppChannel,
};
use crate::config::Config;
use std::sync::Arc;

/// Every channel the config actually configures, as `(key, display, channel)`.
///
/// The single construction site. It was written out separately in `doctor_channels`
/// and `start_channels_with_cancellation` (and, until it was deleted, a third copy
/// in the channel registry), and the copies had already drifted with a
/// user-visible consequence: the doctor had **no Mattermost branch**, so an
/// operator whose Mattermost bot token expired was told everything was healthy
/// while that channel silently never answered. `MattermostChannel::health_check`
/// had no live caller at all.
///
/// `key` is the lowercase `Channel::name()` value — the same identifier
/// `channels_by_name`, the per-channel allowlists and cron delivery use. `display`
/// is operator-facing. The two WhatsApp variants share the key `whatsapp` because
/// they share `Channel::name()`; they are mutually exclusive, selected by
/// `wa.mode`, so only one is ever built.
pub(crate) fn build_configured_channels(
    config: &Config,
) -> Vec<(&'static str, &'static str, Arc<dyn Channel>)> {
    let mut channels: Vec<(&'static str, &'static str, Arc<dyn Channel>)> = Vec::new();

    if let Some(ref tg) = config.channels_config.telegram {
        channels.push((
            "telegram",
            "Telegram",
            Arc::new(
                TelegramChannel::new(
                    tg.bot_token.clone(),
                    tg.allowed_users.clone(),
                    tg.mention_only,
                )
                .with_streaming(tg.stream_mode, tg.draft_update_interval_ms),
            ),
        ));
    }

    if let Some(ref dc) = config.channels_config.discord {
        channels.push((
            "discord",
            "Discord",
            Arc::new(
                DiscordChannel::new(
                    dc.bot_token.clone(),
                    dc.guild_id.clone(),
                    dc.allowed_users.clone(),
                    dc.listen_to_bots,
                    dc.mention_only,
                )
                // Inbound images obey the operator's size cap, not a default
                // the channel invented for itself.
                .with_multimodal(config.multimodal.clone()),
            ),
        ));
    }

    if let Some(ref sl) = config.channels_config.slack {
        // Socket Mode is not implemented — the channel polls
        // `conversations.history`. Say so rather than accepting an app-level
        // token in silence: an operator who supplied one is entitled to know
        // it changes nothing, and a silent no-op is how this key went
        // unnoticed for as long as it did.
        if sl
            .app_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
        {
            tracing::warn!(
                "Slack: `app_token` is set but ignored — this build polls conversations.history \
                 and does not implement Socket Mode. Remove the key, or leave it for when it does."
            );
        }
        channels.push((
            "slack",
            "Slack",
            Arc::new(SlackChannel::new(
                sl.bot_token.clone(),
                sl.channel_id.clone(),
                sl.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref mm) = config.channels_config.mattermost {
        channels.push((
            "mattermost",
            "Mattermost",
            Arc::new(MattermostChannel::new(
                mm.url.clone(),
                mm.bot_token.clone(),
                mm.channel_id.clone(),
                mm.allowed_users.clone(),
                mm.thread_replies.unwrap_or(true),
                mm.mention_only.unwrap_or(false),
            )),
        ));
    }

    if let Some(ref im) = config.channels_config.imessage {
        channels.push((
            "imessage",
            "iMessage",
            Arc::new(IMessageChannel::new(im.allowed_contacts.clone())),
        ));
    }

    #[cfg(feature = "channel-matrix")]
    if let Some(ref mx) = config.channels_config.matrix {
        channels.push((
            "matrix",
            "Matrix",
            Arc::new(super::MatrixChannel::new_with_session_hint(
                mx.homeserver.clone(),
                mx.access_token.clone(),
                mx.room_id.clone(),
                mx.allowed_users.clone(),
                mx.user_id.clone(),
                mx.device_id.clone(),
            )),
        ));
    }

    #[cfg(not(feature = "channel-matrix"))]
    if config.channels_config.matrix.is_some() {
        tracing::warn!(
            "Matrix channel is configured but this build was compiled without `channel-matrix`; skipping Matrix health check."
        );
    }

    if let Some(ref sig) = config.channels_config.signal {
        channels.push((
            "signal",
            "Signal",
            Arc::new(SignalChannel::new(
                sig.http_url.clone(),
                sig.account.clone(),
                sig.group_id.clone(),
                sig.allowed_from.clone(),
                sig.ignore_attachments,
                sig.ignore_stories,
            )),
        ));
    }

    if let Some(ref wa) = config.channels_config.whatsapp {
        if wa.is_ambiguous_config() {
            tracing::warn!(
                "WhatsApp config has both phone_number_id and session_path set; preferring Cloud API mode. Remove one selector to avoid ambiguity."
            );
        }
        // Runtime negotiation: detect backend type from config
        match wa.backend_type() {
            "cloud" => {
                // Cloud API mode: requires phone_number_id, access_token, verify_token
                if wa.is_cloud_config() {
                    channels.push((
                        "whatsapp",
                        "WhatsApp",
                        Arc::new(
                            WhatsAppChannel::new(
                                wa.access_token.clone().unwrap_or_default(),
                                wa.phone_number_id.clone().unwrap_or_default(),
                                wa.verify_token.clone().unwrap_or_default(),
                                wa.allowed_numbers.clone(),
                            )
                            .with_multimodal(config.multimodal.clone()),
                        ),
                    ));
                } else {
                    tracing::warn!("WhatsApp Cloud API configured but missing required fields (phone_number_id, access_token, verify_token)");
                }
            }
            "web" => {
                // Web mode: requires session_path
                #[cfg(feature = "whatsapp-web")]
                if wa.is_web_config() {
                    channels.push((
                        "whatsapp",
                        "WhatsApp",
                        Arc::new(super::WhatsAppWebChannel::new(
                            wa.session_path.clone().unwrap_or_default(),
                            wa.pair_phone.clone(),
                            wa.pair_code.clone(),
                            wa.allowed_numbers.clone(),
                        )),
                    ));
                } else {
                    tracing::warn!("WhatsApp Web configured but session_path not set");
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    tracing::warn!("WhatsApp Web backend requires 'whatsapp-web' feature. Enable with: cargo build --features whatsapp-web");
                }
            }
            _ => {
                tracing::warn!("WhatsApp config invalid: neither phone_number_id (Cloud API) nor session_path (Web) is set");
            }
        }
    }

    if let Some(ref lq) = config.channels_config.linq {
        channels.push((
            "linq",
            "Linq",
            Arc::new(
                LinqChannel::new(
                    lq.api_token.clone(),
                    lq.from_phone.clone(),
                    lq.allowed_senders.clone(),
                )
                .with_multimodal(config.multimodal.clone()),
            ),
        ));
    }

    if let Some(ref nc) = config.channels_config.nextcloud_talk {
        channels.push((
            "nextcloud_talk",
            "Nextcloud Talk",
            Arc::new(NextcloudTalkChannel::new(
                nc.base_url.clone(),
                nc.app_token.clone(),
                nc.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref email_cfg) = config.channels_config.email {
        channels.push((
            "email",
            "Email",
            Arc::new(
                EmailChannel::new(email_cfg.clone())
                    .with_approval_owners(config.channels_config.approval_owners.clone()),
            ),
        ));
    }

    if let Some(ref irc) = config.channels_config.irc {
        channels.push((
            "irc",
            "IRC",
            Arc::new(IrcChannel::new(irc::IrcChannelConfig {
                server: irc.server.clone(),
                port: irc.port,
                nickname: irc.nickname.clone(),
                username: irc.username.clone(),
                channels: irc.channels.clone(),
                allowed_users: irc.allowed_users.clone(),
                server_password: irc.server_password.clone(),
                nickserv_password: irc.nickserv_password.clone(),
                sasl_password: irc.sasl_password.clone(),
                verify_tls: irc.verify_tls.unwrap_or(true),
                allow_insecure_tls_with_password: irc.allow_insecure_tls_with_password,
                approval_owners: config.channels_config.approval_owners.clone(),
            })),
        ));
    }

    #[cfg(feature = "channel-lark")]
    if let Some(ref lk) = config.channels_config.lark {
        channels.push((
            "lark",
            "Lark",
            Arc::new(super::LarkChannel::from_config(lk)),
        ));
    }

    #[cfg(not(feature = "channel-lark"))]
    if config.channels_config.lark.is_some() {
        tracing::warn!(
            "Lark channel is configured but this build was compiled without `channel-lark`; skipping Lark health check."
        );
    }

    if let Some(ref dt) = config.channels_config.dingtalk {
        channels.push((
            "dingtalk",
            "DingTalk",
            Arc::new(DingTalkChannel::new(
                dt.client_id.clone(),
                dt.client_secret.clone(),
                dt.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref qq) = config.channels_config.qq {
        channels.push((
            "qq",
            "QQ",
            Arc::new(QQChannel::new(
                qq.app_id.clone(),
                qq.app_secret.clone(),
                qq.allowed_users.clone(),
            )),
        ));
    }

    channels
}
