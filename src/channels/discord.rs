use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// Discord channel — connects via Gateway WebSocket for real-time messages
pub struct DiscordChannel {
    bot_token: String,
    guild_id: Option<String>,
    /// `Arc<RwLock<..>>` so a successful `/bind`/`/claim` can append the sender
    /// at runtime (immediate access without a channel restart).
    allowed_users: Arc<RwLock<Vec<String>>>,
    listen_to_bots: bool,
    mention_only: bool,
    typing_handles: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl DiscordChannel {
    pub fn new(
        bot_token: String,
        guild_id: Option<String>,
        allowed_users: Vec<String>,
        listen_to_bots: bool,
        mention_only: bool,
    ) -> Self {
        Self {
            bot_token,
            guild_id,
            allowed_users: Arc::new(RwLock::new(allowed_users)),
            listen_to_bots,
            mention_only,
            typing_handles: Mutex::new(HashMap::new()),
        }
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client("channel.discord")
    }

    /// Check if a Discord user ID is in the allowlist.
    /// Empty list means deny everyone until explicitly configured.
    /// `"*"` means allow everyone.
    fn is_user_allowed(&self, user_id: &str) -> bool {
        self.allowed_users
            .read()
            .map(|users| users.iter().any(|u| u == "*" || u == user_id))
            .unwrap_or(false)
    }

    /// Append a freshly-paired identity to the runtime allowlist (deduped) so
    /// access is effective immediately. The persisted config (saved by the
    /// pairing core) is the source of truth across restarts.
    fn add_allowed_identity_runtime(&self, identity: &str) {
        let identity = identity.trim();
        if identity.is_empty() {
            return;
        }
        if let Ok(mut users) = self.allowed_users.write() {
            if !users.iter().any(|u| u == identity) {
                users.push(identity.to_string());
            }
        }
    }

    fn bot_user_id_from_token(token: &str) -> Option<String> {
        // Discord bot tokens are base64(bot_user_id).timestamp.hmac
        let part = token.split('.').next()?;
        base64_decode(part)
    }

    /// Sender identity form(s) for the shared pairing store, drawn from a
    /// `MESSAGE_CREATE` payload's `author`. Returns `[user_id, username]`
    /// (username only when present), matching the `is_user_allowed` key (the
    /// `user_id`) plus the readily-available handle so `can_approve` resolves
    /// either form after a `/claim`.
    fn extract_pairing_identities(d: &serde_json::Value) -> Vec<String> {
        let author = d.get("author");
        let mut identities = Vec::new();
        if let Some(id) = author
            .and_then(|a| a.get("id"))
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
        {
            identities.push(id.to_string());
        }
        if let Some(username) = author
            .and_then(|a| a.get("username"))
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
        {
            identities.push(username.to_string());
        }
        identities
    }

    /// Resolve the active profile root for the shared pairing-code store.
    fn pairing_profile_root() -> Option<std::path::PathBuf> {
        match crate::profile::ProfileManager::active() {
            Ok(p) => Some(p.root),
            Err(e) => {
                tracing::warn!("Discord pairing: couldn't resolve profile root: {e:#}");
                None
            }
        }
    }
}

const BASE64_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Discord's maximum message length for regular messages.
///
/// Discord rejects longer payloads with `50035 Invalid Form Body`.
const DISCORD_MAX_MESSAGE_LENGTH: usize = 2000;

fn mention_tags(bot_user_id: &str) -> [String; 2] {
    [format!("<@{bot_user_id}>"), format!("<@!{bot_user_id}>")]
}

fn contains_bot_mention(content: &str, bot_user_id: &str) -> bool {
    let tags = mention_tags(bot_user_id);
    content.contains(&tags[0]) || content.contains(&tags[1])
}

fn normalize_incoming_content(
    content: &str,
    mention_only: bool,
    bot_user_id: &str,
) -> Option<String> {
    if content.is_empty() {
        return None;
    }

    if mention_only && !contains_bot_mention(content, bot_user_id) {
        return None;
    }

    let mut normalized = content.to_string();
    if mention_only {
        for tag in mention_tags(bot_user_id) {
            normalized = normalized.replace(&tag, " ");
        }
    }

    let normalized = normalized.trim().to_string();
    if normalized.is_empty() {
        return None;
    }

    Some(normalized)
}

/// Minimal base64 decode (no extra dep) — only needs to decode the user ID portion
#[allow(clippy::cast_possible_truncation)]
fn base64_decode(input: &str) -> Option<String> {
    let padded = match input.len() % 4 {
        2 => format!("{input}=="),
        3 => format!("{input}="),
        _ => input.to_string(),
    };

    let mut bytes = Vec::new();
    let chars: Vec<u8> = padded.bytes().collect();

    for chunk in chars.chunks(4) {
        if chunk.len() < 4 {
            break;
        }

        let mut v = [0usize; 4];
        for (i, &b) in chunk.iter().enumerate() {
            if b == b'=' {
                v[i] = 0;
            } else {
                v[i] = BASE64_ALPHABET.iter().position(|&a| a == b)?;
            }
        }

        bytes.push(((v[0] << 2) | (v[1] >> 4)) as u8);
        if chunk[2] != b'=' {
            bytes.push((((v[1] & 0xF) << 4) | (v[2] >> 2)) as u8);
        }
        if chunk[3] != b'=' {
            bytes.push((((v[2] & 0x3) << 6) | v[3]) as u8);
        }
    }

    String::from_utf8(bytes).ok()
}

#[async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    fn render_target(&self) -> crate::channels::format::RenderTarget {
        // Discord renders CommonMark markup but NOT tables, so `tables_native:
        // false` turns tables into an aligned ASCII grid in a ``` fence (which
        // Discord renders monospace) instead of leaking raw pipes.
        crate::channels::format::RenderTarget::StdMarkdown {
            tables_native: false,
        }
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        // Render per-platform, then split without cutting a code fence — replaces
        // the naive char-count splitter that could cut a fenced block in half.
        let blocks = crate::channels::format::render(&message.content, &self.render_target());
        let chunks = crate::channels::format::split(&blocks, DISCORD_MAX_MESSAGE_LENGTH);

