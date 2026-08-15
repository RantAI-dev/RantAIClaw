use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::sync::Arc;
use uuid::Uuid;

/// Linq channel — uses the Linq Partner V3 API for iMessage, RCS, and SMS.
///
/// This channel operates in webhook mode (push-based) rather than polling.
/// Messages are received via the gateway's `/linq` webhook endpoint.
/// The `listen` method here is a keepalive placeholder; actual message handling
/// happens in the gateway when Linq sends webhook events.
pub struct LinqChannel {
    api_token: String,
    from_phone: String,
    /// Allowed sender phone numbers (E.164). Wrapped so `/bind`/`/claim`
    /// self-onboarding can append a newly-paired sender at runtime without a
    /// daemon restart (mirrors the persisted config update).
    allowed_senders: Arc<RwLock<Vec<String>>>,
    client: reqwest::Client,
    /// Size/type limits for inbound images. Defaults to the shipped
    /// `[multimodal]` defaults; the factory overrides it with the operator's.
    multimodal: crate::config::MultimodalConfig,
}

/// Marker the synchronous webhook parser leaves behind for an inbound image;
/// `hydrate_media` resolves it before dispatch.
const PENDING_MEDIA_PREFIX: &str = "[LINQ_MEDIA:";

/// Base URL every Linq request goes to.
///
/// `pub(crate)` on purpose: the setup provisioner validates the operator's
/// Partner API token against this same base. When the two were independently
/// editable the provisioner drifted to `api.linq.com` — a domain this project
/// does not own — and shipped the live token there on every setup run.
pub(crate) const LINQ_API_BASE: &str = "https://api.linqapp.com/api/partner/v3";

/// Percent-encode a chat id for use as a single URL path segment.
///
/// The value originates in the inbound webhook payload and the request carries
/// a bearer token, so a `/` or `?` in it used to reshape the URL the token is
/// presented to. `NON_ALPHANUMERIC` is deliberate rather than a lighter set:
/// Linq chat ids observed in this codebase's own fixtures are opaque
/// alphanumeric handles with no path structure, so nothing legitimate is
/// altered by encoding everything else.
fn encode_chat_id(chat_id: &str) -> String {
    urlencoding::encode(chat_id).into_owned()
}

impl LinqChannel {
    pub fn new(api_token: String, from_phone: String, allowed_senders: Vec<String>) -> Self {
        Self {
            api_token,
            from_phone,
            allowed_senders: Arc::new(RwLock::new(allowed_senders)),
            client: reqwest::Client::new(),
            multimodal: crate::config::MultimodalConfig::default(),
        }
    }

    /// Apply the operator's `[multimodal]` limits to inbound images.
    #[must_use]
    pub fn with_multimodal(mut self, multimodal: crate::config::MultimodalConfig) -> Self {
        self.multimodal = multimodal;
        self
    }

    fn http_client(&self) -> reqwest::Client {
        self.client.clone()
    }

    /// Check if a sender phone number is allowed (E.164 format: +1234567890)
    fn is_sender_allowed(&self, phone: &str) -> bool {
        self.allowed_senders
            .read()
            .iter()
            .any(|n| n == "*" || n == phone)
    }

    /// Append a newly-paired sender to the runtime allowlist (deduped). Used by
    /// the shared `/bind`/`/claim` flow so the very next message from a paired
    /// sender is accepted without restarting the gateway. Config persistence is
    /// handled separately by the shared pairing core.
    pub fn add_allowed_sender_runtime(&self, phone: &str) {
        let phone = phone.trim();
        if phone.is_empty() {
            return;
        }
        let mut list = self.allowed_senders.write();
        if !list.iter().any(|n| n == phone) {
            list.push(phone.to_string());
        }
    }

    /// Extract `(text, normalized_sender, reply_target)` from a raw inbound Linq
    /// webhook payload for the shared `/bind`/`/claim` pairing path — *before*
    /// the allowlist gate in [`Self::parse_webhook_payload`] drops unknown
    /// senders. `normalized_sender` is the E.164 form used in the allowlist
    /// check (same identity persisted on a successful pairing). Returns `None`
    /// for non-message events, bot self-messages, or payloads with no text body.
    pub fn extract_pairing_context(
        payload: &serde_json::Value,
    ) -> Option<(String, String, String)> {
        if payload
            .get("event_type")
            .and_then(|e| e.as_str())
            .unwrap_or("")
            != "message.received"
        {
            return None;
        }
        let data = payload.get("data")?;
        if data
            .get("is_from_me")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return None;
        }
        let from = data.get("from").and_then(|f| f.as_str())?;
        let normalized_from = if from.starts_with('+') {
            from.to_string()
        } else {
            format!("+{from}")
        };

