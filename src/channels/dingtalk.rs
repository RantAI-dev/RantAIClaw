use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

const DINGTALK_BOT_CALLBACK_TOPIC: &str = "/v1.0/im/bot/messages/get";

/// DingTalk channel — connects via Stream Mode WebSocket for real-time messages.
/// Replies are sent through per-message session webhook URLs.
pub struct DingTalkChannel {
    client_id: String,
    client_secret: String,
    allowed_users: Vec<String>,
    /// Per-chat session webhooks for sending replies (chatID -> webhook URL).
    /// DingTalk provides a unique webhook URL with each incoming message.
    session_webhooks: Arc<RwLock<HashMap<String, String>>>,
}

/// Response from DingTalk gateway connection registration.
#[derive(serde::Deserialize)]
struct GatewayResponse {
    endpoint: String,
    ticket: String,
}

impl DingTalkChannel {
    pub fn new(client_id: String, client_secret: String, allowed_users: Vec<String>) -> Self {
        Self {
            client_id,
            client_secret,
            allowed_users,
            session_webhooks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client("channel.dingtalk")
    }

    fn is_user_allowed(&self, user_id: &str) -> bool {
        self.allowed_users.iter().any(|u| u == "*" || u == user_id)
    }

    /// Resolve the active profile root for the shared pairing-code store.
    fn pairing_profile_root() -> Option<std::path::PathBuf> {
        match crate::profile::ProfileManager::active() {
            Ok(p) => Some(p.root),
            Err(e) => {
                tracing::warn!("DingTalk pairing: couldn't resolve profile root: {e:#}");
                None
            }
        }
    }

    /// Self-onboarding hook: if `content` is a `/bind`/`/claim` command, validate
    /// it against the shared [`crate::security::pairing_store`] (appending the
    /// sender staff id to `allowed_users` and, for an owner-capable `/claim`, to
    /// `approval_owners`, then persisting `config.toml`) and reply via the
    /// message's session webhook.
    ///
    /// Returns `true` when the message WAS a pairing command (handled here — must
    /// NOT be forwarded to the agent), `false` otherwise (normal message → gate).
    async fn try_handle_store_pairing(
        &self,
        content: &str,
        sender_id: &str,
        chat_id: &str,
        session_webhook: Option<&str>,
    ) -> bool {
        use crate::channels::pairing::{parse_pairing_command, try_handle_pairing, AllowlistField};

        if parse_pairing_command(content).is_none() {
            return false;
        }
        let Some(root) = Self::pairing_profile_root() else {
            return false;
        };

        let Some(reply) = try_handle_pairing(
            content,
            "dingtalk",
            AllowlistField::AllowedUsers,
            &[sender_id.to_string()],
            &root,
        )
        .await
        else {
            return false;
        };

        // Register the session webhook so the reply (and future messages) can be
        // sent back to this now-paired chat.
        if let Some(webhook) = session_webhook {
            let mut webhooks = self.session_webhooks.write().await;
            webhooks.insert(chat_id.to_string(), webhook.to_string());
            webhooks.insert(sender_id.to_string(), webhook.to_string());
        }

        if let Err(e) = self.send(&SendMessage::new(reply, chat_id)).await {
            tracing::warn!("DingTalk pairing: failed to send reply: {e:#}");
        }
        true
    }

    fn parse_stream_data(frame: &serde_json::Value) -> Option<serde_json::Value> {
        match frame.get("data") {
            Some(serde_json::Value::String(raw)) => serde_json::from_str(raw).ok(),
            Some(serde_json::Value::Object(_)) => frame.get("data").cloned(),
            _ => None,
        }
    }

    fn resolve_chat_id(data: &serde_json::Value, sender_id: &str) -> String {
        let is_private_chat = data
            .get("conversationType")
            .and_then(|value| {
                value
                    .as_str()
                    .map(|v| v == "1")
                    .or_else(|| value.as_i64().map(|v| v == 1))
            })
            .unwrap_or(true);

        if is_private_chat {
            sender_id.to_string()
        } else {
            data.get("conversationId")
                .and_then(|c| c.as_str())
                .unwrap_or(sender_id)
                .to_string()
        }
    }

    /// Register a connection with DingTalk's gateway to get a WebSocket endpoint.
    async fn register_connection(&self) -> anyhow::Result<GatewayResponse> {
        let body = serde_json::json!({
            "clientId": self.client_id,
            "clientSecret": self.client_secret,
            "subscriptions": [
                {
                    "type": "CALLBACK",
                    "topic": DINGTALK_BOT_CALLBACK_TOPIC,
                }
            ],
        });

        let resp = self
            .http_client()
            .post("https://api.dingtalk.com/v1.0/gateway/connections/open")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("DingTalk gateway registration failed ({status}): {err}");
        }

        let gw: GatewayResponse = resp.json().await?;
        Ok(gw)
    }
}

#[async_trait]
impl Channel for DingTalkChannel {
    fn name(&self) -> &str {
        "dingtalk"
    }

