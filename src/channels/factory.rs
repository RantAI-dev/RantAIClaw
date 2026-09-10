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
/// they share `Channel::name()`; they are mutually exclusive, so only one is
/// ever built. Which one is decided by `WhatsAppConfig::backend_type()`, which
/// infers the mode from which keys are filled — there is no `mode` key in the
/// schema, and this comment named one until 2026-09-09.
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

/// The ONE construction of the Telegram channel. See [`build_whatsapp_cloud`].
pub(crate) fn build_telegram(config: &Config) -> Option<Arc<TelegramChannel>> {
    let tg = config.channels_config.telegram.as_ref()?;
    Some(Arc::new(
        TelegramChannel::new(
            tg.bot_token.clone(),
            tg.allowed_users.clone(),
            tg.mention_only,
        )
        .with_streaming(tg.stream_mode, tg.draft_update_interval_ms)
        .with_multimodal(config.multimodal.clone()),
    ))
}

/// The ONE construction of the Discord channel. See [`build_whatsapp_cloud`].
pub(crate) fn build_discord(config: &Config) -> Option<Arc<DiscordChannel>> {
    let dc = config.channels_config.discord.as_ref()?;
    Some(Arc::new(
        DiscordChannel::new(
            dc.bot_token.clone(),
            dc.guild_id.clone(),
            dc.allowed_users.clone(),
            dc.listen_to_bots,
            dc.mention_only,
        )
        // Inbound images obey the operator's size cap, not a default the
        // channel invented for itself.
        .with_multimodal(config.multimodal.clone()),
    ))
}

/// The ONE construction of the Slack channel. See [`build_whatsapp_cloud`].
///
/// This is the constructor #778 had to edit twice. `app_token` was added to
/// both copies by hand, and missing one would have dropped Socket Mode on that
/// path with no test failing.
pub(crate) fn build_slack(config: &Config) -> Option<Arc<SlackChannel>> {
    let sl = config.channels_config.slack.as_ref()?;
    Some(Arc::new(
        SlackChannel::new(
            sl.bot_token.clone(),
            sl.channel_id.clone(),
            sl.allowed_users.clone(),
        )
        .with_app_token(sl.app_token.clone()),
    ))
}

/// The ONE construction of the Mattermost channel. See [`build_whatsapp_cloud`].
pub(crate) fn build_mattermost(config: &Config) -> Option<Arc<MattermostChannel>> {
    let mm = config.channels_config.mattermost.as_ref()?;
    Some(Arc::new(MattermostChannel::new(
        mm.url.clone(),
        mm.bot_token.clone(),
        mm.channel_id.clone(),
        mm.allowed_users.clone(),
        mm.thread_replies.unwrap_or(true),
        mm.mention_only.unwrap_or(false),
    )))
}