        let parts = data
            .get("message")
            .and_then(|m| m.get("parts"))
            .and_then(|p| p.as_array())?;
        let text = parts
            .iter()
            .filter_map(|part| {
                if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                    part.get("value").and_then(|v| v.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim();
        if text.is_empty() {
            return None;
        }

        let chat_id = data.get("chat_id").and_then(|c| c.as_str()).unwrap_or("");
        let reply_target = if chat_id.is_empty() {
            normalized_from.clone()
        } else {
            chat_id.to_string()
        };

        Some((text.to_string(), normalized_from, reply_target))
    }

    /// Get the bot's phone number
    pub fn phone_number(&self) -> &str {
        &self.from_phone
    }

    /// A media part becomes a **pending** marker, resolved by [`Self::hydrate_media`]
    /// before dispatch.
    ///
    /// It used to emit `[IMAGE:<url>]` — the platform's URL, straight into the
    /// agent's marker path. That was wrong twice over: the URL is fetched (if
    /// at all) only when `[multimodal].allow_remote_fetch` is on, so under the
    /// default config the image silently never loaded; and when it was on, an
    /// attacker-supplied URL was fetched with no size cap and the type taken
    /// from the payload. `docs/security/inbound-media-policy.md` exists to stop
    /// exactly that.
    fn media_part_to_image_marker(part: &serde_json::Value) -> Option<String> {
        let source = part
            .get("url")
            .or_else(|| part.get("value"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())?;

        let mime_type = part
            .get("mime_type")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase();

        if !mime_type.starts_with("image/") {
            return None;
        }
        if source.contains(']') || source.contains('|') {
            // The marker is delimited by `|` and `]`; a URL carrying either
            // would let the payload forge a second marker.
            tracing::warn!("Linq: skipping a media URL containing a marker delimiter");
            return None;
        }

        Some(format!("{PENDING_MEDIA_PREFIX}{source}|{mime_type}]"))
    }

    /// Replace every pending media marker with a `data:` URI or a rejection
    /// note, per `docs/security/inbound-media-policy.md`. Called by the gateway
    /// after parsing and before dispatch, because the parser is synchronous.
    pub async fn hydrate_media(&self, messages: &mut [ChannelMessage]) {
        let cap = crate::channels::media::max_bytes(&self.multimodal);
        let client = self.http_client();
        for message in messages.iter_mut() {
            let sender_key = format!("linq:{}", message.sender);
            while let Some(start) = message.content.find(PENDING_MEDIA_PREFIX) {
                let Some(end) = message.content[start..].find(']') else {
                    break;
                };
                let end = start + end + 1;
                let inner = &message.content[start + PENDING_MEDIA_PREFIX.len()..end - 1];
                let (url, claimed) = inner.rsplit_once('|').unwrap_or((inner, ""));
                let claimed = (!claimed.is_empty()).then_some(claimed);
                // No bearer: the host comes from the payload, so the channel's
                // API token does not belong in this request.
                let replacement = crate::channels::media::fetch_image(
                    &client,
                    url,
                    None,
                    claimed,
                    cap,
                    &sender_key,
                )
                .await
                .to_marker();
                message.content.replace_range(start..end, &replacement);
            }
        }
    }

    /// Parse an incoming webhook payload from Linq and extract messages.
    ///
    /// Linq webhook envelope:
    /// ```json
    /// {
    ///   "api_version": "v3",
    ///   "event_type": "message.received",
    ///   "event_id": "...",
    ///   "created_at": "...",
    ///   "trace_id": "...",
    ///   "data": {
    ///     "chat_id": "...",
    ///     "from": "+1...",
    ///     "recipient_phone": "+1...",
    ///     "is_from_me": false,
    ///     "service": "iMessage",
    ///     "message": {
    ///       "id": "...",
    ///       "parts": [{ "type": "text", "value": "..." }]
    ///     }
    ///   }
    /// }
    /// ```
    pub fn parse_webhook_payload(&self, payload: &serde_json::Value) -> Vec<ChannelMessage> {
        let mut messages = Vec::new();

        // Only handle message.received events
        let event_type = payload
            .get("event_type")
            .and_then(|e| e.as_str())
            .unwrap_or("");
        if event_type != "message.received" {
            tracing::debug!("Linq: skipping non-message event: {event_type}");
            return messages;
        }

        let Some(data) = payload.get("data") else {
            return messages;
        };

        // Skip messages sent by the bot itself
        if data
            .get("is_from_me")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            tracing::debug!("Linq: skipping is_from_me message");
            return messages;
        }

        // Get sender phone number
        let Some(from) = data.get("from").and_then(|f| f.as_str()) else {
            return messages;
        };

        // Normalize to E.164 format
        let normalized_from = if from.starts_with('+') {
            from.to_string()
        } else {
            format!("+{from}")
        };

        // Check allowlist
        if !self.is_sender_allowed(&normalized_from) {
            tracing::warn!(
                "Linq: ignoring message from unauthorized sender: {normalized_from}. \
                Add to channels.linq.allowed_senders in config.toml, \
                or run `rantaiclaw onboard --channels-only` to configure interactively."
            );
            return messages;
        }

        // Get chat_id for reply routing
        let chat_id = data
            .get("chat_id")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        // Extract text from message parts
        let Some(message) = data.get("message") else {
            return messages;
        };

        let Some(parts) = message.get("parts").and_then(|p| p.as_array()) else {
            return messages;
        };

        let content_parts: Vec<String> = parts
            .iter()
            .filter_map(|part| {
                let part_type = part.get("type").and_then(|t| t.as_str())?;
                match part_type {
                    "text" => part
                        .get("value")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string),
                    "media" | "image" => {
                        if let Some(marker) = Self::media_part_to_image_marker(part) {
                            Some(marker)
                        } else {
                            tracing::debug!("Linq: skipping unsupported {part_type} part");
                            None
                        }
                    }
                    _ => {
                        tracing::debug!("Linq: skipping {part_type} part");
                        None
                    }
                }
            })
            .collect();

        if content_parts.is_empty() {
            return messages;
        }

        let content = content_parts.join("\n").trim().to_string();

        if content.is_empty() {
            return messages;
        }

        // Get timestamp from created_at or use current time
        let timestamp = payload
            .get("created_at")
            .and_then(|t| t.as_str())
            .and_then(|t| {
                chrono::DateTime::parse_from_rfc3339(t)
                    .ok()
                    .map(|dt| dt.timestamp().cast_unsigned())
            })
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            });