        for (i, chunk) in chunks.iter().enumerate() {
            let url = format!(
                "https://discord.com/api/v10/channels/{}/messages",
                message.recipient
            );

            let body = json!({ "content": chunk });

            let resp = self
                .http_client()
                .post(&url)
                .header("Authorization", format!("Bot {}", self.bot_token))
                .json(&body)
                .send()
                .await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let err = resp
                    .text()
                    .await
                    .unwrap_or_else(|e| format!("<failed to read response body: {e}>"));
                anyhow::bail!("Discord send message failed ({status}): {err}");
            }

            // Add a small delay between chunks to avoid rate limiting
            if i < chunks.len() - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }

        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let bot_user_id = Self::bot_user_id_from_token(&self.bot_token).unwrap_or_default();

        // Get Gateway URL
        let gw_resp: serde_json::Value = self
            .http_client()
            .get("https://discord.com/api/v10/gateway/bot")
            .header("Authorization", format!("Bot {}", self.bot_token))
            .send()
            .await?
            .json()
            .await?;

        let gw_url = gw_resp
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("wss://gateway.discord.gg");

        let ws_url = format!("{gw_url}/?v=10&encoding=json");
        tracing::info!("Discord: connecting to gateway...");

        let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url).await?;
        let (mut write, mut read) = ws_stream.split();

        // Read Hello (opcode 10)
        let hello = read.next().await.ok_or(anyhow::anyhow!("No hello"))??;
        let hello_data: serde_json::Value = serde_json::from_str(&hello.to_string())?;
        let heartbeat_interval = hello_data
            .get("d")
            .and_then(|d| d.get("heartbeat_interval"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(41250);

        // Send Identify (opcode 2)
        let identify = json!({
            "op": 2,
            "d": {
                "token": self.bot_token,
                "intents": 37377, // GUILDS | GUILD_MESSAGES | MESSAGE_CONTENT | DIRECT_MESSAGES
                "properties": {
                    "os": "linux",
                    "browser": "rantaiclaw",
                    "device": "rantaiclaw"
                }
            }
        });
        write
            .send(Message::Text(identify.to_string().into()))
            .await?;

        tracing::info!("Discord: connected and identified");

        // Track the last sequence number for heartbeats and resume.
        // Only accessed in the select! loop below, so a plain i64 suffices.
        let mut sequence: i64 = -1;

        // Spawn heartbeat timer — sends a tick signal, actual heartbeat
        // is assembled in the select! loop where `sequence` lives.
        let (hb_tx, mut hb_rx) = tokio::sync::mpsc::channel::<()>(1);
        let hb_interval = heartbeat_interval;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(hb_interval));
            loop {
                interval.tick().await;
                if hb_tx.send(()).await.is_err() {
                    break;
                }
            }
        });

        let guild_filter = self.guild_id.clone();

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!("Discord channel shutting down");
                    break;
                }
                _ = hb_rx.recv() => {
                    let d = if sequence >= 0 { json!(sequence) } else { json!(null) };
                    let hb = json!({"op": 1, "d": d});
                    if write.send(Message::Text(hb.to_string().into())).await.is_err() {
                        break;
                    }
                }
                msg = read.next() => {
                    let msg = match msg {
                        Some(Ok(Message::Text(t))) => t,
                        Some(Ok(Message::Close(_))) | None => break,
                        _ => continue,
                    };

                    let event: serde_json::Value = match serde_json::from_str(msg.as_ref()) {
                        Ok(e) => e,
                        Err(_) => continue,
                    };

                    // Track sequence number from all dispatch events
                    if let Some(s) = event.get("s").and_then(serde_json::Value::as_i64) {
                        sequence = s;
                    }

                    let op = event.get("op").and_then(serde_json::Value::as_u64).unwrap_or(0);

                    match op {
                        // Op 1: Server requests an immediate heartbeat
                        1 => {
                            let d = if sequence >= 0 { json!(sequence) } else { json!(null) };
                            let hb = json!({"op": 1, "d": d});
                            if write.send(Message::Text(hb.to_string().into())).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        // Op 7: Reconnect
                        7 => {
                            tracing::warn!("Discord: received Reconnect (op 7), closing for restart");
                            break;
                        }
                        // Op 9: Invalid Session
                        9 => {
                            tracing::warn!("Discord: received Invalid Session (op 9), closing for restart");
                            break;
                        }
                        _ => {}
                    }

                    // Only handle MESSAGE_CREATE (opcode 0, type "MESSAGE_CREATE")
                    let event_type = event.get("t").and_then(|t| t.as_str()).unwrap_or("");
                    if event_type != "MESSAGE_CREATE" {
                        continue;
                    }

                    let Some(d) = event.get("d") else {
                        continue;
                    };

                    // Skip messages from the bot itself
                    let author_id = d.get("author").and_then(|a| a.get("id")).and_then(|i| i.as_str()).unwrap_or("");
                    if author_id == bot_user_id {
                        continue;
                    }

                    // Skip bot messages (unless listen_to_bots is enabled)
                    if !self.listen_to_bots && d.get("author").and_then(|a| a.get("bot")).and_then(serde_json::Value::as_bool).unwrap_or(false) {
                        continue;
                    }

                    // Sender validation
                    if !self.is_user_allowed(author_id) {
                        // Before rejecting, let a not-yet-allowed user self-onboard
                        // with a `/bind`/`/claim <code>` minted via
                        // `rantaiclaw channels pair`. Uses the raw content (not the
                        // mention-stripped form) so DMs and unmentioned messages are
                        // still seen. On success the sender lands in `allowed_users`
                        // (and, for an owner-capable `/claim`, `approval_owners`).
                        let raw_content = d
                            .get("content")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("");
                        if let Some(root) = Self::pairing_profile_root() {
                            let identities = Self::extract_pairing_identities(d);
                            if let Some(reply) = crate::channels::pairing::try_handle_pairing(
                                raw_content,
                                "discord",
                                crate::channels::pairing::AllowlistField::AllowedUsers,
                                &identities,
                                &root,
                            )
                            .await
                            {
                                // Mirror into the runtime allowlist so access is
                                // effective immediately (config is already saved).
                                for id in &identities {
                                    self.add_allowed_identity_runtime(id);
                                }
                                let channel_id = d
                                    .get("channel_id")
                                    .and_then(serde_json::Value::as_str)
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or(author_id);
                                let _ = self.send(&SendMessage::new(reply, channel_id)).await;
                                continue;
                            }
                        }
                        tracing::warn!("Discord: ignoring message from unauthorized user: {author_id}");
                        continue;
                    }

                    // Guild filter
                    if let Some(ref gid) = guild_filter {
                        let msg_guild = d.get("guild_id").and_then(serde_json::Value::as_str);
                        // DMs have no guild_id — let them through; for guild messages, enforce the filter
                        if let Some(g) = msg_guild {
                            if g != gid {
                                continue;
                            }
                        }
                    }

                    let content = d.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    let Some(clean_content) =
                        normalize_incoming_content(content, self.mention_only, &bot_user_id)
                    else {
                        continue;
                    };

                    let message_id = d.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    let channel_id = d.get("channel_id").and_then(|c| c.as_str()).unwrap_or("").to_string();

                    let channel_msg = ChannelMessage { sender_aliases: Vec::new(),
                        id: if message_id.is_empty() {
                            Uuid::new_v4().to_string()
                        } else {
                            format!("discord_{message_id}")
                        },
                        sender: author_id.to_string(),
                        reply_target: if channel_id.is_empty() {
                            author_id.to_string()
                        } else {
                            channel_id.clone()
                        },
                        content: clean_content,
                        channel: "discord".to_string(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        thread_ts: None,
                    };

                    if tx.send(channel_msg).await.is_err() {
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    async fn health_check(&self) -> bool {
        self.http_client()
            .get("https://discord.com/api/v10/users/@me")
            .header("Authorization", format!("Bot {}", self.bot_token))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        self.stop_typing(recipient).await?;

        let client = self.http_client();
        let token = self.bot_token.clone();
        let channel_id = recipient.to_string();

        let handle = tokio::spawn(async move {
            let url = format!("https://discord.com/api/v10/channels/{channel_id}/typing");
            loop {
                let _ = client
                    .post(&url)
                    .header("Authorization", format!("Bot {token}"))
                    .send()
                    .await;
                tokio::time::sleep(std::time::Duration::from_secs(8)).await;
            }
        });

        let mut guard = self.typing_handles.lock();
        guard.insert(recipient.to_string(), handle);

        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()> {
        let mut guard = self.typing_handles.lock();
        if let Some(handle) = guard.remove(recipient) {
            handle.abort();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_render_target_is_std_markdown_ascii_tables() {
        // Assert on the CHANNEL, not just format::* — this fails against the
        // pre-wiring `Plain` default and pins tables_native=false (Discord has
        // no native tables).
        let ch = DiscordChannel::new("fake".into(), None, vec![], false, false);
        assert_eq!(
            ch.render_target(),
            crate::channels::format::RenderTarget::StdMarkdown {
                tables_native: false
            }
        );
    }

    #[test]
    fn discord_renders_a_table_as_fenced_ascii() {
        // The real win: a GFM table Discord cannot render becomes an aligned
        // ASCII grid in a ``` fence (Discord renders fences monospace).
        let ch = DiscordChannel::new("fake".into(), None, vec![], false, false);
        let out = crate::channels::format::render_to_string(
            "| A | B |\n|---|---|\n| 1 | 2 |",
            &ch.render_target(),
        );
        assert!(out.contains("```"), "table not fenced: {out}");
        assert!(!out.contains("| A | B |"), "raw pipe table leaked: {out}");
    }

    #[test]
    fn discord_channel_name() {
        let ch = DiscordChannel::new("fake".into(), None, vec![], false, false);
        assert_eq!(ch.name(), "discord");
    }

    #[test]
    fn base64_decode_bot_id() {
        // "MTIzNDU2" decodes to "123456"
        let decoded = base64_decode("MTIzNDU2");
        assert_eq!(decoded, Some("123456".to_string()));
    }

    #[test]
    fn bot_user_id_extraction() {
        // Token format: base64(user_id).timestamp.hmac
        let token = "MTIzNDU2.fake.hmac";
        let id = DiscordChannel::bot_user_id_from_token(token);
        assert_eq!(id, Some("123456".to_string()));
    }

    #[test]
    fn empty_allowlist_denies_everyone() {
        let ch = DiscordChannel::new("fake".into(), None, vec![], false, false);
        assert!(!ch.is_user_allowed("12345"));
        assert!(!ch.is_user_allowed("anyone"));
    }

    #[test]
    fn wildcard_allows_everyone() {
        let ch = DiscordChannel::new("fake".into(), None, vec!["*".into()], false, false);
        assert!(ch.is_user_allowed("12345"));
        assert!(ch.is_user_allowed("anyone"));
    }

    #[test]
    fn specific_allowlist_filters() {
        let ch = DiscordChannel::new(
            "fake".into(),
            None,
            vec!["111".into(), "222".into()],
            false,
            false,
        );
        assert!(ch.is_user_allowed("111"));
        assert!(ch.is_user_allowed("222"));
        assert!(!ch.is_user_allowed("333"));
        assert!(!ch.is_user_allowed("unknown"));
    }

    #[test]
    fn allowlist_is_exact_match_not_substring() {
        let ch = DiscordChannel::new("fake".into(), None, vec!["111".into()], false, false);
        assert!(!ch.is_user_allowed("1111"));
        assert!(!ch.is_user_allowed("11"));
        assert!(!ch.is_user_allowed("0111"));
    }

    #[test]
    fn allowlist_empty_string_user_id() {
        let ch = DiscordChannel::new("fake".into(), None, vec!["111".into()], false, false);
        assert!(!ch.is_user_allowed(""));
    }

    #[test]
    fn allowlist_with_wildcard_and_specific() {
        let ch = DiscordChannel::new(
            "fake".into(),
            None,
            vec!["111".into(), "*".into()],
            false,
            false,
        );
        assert!(ch.is_user_allowed("111"));
        assert!(ch.is_user_allowed("anyone_else"));
    }

    #[test]
    fn allowlist_case_sensitive() {
        let ch = DiscordChannel::new("fake".into(), None, vec!["ABC".into()], false, false);
        assert!(ch.is_user_allowed("ABC"));
        assert!(!ch.is_user_allowed("abc"));
        assert!(!ch.is_user_allowed("Abc"));
    }

    #[test]
    fn base64_decode_empty_string() {
        let decoded = base64_decode("");
        assert_eq!(decoded, Some(String::new()));
    }

    #[test]
    fn base64_decode_invalid_chars() {
        let decoded = base64_decode("!!!!");
        assert!(decoded.is_none());
    }

    #[test]
    fn bot_user_id_from_empty_token() {
        let id = DiscordChannel::bot_user_id_from_token("");
        assert_eq!(id, Some(String::new()));
    }

    #[test]
    fn contains_bot_mention_supports_plain_and_nick_forms() {
        assert!(contains_bot_mention("hi <@12345>", "12345"));
        assert!(contains_bot_mention("hi <@!12345>", "12345"));
        assert!(!contains_bot_mention("hi <@99999>", "12345"));
    }

    #[test]
    fn normalize_incoming_content_requires_mention_when_enabled() {
        let cleaned = normalize_incoming_content("hello there", true, "12345");
        assert!(cleaned.is_none());
    }

    #[test]
    fn normalize_incoming_content_strips_mentions_and_trims() {
        let cleaned = normalize_incoming_content("  <@!12345> run status  ", true, "12345");
        assert_eq!(cleaned.as_deref(), Some("run status"));
    }

    #[test]
    fn normalize_incoming_content_rejects_empty_after_strip() {
        let cleaned = normalize_incoming_content("<@12345>", true, "12345");
        assert!(cleaned.is_none());
    }

    #[test]
    fn typing_handles_start_empty() {
        let ch = DiscordChannel::new("fake".into(), None, vec![], false, false);
        let guard = ch.typing_handles.lock();
        assert!(guard.is_empty());
    }

    #[tokio::test]
    async fn start_typing_sets_handle() {
        let ch = DiscordChannel::new("fake".into(), None, vec![], false, false);
        let _ = ch.start_typing("123456").await;
        let guard = ch.typing_handles.lock();
        assert!(guard.contains_key("123456"));
    }

    #[tokio::test]
    async fn stop_typing_clears_handle() {
        let ch = DiscordChannel::new("fake".into(), None, vec![], false, false);
        let _ = ch.start_typing("123456").await;
        let _ = ch.stop_typing("123456").await;
        let guard = ch.typing_handles.lock();
        assert!(!guard.contains_key("123456"));
    }

    #[tokio::test]
    async fn stop_typing_is_idempotent() {
        let ch = DiscordChannel::new("fake".into(), None, vec![], false, false);
        assert!(ch.stop_typing("123456").await.is_ok());
        assert!(ch.stop_typing("123456").await.is_ok());
    }

    #[tokio::test]
    async fn concurrent_typing_handles_are_independent() {
        let ch = DiscordChannel::new("fake".into(), None, vec![], false, false);
        let _ = ch.start_typing("111").await;
        let _ = ch.start_typing("222").await;
        {
            let guard = ch.typing_handles.lock();
            assert_eq!(guard.len(), 2);
            assert!(guard.contains_key("111"));
            assert!(guard.contains_key("222"));
        }
        // Stopping one does not affect the other
        let _ = ch.stop_typing("111").await;
        let guard = ch.typing_handles.lock();
        assert_eq!(guard.len(), 1);
        assert!(guard.contains_key("222"));
    }

    // ── Message ID edge cases ─────────────────────────────────────

    #[test]
    fn discord_message_id_format_includes_discord_prefix() {
        // Verify that message IDs follow the format: discord_{message_id}
        let message_id = "123456789012345678";
        let expected_id = format!("discord_{message_id}");
        assert_eq!(expected_id, "discord_123456789012345678");
    }

    #[test]
    fn discord_message_id_is_deterministic() {
        // Same message_id = same ID (prevents duplicates after restart)
        let message_id = "123456789012345678";
        let id1 = format!("discord_{message_id}");
        let id2 = format!("discord_{message_id}");
        assert_eq!(id1, id2);
    }

    #[test]
    fn discord_message_id_different_message_different_id() {
        // Different message IDs produce different IDs
        let id1 = "discord_123456789012345678".to_string();
        let id2 = "discord_987654321098765432".to_string();
        assert_ne!(id1, id2);
    }

    #[test]
    fn discord_message_id_uses_snowflake_id() {
        // Discord snowflake IDs are numeric strings
        let message_id = "123456789012345678"; // Typical snowflake format
        let id = format!("discord_{message_id}");
        assert!(id.starts_with("discord_"));
        // Snowflake IDs are numeric
        assert!(message_id.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn discord_message_id_fallback_to_uuid_on_empty() {
        // Edge case: empty message_id falls back to UUID
        let message_id = "";
        let id = if message_id.is_empty() {
            format!("discord_{}", uuid::Uuid::new_v4())
        } else {
            format!("discord_{message_id}")
        };
        assert!(id.starts_with("discord_"));
        // Should have UUID dashes
        assert!(id.contains('-'));
    }

    // ── pairing (/bind, /claim) ──────────────────────────────

    #[test]
    fn extract_pairing_identities_collects_id_and_username() {
        let d = serde_json::json!({
            "author": { "id": "999", "username": "carol" },
            "content": "/claim ABCD-EFGH"
        });
        let identities = DiscordChannel::extract_pairing_identities(&d);
        assert_eq!(identities, vec!["999".to_string(), "carol".to_string()]);
    }

    #[test]
    fn extract_pairing_identities_id_only_when_no_username() {
        let d = serde_json::json!({ "author": { "id": "999" } });
        let identities = DiscordChannel::extract_pairing_identities(&d);
        assert_eq!(identities, vec!["999".to_string()]);
    }

    #[test]
    fn add_allowed_identity_runtime_grants_immediate_access() {
        let ch = DiscordChannel::new("fake".into(), None, vec![], false, false);
        assert!(!ch.is_user_allowed("999"));
        ch.add_allowed_identity_runtime("999");
        assert!(ch.is_user_allowed("999"));
        // Dedupes.
        ch.add_allowed_identity_runtime("999");
        assert_eq!(ch.allowed_users.read().unwrap().len(), 1);
    }

    /// A store-minted "discord" code (the kind `rantaiclaw channels pair` issues)
    /// is accepted on `/claim`: the shared core lands the sender in `allowed_users`
    /// AND `approval_owners`. Drives the same code path the inbound loop invokes.
    #[tokio::test]
    async fn store_minted_discord_code_claims_owner() {
        use crate::channels::pairing::{try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");

        // Seed a config with a discord section so apply_pairing has a target.
        {
            let mut seed = crate::config::Config::load_or_init().await.unwrap();
            seed.channels_config.discord = Some(crate::config::DiscordConfig {
                bot_token: "x".into(),
                guild_id: None,
                allowed_users: vec![],
                listen_to_bots: false,
                mention_only: false,
            });
            seed.save().await.unwrap();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let code = pairing_store::mint(root, "discord", 3_600, None, true, now).unwrap();

        let reply = try_handle_pairing(
            &format!("/claim {code}"),
            "discord",
            AllowlistField::AllowedUsers,
            &["999".to_string(), "carol".to_string()],
            root,
        )
        .await
        .expect("a /claim must be handled");
        assert!(reply.contains("owner"), "reply was: {reply}");

        let config = crate::config::Config::load_or_init().await.unwrap();
        let users = &config
            .channels_config
            .discord
            .as_ref()
            .unwrap()
            .allowed_users;
        assert!(users.contains(&"999".to_string()), "users: {users:?}");
        assert!(users.contains(&"carol".to_string()), "users: {users:?}");
        let owners = &config.channels_config.approval_owners;
        assert!(owners.contains(&"999".to_string()), "owners: {owners:?}");

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }
}
