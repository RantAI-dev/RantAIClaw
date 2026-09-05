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
/// The ONE construction of the WhatsApp Cloud channel.
///
/// The gateway used to build its own for the webhook path, and the two drifted:
/// the factory applied `with_multimodal`, the gateway did not, so a WhatsApp
/// message arriving over the webhook was processed without the operator's image
/// caps. Both callers go through here now, so a future option cannot be added to
/// one and forgotten in the other.
///
/// Cloud mode only. `is_cloud_config()` requires `phone_number_id`, which is
/// also what makes `backend_type()` say "cloud", so this one guard covers both.
pub(crate) fn build_whatsapp_cloud(config: &Config) -> Option<Arc<WhatsAppChannel>> {
    let wa = config.channels_config.whatsapp.as_ref()?;
    if !wa.is_cloud_config() {
        return None;
    }
    Some(Arc::new(
        WhatsAppChannel::new(
            wa.access_token.clone().unwrap_or_default(),
            wa.phone_number_id.clone().unwrap_or_default(),
            wa.verify_token.clone().unwrap_or_default(),
            wa.allowed_numbers.clone(),
        )
        .with_multimodal(config.multimodal.clone()),
    ))
}

/// The ONE construction of the Linq channel. See [`build_whatsapp_cloud`].
pub(crate) fn build_linq(config: &Config) -> Option<Arc<LinqChannel>> {
    let lq = config.channels_config.linq.as_ref()?;
    Some(Arc::new(
        LinqChannel::new(
            lq.api_token.clone(),
            lq.from_phone.clone(),
            lq.allowed_senders.clone(),
        )
        .with_multimodal(config.multimodal.clone()),
    ))
}