        // Use chat_id as reply_target so replies go to the right conversation
        let reply_target = if chat_id.is_empty() {
            normalized_from.clone()
        } else {
            chat_id
        };

        // Carry the platform id: a UUID minted here makes a redelivery
        // undetectable, so the agent runs again on a message it answered.
        let platform_id = data
            .get("message_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
            .map_or_else(|| Uuid::new_v4().to_string(), |id| format!("linq_{id}"));

        messages.push(ChannelMessage {
            sender_aliases: Vec::new(),
            id: platform_id,
            reply_target,
            sender: normalized_from,
            content,
            channel: "linq".to_string(),
            timestamp,
            thread_ts: None,
        });

        messages
    }
}

#[async_trait]
impl Channel for LinqChannel {
    fn name(&self) -> &str {
        "linq"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        // If reply_target looks like a chat_id, send to existing chat.
        // Otherwise create a new chat with the recipient phone number.
        let recipient = &message.recipient;

        // Linq text parts render no markup — strip to readable text. Used by both
        // the existing-chat and new-chat bind points.
        let rendered = crate::channels::format::render_to_string(
            &message.content,
            &crate::channels::format::RenderTarget::Plain,
        );

        let body = serde_json::json!({
            "message": {
                "parts": [{
                    "type": "text",
                    "value": rendered
                }]
            }
        });

        // Try sending to existing chat (recipient is chat_id)
        let url = format!(
            "{LINQ_API_BASE}/chats/{}/messages",
            encode_chat_id(recipient)
        );

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if resp.status().is_success() {
            return Ok(());
        }

        // If the chat_id-based send failed with 404, try creating a new chat
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            let new_chat_body = serde_json::json!({
                "from": self.from_phone,
                "to": [recipient],
                "message": {
                    "parts": [{
                        "type": "text",
                        "value": rendered
                    }]
                }
            });

            let create_resp = self
                .client
                .post(format!("{LINQ_API_BASE}/chats"))
                .bearer_auth(&self.api_token)
                .header("Content-Type", "application/json")
                .json(&new_chat_body)
                .send()
                .await?;

            if !create_resp.status().is_success() {
                let status = create_resp.status();
                let error_body = create_resp.text().await.unwrap_or_default();
                tracing::error!("Linq create chat failed: {status} — {error_body}");
                anyhow::bail!("Linq API error: {status}");
            }

            return Ok(());
        }

