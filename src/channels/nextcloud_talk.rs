use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

/// Nextcloud Talk channel in webhook mode.
///
/// Incoming messages are received by the gateway endpoint `/nextcloud-talk`.
/// Outbound replies are sent through Nextcloud Talk OCS API.
pub struct NextcloudTalkChannel {
    base_url: String,
    app_token: String,
    allowed_users: Vec<String>,
    client: reqwest::Client,
}

impl NextcloudTalkChannel {
    pub fn new(base_url: String, app_token: String, allowed_users: Vec<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            app_token,
            allowed_users,
            client: reqwest::Client::new(),
        }
    }

    fn is_user_allowed(&self, actor_id: &str) -> bool {
        self.allowed_users.iter().any(|u| u == "*" || u == actor_id)
    }

    /// Resolve the active profile root for the shared pairing-code store.
    fn pairing_profile_root() -> Option<std::path::PathBuf> {
        match crate::profile::ProfileManager::active() {
            Ok(p) => Some(p.root),
            Err(e) => {
                tracing::warn!("Nextcloud Talk pairing: couldn't resolve profile root: {e:#}");
                None
            }
        }
    }

    /// Extract `(room_token, actor_id, content)` from a webhook payload for the
    /// shared pairing path, *without* the allowlist gate (so an unenrolled actor's
    /// `/bind`/`/claim` is still seen). Bot-originated and non-comment events are
    /// skipped. Returns `None` when the payload has no actionable user text.
    fn extract_pairing_context(payload: &serde_json::Value) -> Option<(String, String, String)> {
        if let Some(event_type) = payload.get("type").and_then(|v| v.as_str()) {
            if !event_type.eq_ignore_ascii_case("message") {
                return None;
            }
        }
        let message_obj = payload.get("message")?;

        let room_token = payload
            .get("object")
            .and_then(|obj| obj.get("token"))
            .and_then(|v| v.as_str())
            .or_else(|| message_obj.get("token").and_then(|v| v.as_str()))
            .map(str::trim)
            .filter(|token| !token.is_empty())?;

        let actor_type = message_obj
            .get("actorType")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("actorType").and_then(|v| v.as_str()))
            .unwrap_or("");
        if actor_type.eq_ignore_ascii_case("bots") {
            return None;
        }

        let actor_id = message_obj
            .get("actorId")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("actorId").and_then(|v| v.as_str()))
            .map(str::trim)
            .filter(|id| !id.is_empty())?;

        let content = message_obj
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|content| !content.is_empty())?;

        Some((
            room_token.to_string(),
            actor_id.to_string(),
            content.to_string(),
        ))
    }

    /// Self-onboarding hook: if the payload carries a `/bind`/`/claim` command,
    /// validate it against the shared [`crate::security::pairing_store`] (appending
    /// the actor id to `allowed_users` and, for an owner-capable `/claim`, to
    /// `approval_owners`, then persisting `config.toml`) and reply in-room.
    ///
    /// Returns `true` when the payload WAS a pairing command (handled here — must
    /// NOT be parsed/dispatched), `false` otherwise.
    pub async fn try_handle_store_pairing(&self, payload: &serde_json::Value) -> bool {
        use crate::channels::pairing::{parse_pairing_command, try_handle_pairing, AllowlistField};

        let Some((room_token, actor_id, content)) = Self::extract_pairing_context(payload) else {
            return false;
        };
        if parse_pairing_command(&content).is_none() {
            return false;
        }
        let Some(root) = Self::pairing_profile_root() else {
            return false;
        };

        let Some(reply) = try_handle_pairing(
            &content,
            "nextcloud_talk",
            AllowlistField::AllowedUsers,
            &[actor_id],
            &root,
        )
        .await
        else {
            return false;
        };

        if let Err(e) = self.send_to_room(&room_token, &reply).await {
            tracing::warn!("Nextcloud Talk pairing: failed to send reply: {e:#}");
        }
        true
    }

    fn now_unix_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn parse_timestamp_secs(value: Option<&serde_json::Value>) -> u64 {
        let raw = match value {
            Some(serde_json::Value::Number(num)) => num.as_u64(),
            Some(serde_json::Value::String(s)) => s.trim().parse::<u64>().ok(),
            _ => None,
        }
        .unwrap_or_else(Self::now_unix_secs);

        // Some payloads use milliseconds.
        if raw > 1_000_000_000_000 {
            raw / 1000
        } else {
            raw
        }
    }

    fn value_to_string(value: Option<&serde_json::Value>) -> Option<String> {
        match value {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        }
    }

    /// Parse a Nextcloud Talk webhook payload into channel messages.
    ///
    /// Relevant payload fields:
    /// - `type` (expects `message`)
    /// - `object.token` (room token for reply routing)
    /// - `message.actorType`, `message.actorId`, `message.message`, `message.timestamp`
    pub fn parse_webhook_payload(&self, payload: &serde_json::Value) -> Vec<ChannelMessage> {
        let mut messages = Vec::new();

        if let Some(event_type) = payload.get("type").and_then(|v| v.as_str()) {
            if !event_type.eq_ignore_ascii_case("message") {
                tracing::debug!("Nextcloud Talk: skipping non-message event: {event_type}");
                return messages;
            }
        }

        let Some(message_obj) = payload.get("message") else {
            return messages;
        };

        let room_token = payload
            .get("object")
            .and_then(|obj| obj.get("token"))
            .and_then(|v| v.as_str())
            .or_else(|| message_obj.get("token").and_then(|v| v.as_str()))
            .map(str::trim)
            .filter(|token| !token.is_empty());

        let Some(room_token) = room_token else {
            tracing::warn!("Nextcloud Talk: missing room token in webhook payload");
            return messages;
        };

        let actor_type = message_obj
            .get("actorType")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("actorType").and_then(|v| v.as_str()))
            .unwrap_or("");

        // Ignore bot-originated messages to prevent feedback loops.
        if actor_type.eq_ignore_ascii_case("bots") {
            tracing::debug!("Nextcloud Talk: skipping bot-originated message");
            return messages;
        }

        let actor_id = message_obj
            .get("actorId")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("actorId").and_then(|v| v.as_str()))
            .map(str::trim)
            .filter(|id| !id.is_empty());

        let Some(actor_id) = actor_id else {
            tracing::warn!("Nextcloud Talk: missing actorId in webhook payload");
            return messages;
        };

        if !self.is_user_allowed(actor_id) {
            tracing::warn!(
                "Nextcloud Talk: ignoring message from unauthorized actor: {actor_id}. \
                Add to channels.nextcloud_talk.allowed_users in config.toml, \
                or run `rantaiclaw onboard --channels-only` to configure interactively."
            );
            return messages;
        }

        let message_type = message_obj
            .get("messageType")
            .and_then(|v| v.as_str())
            .unwrap_or("comment");
        if !message_type.eq_ignore_ascii_case("comment") {
            tracing::debug!("Nextcloud Talk: skipping non-comment messageType: {message_type}");
            return messages;
        }

        // Ignore pure system messages.
        let has_system_message = message_obj
            .get("systemMessage")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());
        if has_system_message {
            tracing::debug!("Nextcloud Talk: skipping system message event");
            return messages;
        }

        let content = message_obj
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|content| !content.is_empty());

        let Some(content) = content else {
            return messages;
        };

        let message_id = Self::value_to_string(message_obj.get("id"))
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let timestamp = Self::parse_timestamp_secs(message_obj.get("timestamp"));

        messages.push(ChannelMessage {
            sender_aliases: Vec::new(),
            id: message_id,
            reply_target: room_token.to_string(),
            sender: actor_id.to_string(),
            content: content.to_string(),
            channel: "nextcloud_talk".to_string(),
            timestamp,
            thread_ts: None,
        });

        messages
    }

    async fn send_to_room(&self, room_token: &str, content: &str) -> anyhow::Result<()> {
        let encoded_room = urlencoding::encode(room_token);
        let url = format!(
            "{}/ocs/v2.php/apps/spreed/api/v1/chat/{}?format=json",
            self.base_url, encoded_room
        );

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.app_token)
            .header("OCS-APIRequest", "true")
            .header("Accept", "application/json")
            .json(&serde_json::json!({ "message": content }))
            .send()
            .await?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        tracing::error!("Nextcloud Talk send failed: {status} — {body}");
        anyhow::bail!("Nextcloud Talk API error: {status}");
    }
}

