//! Per-channel health probe — bucketed `live` because it actually round-trips
//! against the platform APIs (offline mode degrades to a config-sanity scan).
//!
//! What this check verifies, per channel:
//!
//! * **Telegram** — `GET https://api.telegram.org/bot<token>/getMe`. 200+success
//!   means the token is live and points at a bot account.
//! * **Discord** — `GET https://discord.com/api/v10/users/@me` with `Bot <token>`.
//!   200+username confirms the token works against the real Discord API.
//! * **Slack** — `GET https://slack.com/api/auth.test` with `Bearer <token>`.
//!   200+`ok=true`+`team` confirms the bot token is installed in a workspace.
//! * **WhatsApp Cloud API** — `GET https://graph.facebook.com/v18.0/<phone_id>`
//!   with `Bearer <access_token>`. 200 confirms the phone-number id resolves
//!   under the configured token.
//! * **WhatsApp Web** — non-network: confirm `session_path` exists and is
//!   non-empty. Without that file the daemon has to re-pair on next run.
//!
//! All probes are wrapped in a 5s timeout per channel — the doctor command
//! prioritises completing quickly over thoroughness.
//!
//! When `ctx.offline` is true the probes are skipped and the check falls
//! back to the synchronous config-sanity pass (`inspect_channels`).

use std::time::Duration;

use async_trait::async_trait;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext, Severity};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ChannelsAuthCheck;

#[async_trait]
impl DoctorCheck for ChannelsAuthCheck {
    fn name(&self) -> &'static str {
        "channels.auth"
    }
    fn category(&self) -> &'static str {
        "live"
    }

    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        if ctx.offline {
            // Config-only sanity — same shape as before, just bucketed
            // honestly so users understand what the check did.
            let summary = inspect_channels(&ctx.config);
            return summarize(self.name(), self.category(), &summary);
        }

        let summary = probe_channels(&ctx.config).await;
        summarize(self.name(), self.category(), &summary)
    }
}

fn summarize(name: &'static str, category: &'static str, summary: &ChannelSummary) -> CheckResult {
    match summary.severity {
        Severity::Ok => CheckResult::ok(name, summary.message.clone()).with_category(category),
        Severity::Warn => CheckResult::warn(name, summary.message.clone())
            .with_category(category)
            .with_hint("run: rantaiclaw channel doctor"),
        Severity::Fail => CheckResult::fail(name, summary.message.clone())
            .with_category(category)
            .with_hint("run: rantaiclaw channel doctor"),
        Severity::Info => CheckResult::info(name, summary.message.clone()).with_category(category),
    }
}

#[derive(Debug, Clone)]
pub struct ChannelSummary {
    pub severity: Severity,
    pub message: String,
}

/// Synchronous config-only sanity. Used for `--offline` doctor runs and
/// kept around so callers that want to skip the network can still get a
/// rough summary. Does NOT prove that any channel actually authenticates.
pub fn inspect_channels(config: &crate::config::Config) -> ChannelSummary {
    let cc = &config.channels_config;
    let mut configured: Vec<&str> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();

    macro_rules! check_token {
        ($name:literal, $opt:expr, $field:ident) => {
            if let Some(c) = $opt.as_ref() {
                if c.$field.trim().is_empty() {
                    missing.push($name);
                } else {
                    configured.push($name);
                }
            }
        };
    }
    check_token!("telegram", cc.telegram, bot_token);
    check_token!("discord", cc.discord, bot_token);
    check_token!("slack", cc.slack, bot_token);

    if let Some(c) = cc.whatsapp.as_ref() {
        // WhatsApp's "credential" depends on which mode is in use.
        let cloud_ok = c
            .access_token
            .as_deref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false);
        let web_ok = c
            .session_path
            .as_deref()
            .map(|p| !p.trim().is_empty())
            .unwrap_or(false);
        if cloud_ok || web_ok {
            configured.push("whatsapp");
        } else {
            missing.push("whatsapp");
        }
    }

    let n_total = configured.len() + missing.len();
    if n_total == 0 {
        return ChannelSummary {
            severity: Severity::Info,
            message: "no channels configured".to_string(),
        };
    }
    if !missing.is_empty() {
        return ChannelSummary {
            severity: Severity::Fail,
            message: format!("channels with missing credentials: {}", missing.join(", ")),
        };
    }
    ChannelSummary {
        severity: Severity::Ok,
        message: format!(
            "{} channel(s) configured: {}",
            configured.len(),
            configured.join(", ")
        ),
    }
}