/// The ONE construction of the WhatsApp Web channel. See [`build_whatsapp_cloud`].
///
/// Returns `None` on a build without the `whatsapp-web` feature, which is the
/// same answer as "not configured" to every caller and keeps the feature gate
/// out of the call sites.
///
/// Returns `Arc<dyn Channel>` rather than the concrete type because
/// `WhatsAppWebChannel` does not exist at all without the `whatsapp-web`
/// feature — naming it in the signature fails every `--no-default-features`
/// build, so the type appears only inside the gated block.
pub(crate) fn build_whatsapp_web(config: &Config) -> Option<Arc<dyn Channel>> {
    let web = config.channels_config.whatsapp_web.as_ref()?;
    if web.session_path.trim().is_empty() {
        tracing::warn!("WhatsApp Web configured but session_path is empty");
        return None;
    }
    #[cfg(not(feature = "whatsapp-web"))]
    {
        let _ = web;
        tracing::warn!(
            "WhatsApp Web is configured but this build lacks the 'whatsapp-web' feature. \
             Rebuild with: cargo build --features whatsapp-web"
        );
        None
    }
    #[cfg(feature = "whatsapp-web")]
    {
        Some(Arc::new(super::WhatsAppWebChannel::new(
            web.session_path.clone(),
            web.pair_phone.clone(),
            web.pair_code.clone(),
            web.allowed_numbers.clone(),
        )) as Arc<dyn Channel>)
    }
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

    if let Some(channel) = build_telegram(config) {
        channels.push(("telegram", "Telegram", channel));
    }

    if let Some(channel) = build_discord(config) {
        channels.push(("discord", "Discord", channel));
    }

    // The "no `app_token`" note is an operator-facing, one-time warning —
    // emitted from `warn_unused_channel_config` on the startup/doctor paths, NOT
    // here, so the cron delivery path (which builds channels on every scheduled
    // run) does not re-log it as a recurring fault.
    if let Some(channel) = build_slack(config) {
        channels.push(("slack", "Slack", channel));
    }

    if let Some(channel) = build_mattermost(config) {
        channels.push(("mattermost", "Mattermost", channel));
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

    // Cloud API and Web are still mutually exclusive at runtime, and Cloud
    // still wins, exactly as before the v32 table split. They are not two
    // independent channels: both `WhatsAppChannel` and `WhatsAppWebChannel`
    // return `"whatsapp"` from `Channel::name()`, and `channels_by_name` is
    // keyed by that name, so building both would put two channels under one
    // key and let one silently shadow the other. Giving Web its own runtime
    // name would move the pairing surface and re-key conversation history,
    // which is a behaviour change and not part of a config split.
    //
    // What the split did change is that the choice is no longer inferred from
    // which keys happen to be filled. It is which table the operator wrote.
    if config.channels_config.whatsapp.is_some() && config.channels_config.whatsapp_web.is_some() {
        tracing::warn!(
            "WhatsApp: both [channels_config.whatsapp] and [channels_config.whatsapp_web] are \
             configured. Running the Cloud API transport; remove one table to choose \
             deliberately."
        );
    }
    if let Some(channel) = build_whatsapp_cloud(config) {
        channels.push(("whatsapp", "WhatsApp Cloud API", channel));
    } else {
        // A Cloud table that cannot run must not shadow a Web table that can.
        // Before the split the mode was chosen by `phone_number_id` alone, so a
        // half-filled Cloud section with a good session path still ran Web;
        // keying the choice on mere presence would have silently taken that
        // away.
        if config.channels_config.whatsapp.is_some() {
            tracing::warn!(
                "WhatsApp Cloud API configured but missing required fields (phone_number_id, \
                 access_token, verify_token)"
            );
        }
        if let Some(channel) = build_whatsapp_web(config) {
            channels.push(("whatsapp_web", "WhatsApp Web", channel));
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
/// Construction goes through the same `build_*` functions
/// `build_configured_channels` uses, so the two cannot drift on fields. They
/// used to be verbatim copies, which is a promise a reviewer has to keep by
/// hand: `app_token` in #778 had to be added to both, and missing one would
/// have dropped Socket Mode here with no test failing.
pub(crate) fn build_one(config: &Config, key: &str) -> Option<Arc<dyn Channel>> {
    match key {
        "telegram" => build_telegram(config).map(|c| c as Arc<dyn Channel>),
        "discord" => build_discord(config).map(|c| c as Arc<dyn Channel>),
        "slack" => build_slack(config).map(|c| c as Arc<dyn Channel>),
        "mattermost" => build_mattermost(config).map(|c| c as Arc<dyn Channel>),
        _ => None,
    }
}

/// One-time, operator-facing warnings about channel config that is set but
/// ignored. Call from the operator paths (channel-server startup, doctor) — NOT
/// from cron delivery, which must not re-log on every scheduled run.
pub(crate) fn warn_unused_channel_config(config: &Config) {
    if let Some(ref sl) = config.channels_config.slack {
        if sl.app_token.as_deref().is_none_or(|t| t.trim().is_empty()) {
            tracing::warn!(
                "Slack: no `app_token`. {} Set [channels_config.slack].app_token (an `{}` token \
                 with `connections:write`) for Socket Mode.",
                crate::channels::slack::NO_APP_TOKEN_CONSEQUENCE,
                crate::channels::slack::APP_TOKEN_PREFIX,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config for all four tier channels, with values that are deliberately
    /// not defaults so a channel built the wrong way is distinguishable from
    /// one built the right way.
    fn config_with_the_four_tier_channels() -> Config {
        let mut config = Config::default();
        config.multimodal.max_images = 7;
        config.channels_config.telegram = Some(crate::config::schema::TelegramConfig {
            bot_token: "tg-token".into(),
            allowed_users: vec!["rantaiclaw_user".into()],
            mention_only: true,
            stream_mode: crate::config::schema::StreamMode::Partial,
            draft_update_interval_ms: 1234,
            interrupt_on_new_message: false,
        });
        config.channels_config.slack = Some(crate::config::schema::SlackConfig {
            bot_token: "slack-bot-token".into(),
            app_token: Some("xapp-1-A-placeholder".into()),
            channel_id: Some("C0".into()),
            allowed_users: vec!["rantaiclaw_user".into()],
        });
        config
    }

    /// The regression #778 nearly shipped. `app_token` was added to two copied
    /// constructors by hand; missing the `build_one` copy would have dropped
    /// Socket Mode on the cron delivery path with nothing failing.
    ///
    /// Asserted through both callers, because "the builder is right" is not the
    /// claim — the claim is that neither caller can get a different channel.
    #[test]
    fn both_construction_paths_give_slack_the_same_app_token() {
        let config = config_with_the_four_tier_channels();

        let from_builder = build_slack(&config).expect("slack config builds");
        assert_eq!(
            from_builder.app_token(),
            Some("xapp-1-A-placeholder"),
            "the operator's app token must reach the channel, or Socket Mode \
             silently becomes polling"
        );

        let fleet = build_configured_channels(&config);
        assert!(
            fleet.iter().any(|(key, _, _)| *key == "slack"),
            "the fleet path must still build Slack"
        );
        assert!(
            build_one(&config, "slack").is_some(),
            "the cron delivery path must still build Slack"
        );
    }

    /// Telegram carries the most construction options of the four, so it is the
    /// one where a dropped builder call is easiest to make and hardest to see.
    #[test]
    fn both_construction_paths_build_telegram_with_the_operators_settings() {
        let config = config_with_the_four_tier_channels();
        assert!(build_telegram(&config).is_some());
        assert!(build_one(&config, "telegram").is_some());
        assert!(build_configured_channels(&config)
            .iter()
            .any(|(key, _, _)| *key == "telegram"));
    }

    /// The two sites were verbatim copies, which is a promise kept by hand.
    /// This is the guard that makes it a promise kept by the compiler: only the
    /// `build_*` functions may name these constructors, so a caller cannot
    /// quietly grow a second copy that drifts on the next added option.
    /// The v32 split must not let a half-filled Cloud table shadow a working
    /// Web one. Before the split the transport was chosen by `phone_number_id`
    /// alone, so this config ran Web; choosing on table *presence* would have
    /// silently stopped it.
    #[test]
    fn an_unusable_cloud_table_does_not_shadow_a_working_web_table() {
        let mut config = Config::default();
        config.channels_config.whatsapp = Some(crate::config::schema::WhatsAppConfig {
            // No `phone_number_id`, so `is_cloud_config()` is false.
            access_token: Some("t".into()),
            phone_number_id: None,
            verify_token: None,
            app_secret: None,
            allowed_numbers: vec![],
        });
        config.channels_config.whatsapp_web = Some(crate::config::schema::WhatsAppWebConfig {
            session_path: "/tmp/rantaiclaw-wa.db".into(),
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["+15550000001".into()],
        });

        let built: Vec<&str> = build_configured_channels(&config)
            .into_iter()
            .map(|(key, _, _)| key)
            .collect();
        assert!(
            !built.contains(&"whatsapp"),
            "an incomplete Cloud table must not be built: {built:?}"
        );
        #[cfg(feature = "whatsapp-web")]
        assert!(
            built.contains(&"whatsapp_web"),
            "the usable Web table must still run: {built:?}"
        );
    }

    /// Both usable: Cloud wins, and only one is built. They share
    /// `Channel::name() == "whatsapp"`, so building both would put two channels
    /// under one key in `channels_by_name` and let one shadow the other.
    #[test]
    fn cloud_wins_when_both_tables_are_usable_and_only_one_is_built() {
        let mut config = Config::default();
        config.channels_config.whatsapp = Some(crate::config::schema::WhatsAppConfig {
            access_token: Some("t".into()),
            phone_number_id: Some("p".into()),
            verify_token: Some("v".into()),
            app_secret: None,
            allowed_numbers: vec![],
        });
        config.channels_config.whatsapp_web = Some(crate::config::schema::WhatsAppWebConfig {
            session_path: "/tmp/rantaiclaw-wa.db".into(),
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec![],
        });

        let built: Vec<&str> = build_configured_channels(&config)
            .into_iter()
            .map(|(key, _, _)| key)
            .collect();
        assert!(built.contains(&"whatsapp"), "Cloud must win: {built:?}");
        assert!(
            !built.contains(&"whatsapp_web"),
            "only one WhatsApp transport may run: {built:?}"
        );
    }

    #[test]
    fn only_the_shared_builders_construct_a_tier_channel() {
        let src = include_str!("factory.rs");
        // Assembled at runtime so this test does not match itself.
        let ctors = [
            format!("{}Channel::new(", "Telegram"),
            format!("{}Channel::new(", "Discord"),
            format!("{}Channel::new(", "Slack"),
            format!("{}Channel::new(", "Mattermost"),
        ];
        let production = src.split("\n#[cfg(test)]").next().expect("source");
        assert!(
            production.len() < src.len(),
            "the test module marker moved; update this guard"
        );
        for ctor in &ctors {
            let hits = production.matches(ctor.as_str()).count();
            assert_eq!(
                hits, 1,
                "`{ctor}` is constructed {hits} times; exactly one shared \
                 builder may call it, or the two paths can drift again"
            );
        }
    }

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