/// The ONE construction of the Nextcloud Talk channel. See
/// [`build_whatsapp_cloud`]. This one carries no multimodal caps on either
/// path today — that is the channel's current shape, not a drift.
pub(crate) fn build_nextcloud_talk(config: &Config) -> Option<Arc<NextcloudTalkChannel>> {
    let nc = config.channels_config.nextcloud_talk.as_ref()?;
    Some(Arc::new(NextcloudTalkChannel::new(
        nc.base_url.clone(),
        nc.app_token.clone(),
        nc.allowed_users.clone(),
    )))
}

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
                .with_streaming(tg.stream_mode, tg.draft_update_interval_ms)
                .with_multimodal(config.multimodal.clone()),
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
        // The "`app_token` is set but ignored" note is an operator-facing,
        // one-time warning — emitted from `warn_unused_channel_config` on the
        // startup/doctor paths, NOT here, so the cron delivery path (which builds
        // channels on every scheduled run) does not re-log it as a recurring fault.
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
                if let Some(channel) = build_whatsapp_cloud(config) {
                    channels.push(("whatsapp", "WhatsApp", channel));
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
        let _ = lq;
        if let Some(channel) = build_linq(config) {
            channels.push(("linq", "Linq", channel));
        }
    }

    if let Some(channel) = build_nextcloud_talk(config) {
        channels.push(("nextcloud_talk", "Nextcloud Talk", channel));
    }

    if let Some(ref email_cfg) = config.channels_config.email {
        channels.push((
            "email",
            "Email",
            Arc::new(
                EmailChannel::new(email_cfg.clone())
                    .with_approval_owners(config.channels_config.approval_owners.clone())
                    .with_multimodal(config.multimodal.clone()),
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

/// Build exactly one channel by its lowercase `key`, for the cron delivery path
/// (which needs a single target, not the whole fleet). Covers only the
/// announce-capable channels — the set `channel_supports_announce_delivery`
/// allows, which is the only set cron delivery selects on; returns `None` for any
/// other key or when that channel is not configured. Unlike
/// `build_configured_channels` this allocates one channel, not ~15, and emits no
/// construction-time warnings. Keep this key set a superset of
/// `channel_supports_announce_delivery`; if that gate widens, add the key here.
/// Constructors are copied verbatim from `build_configured_channels` so the two
/// cannot drift on fields.
pub(crate) fn build_one(config: &Config, key: &str) -> Option<Arc<dyn Channel>> {
    match key {
        "telegram" => config.channels_config.telegram.as_ref().map(|tg| {
            Arc::new(
                TelegramChannel::new(
                    tg.bot_token.clone(),
                    tg.allowed_users.clone(),
                    tg.mention_only,
                )
                .with_streaming(tg.stream_mode, tg.draft_update_interval_ms)
                .with_multimodal(config.multimodal.clone()),
            ) as Arc<dyn Channel>
        }),
        "discord" => config.channels_config.discord.as_ref().map(|dc| {
            Arc::new(
                DiscordChannel::new(
                    dc.bot_token.clone(),
                    dc.guild_id.clone(),
                    dc.allowed_users.clone(),
                    dc.listen_to_bots,
                    dc.mention_only,
                )
                .with_multimodal(config.multimodal.clone()),
            ) as Arc<dyn Channel>
        }),
        "slack" => config.channels_config.slack.as_ref().map(|sl| {
            Arc::new(SlackChannel::new(
                sl.bot_token.clone(),
                sl.channel_id.clone(),
                sl.allowed_users.clone(),
            )) as Arc<dyn Channel>
        }),
        "mattermost" => config.channels_config.mattermost.as_ref().map(|mm| {
            Arc::new(MattermostChannel::new(
                mm.url.clone(),
                mm.bot_token.clone(),
                mm.channel_id.clone(),
                mm.allowed_users.clone(),
                mm.thread_replies.unwrap_or(true),
                mm.mention_only.unwrap_or(false),
            )) as Arc<dyn Channel>
        }),
        _ => None,
    }
}

/// One-time, operator-facing warnings about channel config that is set but
/// ignored. Call from the operator paths (channel-server startup, doctor) — NOT
/// from cron delivery, which must not re-log on every scheduled run.
pub(crate) fn warn_unused_channel_config(config: &Config) {
    if let Some(ref sl) = config.channels_config.slack {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config whose `[multimodal]` limits are deliberately NOT the defaults,
    /// so a channel built without `with_multimodal` is distinguishable from one
    /// built with it.
    fn config_with_whatsapp_cloud() -> Config {
        let mut config = Config::default();
        config.multimodal.max_images = 7;
        config.channels_config.whatsapp = Some(crate::config::schema::WhatsAppConfig {
            access_token: Some("t".into()),
            phone_number_id: Some("p".into()),
            verify_token: Some("v".into()),
            app_secret: None,
            session_path: None,
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec![],
        });
        config
    }

    #[test]
    fn the_whatsapp_builder_applies_the_operators_multimodal_caps() {
        let config = config_with_whatsapp_cloud();
        let channel = build_whatsapp_cloud(&config).expect("cloud config builds");
        assert_eq!(
            channel.multimodal().max_images,
            7,
            "the operator's caps must reach the channel, not the struct default"
        );
    }

    /// Linq carried the identical drift — the factory applied the caps, the
    /// gateway did not. One test per channel that has caps, not one per change:
    /// the contract is "every webhook channel with multimodal limits gets the
    /// operator's", and asserting it for WhatsApp alone would leave Linq free to
    /// regress. Nextcloud Talk carries no multimodal config on either path, so
    /// there is nothing to assert for it.
    #[test]
    fn the_linq_builder_applies_the_operators_multimodal_caps() {
        let mut config = Config::default();
        config.multimodal.max_images = 7;
        config.channels_config.linq = Some(crate::config::schema::LinqConfig {
            api_token: "t".into(),
            from_phone: "p".into(),
            signing_secret: None,
            allowed_senders: vec![],
        });
        let channel = build_linq(&config).expect("linq config builds");
        assert_eq!(channel.multimodal().max_images, 7);
    }

    #[test]
    fn the_whatsapp_builder_refuses_a_config_that_is_not_cloud() {
        let mut config = config_with_whatsapp_cloud();
        // Drop the one field `is_cloud_config` needs; the webhook path has
        // nothing to talk to without it.
        config
            .channels_config
            .whatsapp
            .as_mut()
            .expect("set")
            .phone_number_id = None;
        assert!(build_whatsapp_cloud(&config).is_none());
    }

    fn config_with_telegram() -> Config {
        let mut config = Config::default();
        config.channels_config.telegram = Some(crate::config::schema::TelegramConfig {
            bot_token: "placeholder-token".to_string(),
            allowed_users: vec![],
            stream_mode: crate::config::schema::StreamMode::default(),
            draft_update_interval_ms: 1_000,
            interrupt_on_new_message: false,
            mention_only: false,
        });
        config
    }

    #[test]
    fn build_one_returns_the_configured_announce_channel() {
        let config = config_with_telegram();
        assert!(build_one(&config, "telegram").is_some());
        // A configured-but-not-requested channel and unknown keys return None.
        assert!(build_one(&config, "discord").is_none());
        assert!(build_one(&config, "nope").is_none());
    }

    #[test]
    fn build_one_covers_every_announce_gate_key() {
        // build_one must recognize every key channel_supports_announce_delivery
        // allows, else a valid delivery target would fail to construct. Not
        // configured here, so each returns None — but the key is recognized.
        let config = Config::default();
        for key in ["telegram", "discord", "slack", "mattermost"] {
            assert!(
                crate::channels::channel_supports_announce_delivery(key),
                "{key} must be an announce channel"
            );
            assert!(build_one(&config, key).is_none());
        }
    }
}
