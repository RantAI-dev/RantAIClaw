//! Per-channel auth probe — non-network configuration sanity pass.

use async_trait::async_trait;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext, Severity};

pub struct ChannelsAuthCheck;

#[async_trait]
impl DoctorCheck for ChannelsAuthCheck {
    fn name(&self) -> &'static str { "channels.auth" }
    fn category(&self) -> &'static str { "live" }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let summary = inspect_channels(&ctx.config);
        match summary.severity {
            Severity::Ok => CheckResult::ok(self.name(), summary.message).with_category(self.category()),
            Severity::Warn => CheckResult::warn(self.name(), summary.message)
                .with_category(self.category())
                .with_hint("run: rantaiclaw channel doctor"),
            Severity::Fail => CheckResult::fail(self.name(), summary.message)
                .with_category(self.category())
                .with_hint("run: rantaiclaw channel doctor"),
            Severity::Info => CheckResult::info(self.name(), summary.message).with_category(self.category()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChannelSummary {
    pub severity: Severity,
    pub message: String,
}

pub fn inspect_channels(config: &crate::config::Config) -> ChannelSummary {
    let cc = &config.channels_config;
    let mut configured: Vec<&str> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();

    if let Some(c) = cc.telegram.as_ref() {
        if !c.bot_token.trim().is_empty() { configured.push("telegram") } else { missing.push("telegram") }
    }
    if let Some(c) = cc.discord.as_ref() {
        if !c.bot_token.trim().is_empty() { configured.push("discord") } else { missing.push("discord") }
    }
    if let Some(c) = cc.slack.as_ref() {
        if !c.bot_token.trim().is_empty() { configured.push("slack") } else { missing.push("slack") }
    }

    let n_total = configured.len() + missing.len();
    if n_total == 0 {
        return ChannelSummary { severity: Severity::Info, message: "no channels configured".to_string() };
    }
    if !missing.is_empty() {
        let names = missing.join(", ");
        return ChannelSummary { severity: Severity::Fail, message: format!("channels with missing credentials: {names}") };
    }
    let names = configured.join(", ");
    ChannelSummary { severity: Severity::Ok, message: format!("{} channel(s) ready: {}", configured.len(), names) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn telegram_with_token(token: &str) -> crate::config::TelegramConfig {
        crate::config::TelegramConfig {
            bot_token: token.into(),
            allowed_users: vec![],
            stream_mode: crate::config::StreamMode::default(),
            draft_update_interval_ms: 1000,
            interrupt_on_new_message: false,
            mention_only: false,
        }
    }

    #[test]
    fn no_channels_returns_info() {
        let cfg = Config::default();
        let s = inspect_channels(&cfg);
        assert_eq!(s.severity, Severity::Info);
    }

    #[test]
    fn missing_token_returns_fail() {
        let mut cfg = Config::default();
        cfg.channels_config.telegram = Some(telegram_with_token(""));
        let s = inspect_channels(&cfg);
        assert_eq!(s.severity, Severity::Fail);
    }

    #[test]
    fn populated_token_returns_ok() {
        let mut cfg = Config::default();
        cfg.channels_config.telegram = Some(telegram_with_token("abc:123"));
        let s = inspect_channels(&cfg);
        assert_eq!(s.severity, Severity::Ok);
        assert!(s.message.contains("telegram"));
    }
}