        let status = resp.status();
        let error_body = resp.text().await.unwrap_or_default();
        tracing::error!("Linq send failed: {status} — {error_body}");
        anyhow::bail!("Linq API error: {status}");
    }

    async fn listen(
        &self,
        _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        // Linq uses webhooks (push-based), not polling.
        // Messages are received via the gateway's /linq endpoint.
        tracing::info!(
            "Linq channel active (webhook mode). \
            Configure Linq webhook to POST to your gateway's /linq endpoint."
        );

        // Keep the task alive — it will be cancelled when the channel shuts down
        loop {
            tokio::time::sleep(std::time::Duration::from_hours(1)).await;
        }
    }

    async fn health_check(&self) -> bool {
        // Check if we can reach the Linq API
        let url = format!("{LINQ_API_BASE}/phonenumbers");

        self.client
            .get(&url)
            .bearer_auth(&self.api_token)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        let url = format!("{LINQ_API_BASE}/chats/{}/typing", encode_chat_id(recipient));

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_token)
            .send()
            .await?;

        if !resp.status().is_success() {
            tracing::debug!("Linq start_typing failed: {}", resp.status());
        }

        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()> {
        let url = format!("{LINQ_API_BASE}/chats/{}/typing", encode_chat_id(recipient));

        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&self.api_token)
            .send()
            .await?;

        if !resp.status().is_success() {
            tracing::debug!("Linq stop_typing failed: {}", resp.status());
        }

        Ok(())
    }
}

/// Verify a Linq webhook signature.
///
/// Linq signs webhooks with HMAC-SHA256 over `"{timestamp}.{body}"`.
/// The signature is sent in `X-Webhook-Signature` (hex-encoded) and the
/// timestamp in `X-Webhook-Timestamp`. Reject timestamps older than 300s.
/// `body` is the RAW request body.
///
/// It used to be a `&str` the caller produced with `from_utf8_lossy`, while the
/// handler parsed the raw bytes — so the string that was verified was a
/// many-to-one projection of the body that was acted on. Every invalid sequence
/// collapses to `U+FFFD`, so distinct bodies verify identically.
pub fn verify_linq_signature(secret: &str, body: &[u8], timestamp: &str, signature: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    // Reject stale timestamps (>300s old)
    if let Ok(ts) = timestamp.parse::<i64>() {
        let now = chrono::Utc::now().timestamp();
        if (now - ts).unsigned_abs() > 300 {
            tracing::warn!("Linq: rejecting stale webhook timestamp ({ts}, now={now})");
            return false;
        }
    } else {
        tracing::warn!("Linq: invalid webhook timestamp: {timestamp}");
        return false;
    }

    // HMAC-SHA256 over "{timestamp}." followed by the raw body bytes.
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(body);
    let signature_hex = signature
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or(signature);
    let Ok(provided) = hex::decode(signature_hex.trim()) else {
        tracing::warn!("Linq: invalid webhook signature format");
        return false;
    };

    // Constant-time comparison via HMAC verify.
    mac.verify_slice(&provided).is_ok()
}

#[cfg(test)]
mod tests {
    /// The chat id comes from the inbound webhook payload and the request
    /// carries a bearer token, so a path metacharacter used to reshape the URL
    /// that token is presented to.
    #[test]
    fn linq_recipient_with_a_metacharacter_is_encoded() {
        assert_eq!(encode_chat_id("chat-123"), "chat-123");
        assert_eq!(
            encode_chat_id("../../admin"),
            "..%2F..%2Fadmin",
            "a path traversal must not survive into the URL"
        );
        assert_eq!(encode_chat_id("a?b=c"), "a%3Fb%3Dc");
        assert_eq!(encode_chat_id("a b"), "a%20b");
    }