/// Network-probing version. Skipped under `ctx.offline`.
pub async fn probe_channels(config: &crate::config::Config) -> ChannelSummary {
    let cc = &config.channels_config;
    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return ChannelSummary {
                severity: Severity::Fail,
                message: format!("could not build http client: {e}"),
            };
        }
    };

    let mut ok: Vec<String> = Vec::new();
    let mut bad: Vec<String> = Vec::new();
    let mut warn: Vec<String> = Vec::new();

    if let Some(c) = cc.telegram.as_ref() {
        match probe_telegram(&client, &c.bot_token).await {
            Ok(name) => ok.push(format!("telegram (@{name})")),
            Err(e) => bad.push(format!("telegram: {e}")),
        }
    }
    if let Some(c) = cc.discord.as_ref() {
        match probe_discord(&client, &c.bot_token).await {
            Ok(name) => ok.push(format!("discord ({name})")),
            Err(e) => bad.push(format!("discord: {e}")),
        }
    }
    if let Some(c) = cc.slack.as_ref() {
        match probe_slack(&client, &c.bot_token).await {
            Ok(team) => ok.push(format!("slack ({team})")),
            Err(e) => bad.push(format!("slack: {e}")),
        }
    }
    if let Some(c) = cc.whatsapp.as_ref() {
        // Cloud API path: usable when access_token + phone_number_id are both set.
        let cloud_usable = matches!(
            (c.access_token.as_deref(), c.phone_number_id.as_deref()),
            (Some(token), Some(phone_id)) if !token.trim().is_empty() && !phone_id.trim().is_empty()
        );
        // Web path: usable when a non-empty session DB path is configured.
        let web_usable = c
            .session_path
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty());

        if cloud_usable {
            // Safe to unwrap the pair — `cloud_usable` proved both are present.
            let token = c.access_token.as_deref().unwrap_or_default();
            let phone_id = c.phone_number_id.as_deref().unwrap_or_default();
            match probe_whatsapp_cloud(&client, token, phone_id).await {
                Ok(()) => ok.push("whatsapp (cloud-api)".to_string()),
                Err(e) => bad.push(format!("whatsapp cloud: {e}")),
            }
        }
        if web_usable {
            let path = c.session_path.as_deref().unwrap_or_default();
            match probe_whatsapp_web(path) {
                ProbeWebResult::Ok => ok.push("whatsapp (web)".to_string()),
                ProbeWebResult::SessionMissing => {
                    warn.push(
                        "whatsapp web: no session — needs QR pairing on next run".to_string(),
                    );
                }
                ProbeWebResult::SessionPathBad(e) => bad.push(format!("whatsapp web session: {e}")),
            }
        }
        // A configured WhatsApp block with neither a usable cloud pair nor a
        // session path is broken, not absent — the offline `inspect_channels`
        // already fails it, so the online probe must too (they used to disagree,
        // and it read as "no channels configured" when it was the only channel).
        if !cloud_usable && !web_usable {
            bad.push(
                "whatsapp: incomplete credentials — set access_token + phone_number_id (cloud) \
                 or session_path (web)"
                    .to_string(),
            );
        }
    }

    let n_total = ok.len() + bad.len() + warn.len();
    if n_total == 0 {
        return ChannelSummary {
            severity: Severity::Info,
            message: "no channels configured".to_string(),
        };
    }
    if !bad.is_empty() {
        return ChannelSummary {
            severity: Severity::Fail,
            message: format!("channel auth failures: {}", bad.join("; ")),
        };
    }
    if !warn.is_empty() {
        return ChannelSummary {
            severity: Severity::Warn,
            message: format!(
                "{} ready ({}); needs attention: {}",
                ok.len(),
                ok.join(", "),
                warn.join("; ")
            ),
        };
    }
    ChannelSummary {
        severity: Severity::Ok,
        message: format!("{} channel(s) ready: {}", ok.len(), ok.join(", ")),
    }
}

async fn probe_telegram(client: &reqwest::Client, token: &str) -> Result<String, String> {
    if token.trim().is_empty() {
        return Err("token is empty".into());
    }
    // Telegram carries the bot token in the URL path, so a reqwest error's
    // `Display` would leak it into the rendered doctor output. `without_url()`
    // strips the URL from the error before it is formatted.
    let url = format!("https://api.telegram.org/bot{token}/getMe");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("network: {}", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("decode: {}", e.without_url()))?;
    Ok(body
        .get("result")
        .and_then(|r| r.get("username"))
        .and_then(|u| u.as_str())
        .unwrap_or("unknown")
        .to_string())
}

async fn probe_discord(client: &reqwest::Client, token: &str) -> Result<String, String> {
    if token.trim().is_empty() {
        return Err("token is empty".into());
    }
    let resp = client
        .get("https://discord.com/api/v10/users/@me")
        .header("Authorization", format!("Bot {token}"))
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    Ok(body
        .get("username")
        .and_then(|u| u.as_str())
        .unwrap_or("unknown")
        .to_string())
}

async fn probe_slack(client: &reqwest::Client, token: &str) -> Result<String, String> {
    if token.trim().is_empty() {
        return Err("token is empty".into());
    }
    let resp = client
        .get("https://slack.com/api/auth.test")
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(format!("slack-side: {err}"));
    }
    Ok(body
        .get("team")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown-workspace")
        .to_string())
}