#[async_trait]
impl Channel for NextcloudTalkChannel {
    fn name(&self) -> &str {
        "nextcloud_talk"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.send_to_room(&message.recipient, &message.content)
            .await
    }

    async fn listen(
        &self,
        _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "Nextcloud Talk channel active (webhook mode). \
            Configure Nextcloud Talk bot webhook to POST to your gateway's /nextcloud-talk endpoint."
        );

        // Keep task alive; incoming events are handled by the gateway webhook handler.
        loop {
            tokio::time::sleep(std::time::Duration::from_hours(1)).await;
        }
    }

    async fn health_check(&self) -> bool {
        let url = format!("{}/status.php", self.base_url);

        self.client
            .get(&url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

/// Verify Nextcloud Talk webhook signature.
///
/// Signature calculation (official Talk bot docs):
/// `hex(hmac_sha256(secret, X-Nextcloud-Talk-Random + raw_body))`
pub fn verify_nextcloud_talk_signature(
    secret: &str,
    random: &str,
    body: &str,
    signature: &str,
) -> bool {
    let random = random.trim();
    if random.is_empty() {
        tracing::warn!("Nextcloud Talk: missing X-Nextcloud-Talk-Random header");
        return false;
    }

    let signature_hex = signature
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or(signature)
        .trim();

    let Ok(provided) = hex::decode(signature_hex) else {
        tracing::warn!("Nextcloud Talk: invalid signature format");
        return false;
    };

    let payload = format!("{random}{body}");
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(payload.as_bytes());

    mac.verify_slice(&provided).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel() -> NextcloudTalkChannel {
        NextcloudTalkChannel::new(
            "https://cloud.example.com".into(),
            "app-token".into(),
            vec!["user_a".into()],
        )
    }

    #[test]
    fn nextcloud_talk_channel_name() {
        let channel = make_channel();
        assert_eq!(channel.name(), "nextcloud_talk");
    }

    #[test]
    fn nextcloud_talk_user_allowlist_exact_and_wildcard() {
        let channel = make_channel();
        assert!(channel.is_user_allowed("user_a"));
        assert!(!channel.is_user_allowed("user_b"));

        let wildcard = NextcloudTalkChannel::new(
            "https://cloud.example.com".into(),
            "app-token".into(),
            vec!["*".into()],
        );
        assert!(wildcard.is_user_allowed("any_user"));
    }

    #[test]
    fn nextcloud_talk_parse_valid_message_payload() {
        let channel = make_channel();
        let payload = serde_json::json!({
            "type": "message",
            "object": {
                "id": "42",
                "token": "room-token-123",
                "name": "Team Room",
                "type": "room"
            },
            "message": {
                "id": 77,
                "token": "room-token-123",
                "actorType": "users",
                "actorId": "user_a",
                "actorDisplayName": "User A",
                "timestamp": 1_735_701_200,
                "messageType": "comment",
                "systemMessage": "",
                "message": "Hello from Nextcloud"
            }
        });

        let messages = channel.parse_webhook_payload(&payload);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "77");
        assert_eq!(messages[0].reply_target, "room-token-123");
        assert_eq!(messages[0].sender, "user_a");
        assert_eq!(messages[0].content, "Hello from Nextcloud");
        assert_eq!(messages[0].channel, "nextcloud_talk");
        assert_eq!(messages[0].timestamp, 1_735_701_200);
    }

    #[test]
    fn nextcloud_talk_parse_skips_non_message_events() {
        let channel = make_channel();
        let payload = serde_json::json!({
            "type": "room",
            "object": {"token": "room-token-123"},
            "message": {
                "actorType": "users",
                "actorId": "user_a",
                "message": "Hello"
            }
        });

        let messages = channel.parse_webhook_payload(&payload);
        assert!(messages.is_empty());
    }

    #[test]
    fn nextcloud_talk_parse_skips_bot_messages() {
        let channel = NextcloudTalkChannel::new(
            "https://cloud.example.com".into(),
            "app-token".into(),
            vec!["*".into()],
        );
        let payload = serde_json::json!({
            "type": "message",
            "object": {"token": "room-token-123"},
            "message": {
                "actorType": "bots",
                "actorId": "bot_1",
                "message": "Self message"
            }
        });

        let messages = channel.parse_webhook_payload(&payload);
        assert!(messages.is_empty());
    }

    #[test]
    fn nextcloud_talk_parse_skips_unauthorized_sender() {
        let channel = make_channel();
        let payload = serde_json::json!({
            "type": "message",
            "object": {"token": "room-token-123"},
            "message": {
                "actorType": "users",
                "actorId": "user_b",
                "message": "Unauthorized"
            }
        });

        let messages = channel.parse_webhook_payload(&payload);
        assert!(messages.is_empty());
    }

    #[test]
    fn nextcloud_talk_parse_skips_system_message() {
        let channel = NextcloudTalkChannel::new(
            "https://cloud.example.com".into(),
            "app-token".into(),
            vec!["*".into()],
        );
        let payload = serde_json::json!({
            "type": "message",
            "object": {"token": "room-token-123"},
            "message": {
                "actorType": "users",
                "actorId": "user_a",
                "messageType": "comment",
                "systemMessage": "joined",
                "message": ""
            }
        });

        let messages = channel.parse_webhook_payload(&payload);
        assert!(messages.is_empty());
    }

    #[test]
    fn nextcloud_talk_parse_timestamp_millis_to_seconds() {
        let channel = NextcloudTalkChannel::new(
            "https://cloud.example.com".into(),
            "app-token".into(),
            vec!["*".into()],
        );
        let payload = serde_json::json!({
            "type": "message",
            "object": {"token": "room-token-123"},
            "message": {
                "actorType": "users",
                "actorId": "user_a",
                "timestamp": 1_735_701_200_123_u64,
                "message": "hello"
            }
        });

        let messages = channel.parse_webhook_payload(&payload);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].timestamp, 1_735_701_200);
    }

    const TEST_WEBHOOK_SECRET: &str = "nextcloud_test_webhook_secret";

    #[test]
    fn nextcloud_talk_signature_verification_valid() {
        let secret = TEST_WEBHOOK_SECRET;
        let random = "random-seed";
        let body = r#"{"type":"message"}"#;

        let payload = format!("{random}{body}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());

        assert!(verify_nextcloud_talk_signature(
            secret, random, body, &signature
        ));
    }

    #[test]
    fn nextcloud_talk_signature_verification_invalid() {
        assert!(!verify_nextcloud_talk_signature(
            TEST_WEBHOOK_SECRET,
            "random-seed",
            r#"{"type":"message"}"#,
            "deadbeef"
        ));
    }

    #[test]
    fn nextcloud_talk_signature_verification_accepts_sha256_prefix() {
        let secret = TEST_WEBHOOK_SECRET;
        let random = "random-seed";
        let body = r#"{"type":"message"}"#;

        let payload = format!("{random}{body}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        assert!(verify_nextcloud_talk_signature(
            secret, random, body, &signature
        ));
    }

    /// The pairing context is extracted even for an actor not (yet) in the
    /// allowlist, so an unenrolled user's `/bind`/`/claim` reaches the shared core.
    #[test]
    fn nextcloud_talk_extract_pairing_context_ignores_allowlist() {
        let payload = serde_json::json!({
            "type": "message",
            "object": {"token": "room-token-123"},
            "message": {
                "actorType": "users",
                "actorId": "user_not_allowed",
                "message": "/bind ABCD-EFGH"
            }
        });
        let (room, actor, content) =
            NextcloudTalkChannel::extract_pairing_context(&payload).expect("should extract");
        assert_eq!(room, "room-token-123");
        assert_eq!(actor, "user_not_allowed");
        assert_eq!(content, "/bind ABCD-EFGH");
    }

    /// A store-minted owner code consumed for the `nextcloud_talk` surface appends
    /// the actor id to `allowed_users` and `approval_owners` and persists the
    /// config — the shared-core path `try_handle_store_pairing` invokes before the
    /// allowlist gate (the OCS reply send is exercised in integration, so we assert
    /// the store + config mutation here).
    #[tokio::test]
    async fn nextcloud_talk_store_minted_claim_grants_owner() {
        use crate::channels::pairing::{try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = ENV_LOCK.lock().await;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");
        {
            let mut seed = crate::config::Config::load_or_init().await.unwrap();
            seed.channels_config.nextcloud_talk = Some(crate::config::NextcloudTalkConfig {
                base_url: "https://cloud.example.com".into(),
                app_token: "tok".into(),
                webhook_secret: None,
                allowed_users: vec![],
            });
            seed.save().await.unwrap();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let code = pairing_store::mint(root, "nextcloud_talk", 900, None, true, now).unwrap();

        let reply = try_handle_pairing(
            &format!("/claim {code}"),
            "nextcloud_talk",
            AllowlistField::AllowedUsers,
            &["actor_99".to_string()],
            root,
        )
        .await
        .expect("pairing command should be handled");
        assert!(reply.contains("owner"), "reply was: {reply}");

        let config = crate::config::Config::load_or_init().await.unwrap();
        let users = &config
            .channels_config
            .nextcloud_talk
            .as_ref()
            .unwrap()
            .allowed_users;
        assert!(users.contains(&"actor_99".to_string()));
        assert!(config
            .channels_config
            .approval_owners
            .contains(&"actor_99".to_string()));

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }
}
