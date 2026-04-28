//! Channels setup section — wraps the existing channel-config wizard
//! in `crate::onboard::wizard::setup_channels`.
//!
//! Like `ProviderSection`, this is intentionally a thin adapter over the
//! existing 1.5k-line interview helper; lifting the body wholesale is
//! Wave 4+ scope (the spec breaks `channels` into per-platform subsections).
//!
//! `is_already_configured` returns `true` iff at least one non-CLI channel
//! is populated — the user is past the "no channels yet" state and we
//! shouldn't re-prompt by default.

use anyhow::Result;

use super::{SetupContext, SetupSection};
use crate::config::{ChannelsConfig, Config};
use crate::onboard::wizard;
use crate::profile::Profile;

pub struct ChannelsSection;

impl SetupSection for ChannelsSection {
    fn name(&self) -> &'static str {
        "channels"
    }

    fn description(&self) -> &'static str {
        "How RantaiClaw talks to you (Telegram / Discord / Slack / …)"
    }

    fn is_already_configured(&self, _profile: &Profile, config: &Config) -> bool {
        any_channel_set(&config.channels_config)
    }

    fn run(&self, ctx: &mut SetupContext) -> Result<()> {
        if !ctx.interactive {
            eprintln!("{}", self.headless_hint());
            return Ok(());
        }
        ctx.config.channels_config = wizard::setup_channels()?;
        Ok(())
    }

    fn headless_hint(&self) -> &'static str {
        "rantaiclaw channel add <platform> '<json>'  # see `rantaiclaw channel --help`"
    }
}

/// Returns `true` if any non-CLI channel has at least one configuration
/// block populated. CLI + webhook are bundled defaults and are not
/// evidence of user-driven configuration.
fn any_channel_set(c: &ChannelsConfig) -> bool {
    c.telegram.is_some()
        || c.discord.is_some()
        || c.slack.is_some()
        || c.mattermost.is_some()
        || c.imessage.is_some()
        || c.matrix.is_some()
        || c.signal.is_some()
        || c.whatsapp.is_some()
        || c.email.is_some()
        || c.irc.is_some()
        || c.lark.is_some()
        || c.dingtalk.is_some()
        || c.linq.is_some()
        || c.qq.is_some()
        || c.nextcloud_talk.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let s = ChannelsSection;
        assert_eq!(s.name(), "channels");
        assert!(!s.description().is_empty());
        assert!(s.headless_hint().contains("rantaiclaw channel"));
    }

    #[test]
    fn empty_config_is_not_configured() {
        let s = ChannelsSection;
        let dummy = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp/_rt_test"),
        };
        let cfg = Config::default();
        assert!(!s.is_already_configured(&dummy, &cfg));
    }
}