    /// A UUID minted per inbound message makes a redelivery undetectable, so
    /// the agent runs again on a message it already answered.
    #[test]
    fn linq_platform_message_id_is_carried() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "message_id": "msg-abc",
                "chat_id": "chat-789",
                "from": "1234567890",
                "message": { "parts": [{ "type": "text", "value": "hello" }] }
            }
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1, "expected one parsed message: {msgs:?}");
        assert_eq!(msgs[0].id, "linq_msg-abc");
    }

    use super::*;

    fn make_channel() -> LinqChannel {
        LinqChannel::new(
            "test-token".into(),
            "+15551234567".into(),
            vec!["+1234567890".into()],
        )
    }

    #[test]
    fn linq_channel_name() {
        let ch = make_channel();
        assert_eq!(ch.name(), "linq");
    }

    #[test]
    fn linq_sender_allowed_exact() {
        let ch = make_channel();
        assert!(ch.is_sender_allowed("+1234567890"));
        assert!(!ch.is_sender_allowed("+9876543210"));
    }

    #[test]
    fn linq_sender_allowed_wildcard() {
        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec!["*".into()]);
        assert!(ch.is_sender_allowed("+1234567890"));
        assert!(ch.is_sender_allowed("+9999999999"));
    }

    #[test]
    fn linq_sender_allowed_empty() {
        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec![]);
        assert!(!ch.is_sender_allowed("+1234567890"));
    }

    #[test]
    fn linq_parse_valid_text_message() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "api_version": "v3",
            "event_type": "message.received",
            "event_id": "evt-123",
            "created_at": "2025-01-15T12:00:00Z",
            "trace_id": "trace-456",
            "data": {
                "chat_id": "chat-789",
                "from": "+1234567890",
                "recipient_phone": "+15551234567",
                "is_from_me": false,
                "service": "iMessage",
                "message": {
                    "id": "msg-abc",
                    "parts": [{
                        "type": "text",
                        "value": "Hello RantaiClaw!"
                    }]
                }
            }
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender, "+1234567890");
        assert_eq!(msgs[0].content, "Hello RantaiClaw!");
        assert_eq!(msgs[0].channel, "linq");
        assert_eq!(msgs[0].reply_target, "chat-789");
    }

    #[test]
    fn linq_parse_skip_is_from_me() {
        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "chat_id": "chat-789",
                "from": "+1234567890",
                "is_from_me": true,
                "message": {
                    "id": "msg-abc",
                    "parts": [{ "type": "text", "value": "My own message" }]
                }
            }
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty(), "is_from_me messages should be skipped");
    }

    #[test]
    fn linq_parse_skip_non_message_event() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "event_type": "message.delivered",
            "data": {
                "chat_id": "chat-789",
                "message_id": "msg-abc"
            }
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty(), "Non-message events should be skipped");
    }

    #[test]
    fn linq_parse_unauthorized_sender() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "chat_id": "chat-789",
                "from": "+9999999999",
                "is_from_me": false,
                "message": {
                    "id": "msg-abc",
                    "parts": [{ "type": "text", "value": "Spam" }]
                }
            }
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty(), "Unauthorized senders should be filtered");
    }

    #[test]
    fn linq_parse_empty_payload() {
        let ch = make_channel();
        let payload = serde_json::json!({});
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn linq_parse_media_only_translated_to_image_marker() {
        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "chat_id": "chat-789",
                "from": "+1234567890",
                "is_from_me": false,
                "message": {
                    "id": "msg-abc",
                    "parts": [{
                        "type": "media",
                        "url": "https://example.com/image.jpg",
                        "mime_type": "image/jpeg"
                    }]
                }
            }
        });

        // The parser leaves a PENDING marker: it is synchronous, and the
        // policy (fetch bounded, sniff the bytes, embed as a data: URI) needs
        // a request. `hydrate_media` resolves it before dispatch. This
        // assertion used to expect `[IMAGE:<the platform's URL>]`, which is
        // what handed an attacker-supplied URL to the agent's marker path.
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].content,
            "[LINQ_MEDIA:https://example.com/image.jpg|image/jpeg]"
        );
        assert!(
            !msgs[0].content.starts_with("[IMAGE:"),
            "a remote URL must not reach the agent as an image marker"
        );
    }

    /// The marker is delimited by `|` and `]`, so a URL carrying either could
    /// forge a second one. Such a part is dropped.
    #[test]
    fn linq_media_url_with_a_marker_delimiter_is_refused() {
        let part = serde_json::json!({
            "type": "media",
            "url": "https://example.com/a]b.jpg",
            "mime_type": "image/jpeg"
        });
        assert!(LinqChannel::media_part_to_image_marker(&part).is_none());

        // Control: the same part without the delimiter is accepted.
        let clean = serde_json::json!({
            "type": "media",
            "url": "https://example.com/ab.jpg",
            "mime_type": "image/jpeg"
        });
        assert!(LinqChannel::media_part_to_image_marker(&clean).is_some());
    }

    /// The budget key is channel-qualified, so a Linq sender cannot spend
    /// another channel's allowance. Dropping the `linq:` prefix fails this.
    #[tokio::test]
    async fn linq_charges_the_media_budget_under_a_channel_qualified_key() {
        use crate::channels::media;

        let sender = "+15559999999";
        for _ in 0..media::BUDGET_IMAGES {
            assert!(media::charge(&format!("linq:{sender}")).is_ok());
        }

        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec!["*".into()]);
        // Port 1 on loopback: nothing listens there, so a budget note rather
        // than a fetch failure proves the refusal precedes the request.
        let mut messages = vec![ChannelMessage {
            sender: sender.to_string(),
            content: "[LINQ_MEDIA:http://127.0.0.1:1/x.jpg|image/jpeg]".to_string(),
            ..Default::default()
        }];
        ch.hydrate_media(&mut messages).await;

        assert!(
            messages[0].content.contains("media budget spent"),
            "{}",
            messages[0].content
        );
    }

    #[tokio::test]
    async fn linq_hydrate_media_applies_the_shared_policy() {
        async fn pdf() -> (axum::http::HeaderMap, axum::body::Bytes) {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("content-type", "image/jpeg".parse().expect("header"));
            (
                headers,
                axum::body::Bytes::from_static(b"%PDF-1.7 not an image"),
            )
        }
        let app = axum::Router::new().route("/x.jpg", axum::routing::get(pdf));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec!["*".into()]);
        let mut messages = vec![ChannelMessage {
            content: format!("[LINQ_MEDIA:http://{addr}/x.jpg|image/jpeg]"),
            ..Default::default()
        }];
        ch.hydrate_media(&mut messages).await;

        // Bytes decide, not the claimed type — and the user is told.
        assert!(
            messages[0].content.contains("unsupported type"),
            "{}",
            messages[0].content
        );
        assert!(!messages[0].content.contains("LINQ_MEDIA"));
    }

    #[test]
    fn linq_parse_media_non_image_still_skipped() {
        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "chat_id": "chat-789",
                "from": "+1234567890",
                "is_from_me": false,
                "message": {
                    "id": "msg-abc",
                    "parts": [{
                        "type": "media",
                        "url": "https://example.com/sound.mp3",
                        "mime_type": "audio/mpeg"
                    }]
                }
            }
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty(), "Non-image media should still be skipped");
    }

    #[test]
    fn linq_parse_multiple_text_parts() {
        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "chat_id": "chat-789",
                "from": "+1234567890",
                "is_from_me": false,
                "message": {
                    "id": "msg-abc",
                    "parts": [
                        { "type": "text", "value": "First part" },
                        { "type": "text", "value": "Second part" }
                    ]
                }
            }
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "First part\nSecond part");
    }

    /// Fixture secret used exclusively in signature-verification unit tests (not a real credential).
    const TEST_WEBHOOK_SECRET: &str = "test_webhook_secret";

    #[test]
    fn linq_signature_verification_valid() {
        let secret = TEST_WEBHOOK_SECRET;
        let body = r#"{"event_type":"message.received"}"#;
        let now = chrono::Utc::now().timestamp().to_string();

        // Compute expected signature
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let message = format!("{now}.{body}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(message.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());

        assert!(verify_linq_signature(
            secret,
            body.as_bytes(),
            &now,
            &signature
        ));
    }

    /// The verifier used to receive a `from_utf8_lossy` copy while the handler
    /// parsed the raw bytes, so two different bodies could verify identically:
    /// every invalid sequence collapses to the same U+FFFD.
    #[test]
    fn linq_signature_verifies_over_raw_bytes() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let secret = TEST_WEBHOOK_SECRET;
        let now = chrono::Utc::now().timestamp().to_string();

        // Two distinct bodies that `from_utf8_lossy` maps to the same string.
        let body_a: Vec<u8> = [br#"{"a":""#.as_ref(), &[0xC3], br#""}"#.as_ref()].concat();
        let body_b: Vec<u8> = [br#"{"a":""#.as_ref(), &[0xC2], br#""}"#.as_ref()].concat();
        assert_ne!(body_a, body_b, "the bodies must differ");
        assert_eq!(
            String::from_utf8_lossy(&body_a),
            String::from_utf8_lossy(&body_b),
            "precondition: lossy decoding erases the difference"
        );

        let sign = |body: &[u8]| {
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
            mac.update(format!("{now}.").as_bytes());
            mac.update(body);
            hex::encode(mac.finalize().into_bytes())
        };

        let sig_a = sign(&body_a);
        assert!(verify_linq_signature(secret, &body_a, &now, &sig_a));
        assert!(
            !verify_linq_signature(secret, &body_b, &now, &sig_a),
            "a signature for one body must not verify the other"
        );
    }

    #[test]
    fn linq_signature_verification_invalid() {
        let secret = TEST_WEBHOOK_SECRET;
        let body = r#"{"event_type":"message.received"}"#;
        let now = chrono::Utc::now().timestamp().to_string();

        assert!(!verify_linq_signature(
            secret,
            body.as_bytes(),
            &now,
            "deadbeefdeadbeefdeadbeef"
        ));
    }

    #[test]
    fn linq_signature_verification_stale_timestamp() {
        let secret = TEST_WEBHOOK_SECRET;
        let body = r#"{"event_type":"message.received"}"#;
        // 10 minutes ago — stale
        let stale_ts = (chrono::Utc::now().timestamp() - 600).to_string();

        // Even with correct signature, stale timestamp should fail
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let message = format!("{stale_ts}.{body}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(message.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());

        assert!(
            !verify_linq_signature(secret, body.as_bytes(), &stale_ts, &signature),
            "Stale timestamps (>300s) should be rejected"
        );
    }

    #[test]
    fn linq_signature_verification_accepts_sha256_prefix() {
        let secret = TEST_WEBHOOK_SECRET;
        let body = r#"{"event_type":"message.received"}"#;
        let now = chrono::Utc::now().timestamp().to_string();

        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let message = format!("{now}.{body}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(message.as_bytes());
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        assert!(verify_linq_signature(
            secret,
            body.as_bytes(),
            &now,
            &signature
        ));
    }

    #[test]
    fn linq_signature_verification_accepts_uppercase_hex() {
        let secret = TEST_WEBHOOK_SECRET;
        let body = r#"{"event_type":"message.received"}"#;
        let now = chrono::Utc::now().timestamp().to_string();

        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let message = format!("{now}.{body}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(message.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes()).to_ascii_uppercase();

        assert!(verify_linq_signature(
            secret,
            body.as_bytes(),
            &now,
            &signature
        ));
    }

    #[test]
    fn linq_parse_normalizes_phone_with_plus() {
        let ch = LinqChannel::new(
            "tok".into(),
            "+15551234567".into(),
            vec!["+1234567890".into()],
        );
        // API sends without +, normalize to +
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "chat_id": "chat-789",
                "from": "1234567890",
                "is_from_me": false,
                "message": {
                    "id": "msg-abc",
                    "parts": [{ "type": "text", "value": "Hi" }]
                }
            }
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender, "+1234567890");
    }

    #[test]
    fn linq_parse_missing_data() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "event_type": "message.received"
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn linq_parse_missing_message_parts() {
        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "chat_id": "chat-789",
                "from": "+1234567890",
                "is_from_me": false,
                "message": {
                    "id": "msg-abc"
                }
            }
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn linq_parse_empty_text_value() {
        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "chat_id": "chat-789",
                "from": "+1234567890",
                "is_from_me": false,
                "message": {
                    "id": "msg-abc",
                    "parts": [{ "type": "text", "value": "" }]
                }
            }
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty(), "Empty text should be skipped");
    }

    #[test]
    fn linq_parse_fallback_reply_target_when_no_chat_id() {
        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "from": "+1234567890",
                "is_from_me": false,
                "message": {
                    "id": "msg-abc",
                    "parts": [{ "type": "text", "value": "Hi" }]
                }
            }
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        // Falls back to sender phone number when no chat_id
        assert_eq!(msgs[0].reply_target, "+1234567890");
    }

    #[test]
    fn linq_phone_number_accessor() {
        let ch = make_channel();
        assert_eq!(ch.phone_number(), "+15551234567");
    }

    // ══════════════════════════════════════════════════════════
    // Pairing (`/bind` / `/claim`) self-onboarding
    // ══════════════════════════════════════════════════════════

    #[test]
    fn linq_extract_pairing_context_normalizes_sender() {
        // API sends `from` without a leading `+`; pairing identity must be the
        // E.164 form used in the allowlist check.
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "chat_id": "chat-789",
                "from": "1234567890",
                "is_from_me": false,
                "message": {
                    "id": "msg-abc",
                    "parts": [{ "type": "text", "value": "/bind ABCD-EFGH" }]
                }
            }
        });
        let (text, sender, reply_target) =
            LinqChannel::extract_pairing_context(&payload).expect("should extract");
        assert_eq!(text, "/bind ABCD-EFGH");
        assert_eq!(sender, "+1234567890");
        assert_eq!(reply_target, "chat-789");
    }

    #[test]
    fn linq_extract_pairing_context_skips_self_and_non_message() {
        // is_from_me must not be paired against.
        let from_me = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "from": "+1234567890",
                "is_from_me": true,
                "message": { "parts": [{ "type": "text", "value": "/bind X" }] }
            }
        });
        assert!(LinqChannel::extract_pairing_context(&from_me).is_none());

        // Non-message events are ignored.
        let delivered = serde_json::json!({
            "event_type": "message.delivered",
            "data": { "from": "+1234567890" }
        });
        assert!(LinqChannel::extract_pairing_context(&delivered).is_none());
    }

    #[test]
    fn linq_extract_pairing_context_falls_back_to_sender_reply_target() {
        let payload = serde_json::json!({
            "event_type": "message.received",
            "data": {
                "from": "+1234567890",
                "is_from_me": false,
                "message": { "parts": [{ "type": "text", "value": "/claim WXYZ-1234" }] }
            }
        });
        let (_, sender, reply_target) =
            LinqChannel::extract_pairing_context(&payload).expect("should extract");
        assert_eq!(reply_target, sender);
    }

    #[test]
    fn linq_add_allowed_sender_runtime_accepts_paired_sender() {
        let ch = LinqChannel::new("tok".into(), "+15551234567".into(), vec![]);
        assert!(!ch.is_sender_allowed("+1999999999"));
        ch.add_allowed_sender_runtime("+1999999999");
        assert!(ch.is_sender_allowed("+1999999999"));
        // Dedupe: adding again keeps a single entry.
        ch.add_allowed_sender_runtime("+1999999999");
        assert_eq!(
            ch.allowed_senders
                .read()
                .iter()
                .filter(|n| n.as_str() == "+1999999999")
                .count(),
            1
        );
    }

    /// A store-minted "linq" code (the kind `rantaiclaw channels pair --channel
    /// linq` issues) is accepted on `/claim`: the shared core lands the sender in
    /// `allowed_senders` AND `approval_owners`. Mirrors telegram's
    /// `store_minted_telegram_code_claims_owner` against the linq surface/field.
    #[tokio::test]
    async fn store_minted_linq_code_claims_owner() {
        use crate::channels::pairing::{try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");

        // Seed a config with a linq section so apply_pairing has a target.
        {
            let mut seed = crate::config::Config::load_or_init().await.unwrap();
            seed.channels_config.linq = Some(crate::config::schema::LinqConfig {
                api_token: "x".into(),
                from_phone: "+15551234567".into(),
                allowed_senders: vec![],
                signing_secret: None,
            });
            seed.save().await.unwrap();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let code = pairing_store::mint(root, "linq", 3_600, None, true, now).unwrap();
        assert!(pairing_store::contains(root, "linq", &code, now + 1).unwrap());

        let reply = try_handle_pairing(
            &format!("/claim {code}"),
            "linq",
            AllowlistField::AllowedSenders,
            &["+1999999999".to_string()],
            root,
        )
        .await
        .expect("a /claim must be handled");
        assert!(reply.contains("owner"), "reply was: {reply}");

        let config = crate::config::Config::load_or_init().await.unwrap();
        let senders = &config
            .channels_config
            .linq
            .as_ref()
            .unwrap()
            .allowed_senders;
        assert!(
            senders.contains(&"+1999999999".to_string()),
            "senders: {senders:?}"
        );
        let owners = &config.channels_config.approval_owners;
        assert!(
            owners.contains(&"+1999999999".to_string()),
            "owners: {owners:?}"
        );

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }
}