async fn probe_whatsapp_cloud(
    client: &reqwest::Client,
    token: &str,
    phone_id: &str,
) -> Result<(), String> {
    let url = format!("https://graph.facebook.com/v18.0/{phone_id}");
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(())
}

enum ProbeWebResult {
    Ok,
    SessionMissing,
    SessionPathBad(String),
}

fn probe_whatsapp_web(path: &str) -> ProbeWebResult {
    let expanded = shellexpand::tilde(path).to_string();
    let p = std::path::Path::new(&expanded);
    match std::fs::metadata(p) {
        Ok(m) if m.is_file() && m.len() > 0 => ProbeWebResult::Ok,
        Ok(_) => ProbeWebResult::SessionMissing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProbeWebResult::SessionMissing,
        Err(e) => ProbeWebResult::SessionPathBad(format!("{e}")),
    }
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

    #[test]
    fn whatsapp_with_session_path_counts_as_configured() {
        let mut cfg = Config::default();
        cfg.channels_config.whatsapp = Some(crate::config::schema::WhatsAppConfig {
            access_token: None,
            phone_number_id: None,
            verify_token: None,
            app_secret: None,
            session_path: Some("~/.rantaiclaw/state/whatsapp-web/session.db".into()),
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["*".into()],
        });
        let s = inspect_channels(&cfg);
        // No telegram/discord/slack — so wa is the only one. Marked OK
        // (config-only) because session_path is set.
        assert_eq!(s.severity, Severity::Ok);
        assert!(s.message.contains("whatsapp"));
    }

    #[test]
    fn whatsapp_with_no_credentials_returns_fail() {
        let mut cfg = Config::default();
        cfg.channels_config.whatsapp = Some(crate::config::schema::WhatsAppConfig {
            access_token: None,
            phone_number_id: None,
            verify_token: None,
            app_secret: None,
            session_path: None,
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["*".into()],
        });
        let s = inspect_channels(&cfg);
        assert_eq!(s.severity, Severity::Fail);
        assert!(s.message.contains("whatsapp"));
    }

    #[tokio::test]
    async fn online_whatsapp_incomplete_fails() {
        // The ONLINE probe must agree with offline `inspect_channels`: a
        // configured-but-incomplete WhatsApp block Fails, rather than reading
        // "no channels configured" when it is the only channel. Both cloud and
        // web are unusable here, so no network request is made.
        let mut cfg = Config::default();
        cfg.channels_config.whatsapp = Some(crate::config::schema::WhatsAppConfig {
            access_token: None,
            phone_number_id: None,
            verify_token: None,
            app_secret: None,
            session_path: None,
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["*".into()],
        });
        let s = probe_channels(&cfg).await;
        assert_eq!(s.severity, Severity::Fail, "{}", s.message);
        assert!(s.message.contains("whatsapp"), "{}", s.message);
    }

    #[test]
    fn whatsapp_web_session_present() {
        // Real file → Ok.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.db");
        std::fs::write(&path, b"some bytes").unwrap();
        match probe_whatsapp_web(path.to_str().unwrap()) {
            ProbeWebResult::Ok => {}
            ProbeWebResult::SessionMissing => panic!("file with bytes should not be Missing"),
            ProbeWebResult::SessionPathBad(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn whatsapp_web_session_missing() {
        match probe_whatsapp_web("/nonexistent/path/to/session.db") {
            ProbeWebResult::SessionMissing => {}
            ProbeWebResult::Ok => panic!("missing file should not be Ok"),
            ProbeWebResult::SessionPathBad(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn whatsapp_web_session_zero_bytes_treated_as_missing() {
        // Zero-byte session DB means the daemon hasn't paired yet; flag as
        // Missing so doctor warns the user instead of pretending things
        // are fine.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.db");
        std::fs::write(&path, b"").unwrap();
        match probe_whatsapp_web(path.to_str().unwrap()) {
            ProbeWebResult::SessionMissing => {}
            ProbeWebResult::Ok => panic!("zero-byte file should be Missing"),
            ProbeWebResult::SessionPathBad(e) => panic!("unexpected error: {e}"),
        }
    }

    /// Spin up a one-shot HTTP responder for `getMe` so the Telegram
    /// probe's success-path can be exercised without the real Telegram
    /// API.
    async fn telegram_probe_ok() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let body = serde_json::json!({"ok": true, "result": {"username": "rantaibot"}})
                    .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        // We can't redirect the real probe URL in this test (it hardcodes
        // api.telegram.org). Skip the actual call here — this test is a
        // placeholder showing the mock pattern; full e2e is exercised
        // via the existing setup-wizard `getMe` integration test.
        let _ = addr;
    }

    #[tokio::test]
    async fn telegram_probe_pattern_is_callable() {
        // Smoke test that the helper compiles + runs; the network round-
        // trip belongs in an integration test that controls DNS.
        telegram_probe_ok().await;
    }
}