    fn render_target(&self) -> crate::channels::format::RenderTarget {
        // DingTalk's `markdown` message type renders a CommonMark-ish subset but
        // no tables, so `tables_native: false` sends tables as an ASCII grid.
        crate::channels::format::RenderTarget::StdMarkdown {
            tables_native: false,
        }
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let webhooks = self.session_webhooks.read().await;
        let webhook_url = webhooks.get(&message.recipient).ok_or_else(|| {
            anyhow::anyhow!(
                "No session webhook found for chat {}. \
                 The user must send a message first to establish a session.",
                message.recipient
            )
        })?;

        let title = message.subject.as_deref().unwrap_or("RantaiClaw");
        let rendered =
            crate::channels::format::render_to_string(&message.content, &self.render_target());
        let body = serde_json::json!({
            "msgtype": "markdown",
            "markdown": {
                "title": title,
                "text": rendered,
            }
        });

        let resp = self
            .http_client()
            .post(webhook_url)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("DingTalk webhook reply failed ({status}): {err}");
        }

        Ok(())
    }

    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        tracing::info!("DingTalk: registering gateway connection...");

        let gw = self.register_connection().await?;
        let ws_url = format!("{}?ticket={}", gw.endpoint, gw.ticket);

        tracing::info!("DingTalk: connecting to stream WebSocket...");
        let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url).await?;
        let (mut write, mut read) = ws_stream.split();

        tracing::info!("DingTalk: connected and listening for messages...");

        while let Some(msg) = read.next().await {
            let msg = match msg {
                Ok(Message::Text(t)) => t,
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    tracing::warn!("DingTalk WebSocket error: {e}");
                    break;
                }
                _ => continue,
            };

            let frame: serde_json::Value = match serde_json::from_str(msg.as_ref()) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let frame_type = frame.get("type").and_then(|t| t.as_str()).unwrap_or("");

            match frame_type {
                "SYSTEM" => {
                    // Respond to system pings to keep the connection alive
                    let message_id = frame
                        .get("headers")
                        .and_then(|h| h.get("messageId"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("");

                    let pong = serde_json::json!({
                        "code": 200,
                        "headers": {
                            "contentType": "application/json",
                            "messageId": message_id,
                        },
                        "message": "OK",
                        "data": "",
                    });

                    if let Err(e) = write.send(Message::Text(pong.to_string().into())).await {
                        tracing::warn!("DingTalk: failed to send pong: {e}");
                        break;
                    }
                }
                "EVENT" | "CALLBACK" => {
                    // Parse the chatbot callback data from the frame.
                    let data = match Self::parse_stream_data(&frame) {
                        Some(v) => v,
                        None => {
                            tracing::debug!("DingTalk: frame has no parseable data payload");
                            continue;
                        }
                    };

                    // Extract message content
                    let content = data
                        .get("text")
                        .and_then(|t| t.get("content"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .trim();

                    if content.is_empty() {
                        continue;
                    }

                    let sender_id = data
                        .get("senderStaffId")
                        .and_then(|s| s.as_str())
                        .unwrap_or("unknown");

                    // Private chat uses sender ID, group chat uses conversation ID.
                    let chat_id = Self::resolve_chat_id(&data, sender_id);

                    // Intercept on-demand store-minted `/bind`/`/claim` pairing
                    // codes before the allowlist gate so unenrolled users can
                    // self-onboard without a daemon restart. Consumes when handled.
                    let session_webhook = data.get("sessionWebhook").and_then(|w| w.as_str());
                    if self
                        .try_handle_store_pairing(content, sender_id, &chat_id, session_webhook)
                        .await
                    {
                        continue;
                    }

                    if !self.is_user_allowed(sender_id) {
                        tracing::warn!(
                            "DingTalk: ignoring message from unauthorized user: {sender_id}"
                        );
                        continue;
                    }

                    // Store session webhook for later replies
                    if let Some(webhook) = data.get("sessionWebhook").and_then(|w| w.as_str()) {
                        let webhook = webhook.to_string();
                        let mut webhooks = self.session_webhooks.write().await;
                        // Use both keys so reply routing works for both group and private flows.
                        webhooks.insert(chat_id.clone(), webhook.clone());
                        webhooks.insert(sender_id.to_string(), webhook);
                    }

                    // Acknowledge the event
                    let message_id = frame
                        .get("headers")
                        .and_then(|h| h.get("messageId"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("");

                    let ack = serde_json::json!({
                        "code": 200,
                        "headers": {
                            "contentType": "application/json",
                            "messageId": message_id,
                        },
                        "message": "OK",
                        "data": "",
                    });
                    let _ = write.send(Message::Text(ack.to_string().into())).await;

                    let channel_msg = ChannelMessage {
                        sender_aliases: Vec::new(),
                        id: Uuid::new_v4().to_string(),
                        sender: sender_id.to_string(),
                        reply_target: chat_id,
                        content: content.to_string(),
                        channel: "dingtalk".to_string(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        thread_ts: None,
                    };

                    if tx.send(channel_msg).await.is_err() {
                        tracing::warn!("DingTalk: message channel closed");
                        break;
                    }
                }
                _ => {}
            }
        }

        anyhow::bail!("DingTalk WebSocket stream ended")
    }

    async fn health_check(&self) -> bool {
        self.register_connection().await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dingtalk_render_target_is_std_markdown() {
        let ch = DingTalkChannel::new("id".into(), "secret".into(), vec![]);
        assert_eq!(
            ch.render_target(),
            crate::channels::format::RenderTarget::StdMarkdown {
                tables_native: false
            }
        );
    }

    #[test]
    fn test_name() {
        let ch = DingTalkChannel::new("id".into(), "secret".into(), vec![]);
        assert_eq!(ch.name(), "dingtalk");
    }

    #[test]
    fn test_user_allowed_wildcard() {
        let ch = DingTalkChannel::new("id".into(), "secret".into(), vec!["*".into()]);
        assert!(ch.is_user_allowed("anyone"));
    }

    #[test]
    fn test_user_allowed_specific() {
        let ch = DingTalkChannel::new("id".into(), "secret".into(), vec!["user123".into()]);
        assert!(ch.is_user_allowed("user123"));
        assert!(!ch.is_user_allowed("other"));
    }

    #[test]
    fn test_user_denied_empty() {
        let ch = DingTalkChannel::new("id".into(), "secret".into(), vec![]);
        assert!(!ch.is_user_allowed("anyone"));
    }

    #[test]
    fn test_config_serde() {
        let toml_str = r#"
client_id = "app_id_123"
client_secret = "secret_456"
allowed_users = ["user1", "*"]
"#;
        let config: crate::config::schema::DingTalkConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.client_id, "app_id_123");
        assert_eq!(config.client_secret, "secret_456");
        assert_eq!(config.allowed_users, vec!["user1", "*"]);
    }

    #[test]
    fn test_config_serde_defaults() {
        let toml_str = r#"
client_id = "id"
client_secret = "secret"
"#;
        let config: crate::config::schema::DingTalkConfig = toml::from_str(toml_str).unwrap();
        assert!(config.allowed_users.is_empty());
    }

    #[test]
    fn parse_stream_data_supports_string_payload() {
        let frame = serde_json::json!({
            "data": "{\"text\":{\"content\":\"hello\"}}"
        });
        let parsed = DingTalkChannel::parse_stream_data(&frame).unwrap();
        assert_eq!(
            parsed.get("text").and_then(|v| v.get("content")),
            Some(&serde_json::json!("hello"))
        );
    }

    #[test]
    fn parse_stream_data_supports_object_payload() {
        let frame = serde_json::json!({
            "data": {"text": {"content": "hello"}}
        });
        let parsed = DingTalkChannel::parse_stream_data(&frame).unwrap();
        assert_eq!(
            parsed.get("text").and_then(|v| v.get("content")),
            Some(&serde_json::json!("hello"))
        );
    }

    /// A store-minted owner code consumed for the `dingtalk` surface appends the
    /// sender staff id to `allowed_users` and `approval_owners` and persists the
    /// config — the shared-core path `try_handle_store_pairing` invokes before the
    /// allowlist gate (the session-webhook reply send is exercised in integration,
    /// so we assert the store + config mutation here).
    #[tokio::test]
    async fn dingtalk_store_minted_claim_grants_owner() {
        use crate::channels::pairing::{try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        let _guard = crate::test_env::ENV_LOCK.lock().await;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");
        {
            let mut seed = crate::config::Config::load_or_init().await.unwrap();
            seed.channels_config.dingtalk = Some(crate::config::schema::DingTalkConfig {
                client_id: "id".into(),
                client_secret: "secret".into(),
                allowed_users: vec![],
            });
            seed.save().await.unwrap();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let code = pairing_store::mint(root, "dingtalk", 900, None, true, now).unwrap();

        let reply = try_handle_pairing(
            &format!("/claim {code}"),
            "dingtalk",
            AllowlistField::AllowedUsers,
            &["staff_abc".to_string()],
            root,
        )
        .await
        .expect("pairing command should be handled");
        assert!(reply.contains("owner"), "reply was: {reply}");

        let config = crate::config::Config::load_or_init().await.unwrap();
        let users = &config
            .channels_config
            .dingtalk
            .as_ref()
            .unwrap()
            .allowed_users;
        assert!(users.contains(&"staff_abc".to_string()));
        assert!(config
            .channels_config
            .approval_owners
            .contains(&"staff_abc".to_string()));

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }

    #[test]
    fn resolve_chat_id_handles_numeric_group_conversation_type() {
        let data = serde_json::json!({
            "conversationType": 2,
            "conversationId": "cid-group",
        });
        let chat_id = DingTalkChannel::resolve_chat_id(&data, "staff-1");
        assert_eq!(chat_id, "cid-group");
    }
}
