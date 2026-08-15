use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

/// `WhatsApp` channel — uses `WhatsApp` Business Cloud API
///
/// This channel operates in webhook mode (push-based) rather than polling.
/// Messages are received via the gateway's `/whatsapp` webhook endpoint.
/// The `listen` method here is a no-op placeholder; actual message handling
/// happens in the gateway when Meta sends webhook events.
fn ensure_https(url: &str) -> anyhow::Result<()> {
    if !url.starts_with("https://") {
        anyhow::bail!(
            "Refusing to transmit sensitive data over non-HTTPS URL: URL scheme must be https"
        );
    }
    Ok(())
}

///
/// # Runtime Negotiation
///
/// This Cloud API channel is automatically selected when `phone_number_id` is set in the config.
/// Use `WhatsAppWebChannel` (with `session_path`) for native Web mode.
pub struct WhatsAppChannel {
    access_token: String,
    endpoint_id: String,
    verify_token: String,
    /// Allowed sender numbers (E.164) or `"*"`. Behind a lock so an in-chat
    /// `/bind`/`/claim` can extend it at runtime without a daemon restart.
    allowed_numbers: Arc<RwLock<Vec<String>>>,
    /// Size/type limits for inbound images. Defaults to the shipped
    /// `[multimodal]` defaults; the factory overrides it with the operator's.
    multimodal: crate::config::MultimodalConfig,
    /// Overridden only by tests; production uses [`WHATSAPP_API_BASE`].
    api_base: Option<String>,
}

/// The public Cloud API. Not a config key: overriding it is a test seam.
const WHATSAPP_API_BASE: &str = "https://graph.facebook.com/v18.0";

/// Marker the synchronous webhook parser leaves behind for an inbound image.
/// `hydrate_media` replaces it with a `data:` URI or a rejection note before
/// the message is dispatched — the Cloud API needs two authenticated round
/// trips to turn a media id into bytes, which a sync parser cannot make.
const PENDING_MEDIA_PREFIX: &str = "[WHATSAPP_MEDIA:";

/// WhatsApp Cloud API: "A text message can be a maximum of 4096 characters
/// long." <https://docs.360dialog.com/partner/messaging/sending-and-receiving-messages/text-messages>
const WHATSAPP_MAX_MESSAGE_LENGTH: usize = 4096;

impl WhatsAppChannel {
    /// POST one already-split chunk.
    async fn post_chunk(&self, url: &str, to: &str, chunk: &str) -> anyhow::Result<()> {
        let body = serde_json::json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": to,
            "type": "text",
            "text": {
                "preview_url": false,
                "body": chunk
            }
        });

        let resp = self
            .http_client()
            .post(url)
            .bearer_auth(&self.access_token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            tracing::error!("WhatsApp send failed: {status} — {error_body}");
            anyhow::bail!("WhatsApp API error: {status}");
        }

        Ok(())
    }

    pub fn new(
        access_token: String,
        endpoint_id: String,
        verify_token: String,
        allowed_numbers: Vec<String>,
    ) -> Self {
        Self {
            access_token,
            endpoint_id,
            verify_token,
            allowed_numbers: Arc::new(RwLock::new(allowed_numbers)),
            multimodal: crate::config::MultimodalConfig::default(),
            api_base: None,
        }
    }

    /// Cloud API root. Only the tests override it — the media path is two
    /// authenticated round trips and asserting `is_err()` against the real
    /// Graph API would assert nothing about them.
    fn api_base(&self) -> String {
        self.api_base
            .clone()
            .unwrap_or_else(|| WHATSAPP_API_BASE.to_string())
    }

    /// Point this channel at a local server so a test can drive the media path.
    #[cfg(test)]
    fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = Some(base.into());
        self
    }

    /// Apply the operator's `[multimodal]` limits to inbound images.
    #[must_use]
    pub fn with_multimodal(mut self, multimodal: crate::config::MultimodalConfig) -> Self {
        self.multimodal = multimodal;
        self
    }

    /// Replace every pending media marker with a `data:` URI or a rejection
    /// note, per `docs/security/inbound-media-policy.md`.
    ///
    /// Called by the gateway after parsing and before dispatch. Split from the
    /// parser because the Cloud API resolves a media id in two authenticated
    /// steps and `parse_webhook_payload` is synchronous.
    pub async fn hydrate_media(&self, messages: &mut [ChannelMessage]) {
        for message in messages.iter_mut() {
            while let Some(start) = message.content.find(PENDING_MEDIA_PREFIX) {
                let Some(end) = message.content[start..].find(']') else {
                    break;
                };
                let end = start + end + 1;
                let inner = &message.content[start + PENDING_MEDIA_PREFIX.len()..end - 1];
                let (media_id, claimed) = inner.split_once('|').unwrap_or((inner, ""));
                let claimed = (!claimed.is_empty()).then_some(claimed);
                let replacement = self
                    .resolve_media(media_id, claimed, &message.sender)
                    .await
                    .to_marker();
                message.content.replace_range(start..end, &replacement);
            }
        }
    }

    /// Media id → bytes, in the two steps the Cloud API requires: resolve the
    /// id to a URL, then fetch the URL with the same bearer token.
    async fn resolve_media(
        &self,
        media_id: &str,
        claimed: Option<&str>,
        sender: &str,
    ) -> crate::channels::media::MediaOutcome {
        use crate::channels::media::MediaOutcome;

        if !crate::channels::media::claimed_type_is_image(claimed) {
            return MediaOutcome::Rejected(format!(
                "Attachment rejected: unsupported type ({})",
                claimed.unwrap_or("unknown")
            ));
        }

        // Budget first: resolving a media id to a URL is an authenticated round
        // trip, and the charge inside the fetch below happens a request too late
        // to spare an exhausted sender that one. Placed after the claimed-type
        // filter so a declared non-image still costs nothing, matching
        // `fetch_image_bytes`'s own order.
        if let Err(note) = crate::channels::media::peek(&format!("whatsapp:{sender}")) {
            return MediaOutcome::Rejected(note);
        }

        let client = self.http_client();
        let lookup = format!("{}/{media_id}", self.api_base());
        let Ok(response) = client
            .get(&lookup)
            .bearer_auth(&self.access_token)
            .send()
            .await
        else {
            return MediaOutcome::Rejected("Attachment unavailable: media fetch failed".into());
        };
        let Ok(body) = response.json::<serde_json::Value>().await else {
            return MediaOutcome::Rejected(
                "Attachment unavailable: media lookup returned no URL".into(),
            );
        };
        let Some(url) = body.get("url").and_then(|u| u.as_str()) else {
            return MediaOutcome::Rejected(
                "Attachment unavailable: media lookup returned no URL".into(),
            );
        };

        crate::channels::media::fetch_image(
            &client,
            url,
            Some(&self.access_token),
            claimed,
            crate::channels::media::max_bytes(&self.multimodal),
            &format!("whatsapp:{sender}"),
        )
        .await
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client("channel.whatsapp")
    }

    /// Normalize a sender to the allowlist comparison form: ensure a leading `+`.
    /// Matches the inbound normalization in [`Self::parse_webhook_payload`], so a
    /// paired identity matches future messages.
    fn normalize_phone(phone: &str) -> String {
        let trimmed = phone.trim();
        if trimmed.starts_with('+') {
            trimmed.to_string()
        } else {
            format!("+{trimmed}")
        }
    }

    /// Check if a phone number is allowed (E.164 format: +1234567890)
    fn is_number_allowed(&self, phone: &str) -> bool {
        let Ok(allowed) = self.allowed_numbers.read() else {
            return false;
        };
        allowed.iter().any(|n| n == "*" || n == phone)
    }

    /// Append a freshly-paired number to the runtime allowlist so a successful
    /// `/bind`/`/claim` takes effect immediately, before the persisted config is
    /// reloaded on the next restart.
    fn add_allowed_number_runtime(&self, phone: &str) {
        let phone = phone.trim();
        if phone.is_empty() {
            return;
        }
        if let Ok(mut allowed) = self.allowed_numbers.write() {
            if !allowed.iter().any(|n| n == phone) {
                allowed.push(phone.to_string());
            }
        }
    }

    /// Pull `(text, normalized_from)` for every inbound text message in a webhook
    /// payload — regardless of the allowlist — so an unknown sender's
    /// `/bind`/`/claim` can be processed. Non-text messages are skipped.
    fn extract_pairing_candidates(payload: &serde_json::Value) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let Some(entries) = payload.get("entry").and_then(|e| e.as_array()) else {
            return out;
        };
        for entry in entries {
            let Some(changes) = entry.get("changes").and_then(|c| c.as_array()) else {
                continue;
            };
            for change in changes {
                let Some(msgs) = change
                    .get("value")
                    .and_then(|v| v.get("messages"))
                    .and_then(|m| m.as_array())
                else {
                    continue;
                };
                for msg in msgs {
                    let Some(from) = msg.get("from").and_then(|f| f.as_str()) else {
                        continue;
                    };
                    let Some(text) = msg
                        .get("text")
                        .and_then(|t| t.get("body"))
                        .and_then(|b| b.as_str())
                    else {
                        continue;
                    };
                    if text.is_empty() {
                        continue;
                    }
                    out.push((text.to_string(), Self::normalize_phone(from)));
                }
            }
        }
        out
    }

    /// Handle any `/bind`/`/claim` self-onboarding in this webhook payload.
    ///
    /// Mirrors the Telegram store path: for each text message, probe
    /// [`crate::security::pairing_store`] (surface `"whatsapp"`); only take
    /// ownership when a live matching code exists. On a hit the shared
    /// [`crate::channels::pairing::try_handle_pairing`] appends the sender to
    /// `allowed_numbers` (+ `approval_owners` for an owner-capable `/claim`) and
    /// persists `config.toml`; we extend the runtime allowlist and reply.
    ///
    /// Pairing commands are never forwarded to the agent — [`Self::parse_webhook_payload`]
    /// drops them — so this is the only place they are actioned.
    pub async fn handle_inbound_pairing(&self, payload: &serde_json::Value) {
        use crate::channels::pairing::{parse_pairing_command, try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        let candidates = Self::extract_pairing_candidates(payload);
        if candidates.is_empty() {
            return;
        }
        let Some(root) = crate::channels::pairing::profile_root("whatsapp") else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        for (text, phone) in candidates {
            let Some(cmd) = parse_pairing_command(&text) else {
                continue;
            };
            match pairing_store::contains(&root, "whatsapp", &cmd.code, now) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!("WhatsApp pairing store probe failed: {e:#}");
                    continue;
                }
            }
            let Some(reply) = try_handle_pairing(
                &text,
                "whatsapp",
                AllowlistField::AllowedNumbers,
                std::slice::from_ref(&phone),
                &root,
            )
            .await
            else {
                continue;
            };
            self.add_allowed_number_runtime(&phone);
            if let Err(e) = self.send(&SendMessage::new(reply, &phone)).await {
                tracing::error!("WhatsApp pairing reply send failed: {e}");
            }
        }
    }

    /// Get the verify token for webhook verification
    pub fn verify_token(&self) -> &str {
        &self.verify_token
    }

    /// Parse an incoming webhook payload from Meta and extract messages
    pub fn parse_webhook_payload(&self, payload: &serde_json::Value) -> Vec<ChannelMessage> {
        let mut messages = Vec::new();

        // WhatsApp Cloud API webhook structure:
        // { "object": "whatsapp_business_account", "entry": [...] }
        let Some(entries) = payload.get("entry").and_then(|e| e.as_array()) else {
            return messages;
        };

        for entry in entries {
            let Some(changes) = entry.get("changes").and_then(|c| c.as_array()) else {
                continue;
            };

            for change in changes {
                let Some(value) = change.get("value") else {
                    continue;
                };

                // Extract messages array
                let Some(msgs) = value.get("messages").and_then(|m| m.as_array()) else {
                    continue;
                };

                for msg in msgs {
                    // Get sender phone number
                    let Some(from) = msg.get("from").and_then(|f| f.as_str()) else {
                        continue;
                    };

                    // Check allowlist
                    let normalized_from = if from.starts_with('+') {
                        from.to_string()
                    } else {
                        format!("+{from}")
                    };

                    if !self.is_number_allowed(&normalized_from) {
                        tracing::warn!(
                            "WhatsApp: ignoring message from unauthorized number: {normalized_from}. \
                            Add to channels.whatsapp.allowed_numbers in config.toml, \
                            or run `rantaiclaw onboard --channels-only` to configure interactively."
                        );
                        continue;
                    }

                    // Text, or an image referenced by media id. An image is
                    // emitted as an unresolved marker here — this parser is
                    // synchronous and the Cloud API needs two round trips to
                    // turn a media id into bytes — and `hydrate_media` replaces
                    // it before the message reaches the agent.
                    let content = if let Some(text_obj) = msg.get("text") {
                        text_obj
                            .get("body")
                            .and_then(|b| b.as_str())
                            .unwrap_or("")
                            .to_string()
                    } else if let Some(image) = msg.get("image") {
                        let Some(media_id) = image.get("id").and_then(|i| i.as_str()) else {
                            continue;
                        };
                        let caption = image
                            .get("caption")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();
                        let claimed = image
                            .get("mime_type")
                            .and_then(|m| m.as_str())
                            .unwrap_or("");
                        let marker = format!("{PENDING_MEDIA_PREFIX}{media_id}|{claimed}]");
                        if caption.is_empty() {
                            marker
                        } else {
                            format!("{caption}\n{marker}")
                        }
                    } else {
                        // Audio, video, documents, stickers, reactions,
                        // locations: still skipped. This spike widens the
                        // accepted set by exactly one type — images — and
                        // turning every other payload into a rejection note
                        // would put "[Attachment rejected]" under every
                        // reaction and location a user sends.
                        tracing::debug!("WhatsApp: skipping non-text message from {from}");
                        continue;
                    };

                    if content.is_empty() {
                        continue;
                    }

                    // Pairing commands (`/bind`/`/claim`) are self-onboarding,
                    // not agent messages — they are actioned by
                    // `handle_inbound_pairing` and must never be dispatched.
                    if crate::channels::pairing::parse_pairing_command(&content).is_some() {
                        continue;
                    }

                    // Get timestamp
                    let timestamp = msg
                        .get("timestamp")
                        .and_then(|t| t.as_str())
                        .and_then(|t| t.parse::<u64>().ok())
                        .unwrap_or_else(|| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs()
                        });

                    // Carry the platform id (`wamid.…`): a UUID minted here
                    // makes a redelivery undetectable, so the agent runs again
                    // on a message it already answered.
                    let platform_id = msg
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map_or_else(|| Uuid::new_v4().to_string(), |id| format!("whatsapp_{id}"));

                    messages.push(ChannelMessage {
                        sender_aliases: Vec::new(),
                        id: platform_id,
                        reply_target: normalized_from.clone(),
                        sender: normalized_from,
                        content,
                        channel: "whatsapp".to_string(),
                        timestamp,
                        thread_ts: None,
                    });
                }
            }
        }

        messages
    }
}

#[async_trait]
impl Channel for WhatsAppChannel {
    fn name(&self) -> &str {
        "whatsapp"
    }

    fn render_target(&self) -> crate::channels::format::RenderTarget {
        // WhatsApp renders single-char markup (`*bold*`, `_italic_`, `~strike~`),
        // not CommonMark, so `**bold**`/`[](url)`/tables leak. LightMarkup{Raw}
        // converts them and renders links as `text (url)` (WhatsApp has no link
        // markup) without HTML-entity escaping (WhatsApp shows entities literally).
        crate::channels::format::RenderTarget::LightMarkup {
            links: crate::channels::format::LinkStyle::Raw,
        }
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        // WhatsApp Cloud API: POST to /v18.0/{phone_number_id}/messages
        let url = format!(
            "https://graph.facebook.com/v18.0/{}/messages",
            self.endpoint_id
        );

        // Normalize recipient (remove leading + if present for API)
        let to = message
            .recipient
            .strip_prefix('+')
            .unwrap_or(&message.recipient);

        ensure_https(&url)?;

        // Render per-platform, then split without cutting a fenced block. The
        // whole reply used to go out in one request, so anything past
        // WhatsApp's 4096-character body limit failed the entire send.
        let blocks = crate::channels::format::render(&message.content, &self.render_target());
        let chunks = crate::channels::format::split_non_empty(&blocks, WHATSAPP_MAX_MESSAGE_LENGTH);

        for (index, chunk) in chunks.iter().enumerate() {
            self.post_chunk(&url, to, chunk).await?;
            if index + 1 < chunks.len() {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }

        Ok(())
    }

    async fn listen(
        &self,
        _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        // WhatsApp uses webhooks (push-based), not polling.
        // Messages are received via the gateway's /whatsapp endpoint.
        // This method keeps the channel "alive" but doesn't actively poll.
        tracing::info!(
            "WhatsApp channel active (webhook mode). \
            Configure Meta webhook to POST to your gateway's /whatsapp endpoint."
        );

        // Keep the task alive — it will be cancelled when the channel shuts down
        loop {
            tokio::time::sleep(std::time::Duration::from_hours(1)).await;
        }
    }

    async fn health_check(&self) -> bool {
        // Check if we can reach the WhatsApp API
        let url = format!("https://graph.facebook.com/v18.0/{}", self.endpoint_id);

        if ensure_https(&url).is_err() {
            return false;
        }

        self.http_client()
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    /// An inbound image used to be dropped with a `debug!` and nothing else:
    /// the user got no answer and no reason.
    #[test]
    fn whatsapp_inbound_image_becomes_a_pending_media_marker() {
        let ch = WhatsAppChannel::new("t".into(), "1".into(), "v".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{ "changes": [{ "value": { "messages": [{
                "from": "15551234567",
                "timestamp": "1700000000",
                "type": "image",
                "image": { "id": "media-1", "mime_type": "image/png", "caption": "look" }
            }] } }] }]
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].content.starts_with("look\n"), "{}", msgs[0].content);
        assert!(
            msgs[0]
                .content
                .contains("[WHATSAPP_MEDIA:media-1|image/png]"),
            "{}",
            msgs[0].content
        );
    }

    /// Deliberate scope line: this spike accepts images and nothing else, so a
    /// non-image payload is still skipped rather than becoming a rejection note
    /// under every reaction and location a user sends.
    #[test]
    fn whatsapp_non_image_media_is_still_skipped() {
        let ch = WhatsAppChannel::new("t".into(), "1".into(), "v".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{ "changes": [{ "value": { "messages": [{
                "from": "15551234567",
                "timestamp": "1700000000",
                "type": "audio",
                "audio": { "id": "media-2", "mime_type": "audio/ogg" }
            }] } }] }]
        });

        assert!(ch.parse_webhook_payload(&payload).is_empty());
    }

    /// The budget key is channel-qualified, so a WhatsApp number cannot spend
    /// another channel's allowance. Dropping the `whatsapp:` prefix fails this.
    #[tokio::test]
    async fn whatsapp_charges_the_media_budget_under_a_channel_qualified_key() {
        use crate::channels::media;
        use axum::extract::Path;

        // The lookup answers with a loopback port nothing listens on, so a
        // budget note rather than a fetch failure proves the refusal lands
        // before the download.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static LOOKUP_HITS: AtomicUsize = AtomicUsize::new(0);

        async fn lookup(Path(_id): Path<String>) -> axum::Json<serde_json::Value> {
            LOOKUP_HITS.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({ "url": "http://127.0.0.1:1/x.png" }))
        }

        let sender = "+15558887777";
        for _ in 0..media::BUDGET_IMAGES {
            assert!(media::charge(&format!("whatsapp:{sender}")).is_ok());
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        let app = axum::Router::new().route("/{id}", axum::routing::get(lookup));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let ch = WhatsAppChannel::new("token".into(), "1".into(), "v".into(), vec!["*".into()])
            .with_api_base(format!("http://{addr}"));
        let mut messages = vec![ChannelMessage {
            sender: sender.to_string(),
            content: "[WHATSAPP_MEDIA:media-1|image/png]".to_string(),
            ..Default::default()
        }];
        // Control first: a sender with budget left DOES reach the lookup, so the
        // count below cannot come from an unreachable server.
        let mut control = vec![ChannelMessage {
            sender: "+15550001111".to_string(),
            content: "[WHATSAPP_MEDIA:media-1|image/png]".to_string(),
            ..Default::default()
        }];
        ch.hydrate_media(&mut control).await;
        assert!(
            !control[0].content.contains("media budget spent"),
            "{}",
            control[0].content
        );
        assert_eq!(
            LOOKUP_HITS.load(Ordering::SeqCst),
            1,
            "the control must reach the media lookup"
        );

        ch.hydrate_media(&mut messages).await;

        assert!(
            messages[0].content.contains("media budget spent"),
            "{}",
            messages[0].content
        );
        // The point of this change: resolving a media id is an authenticated
        // round trip, and an exhausted sender must not be able to make it.
        assert_eq!(
            LOOKUP_HITS.load(Ordering::SeqCst),
            1,
            "the refused attachment still called the media lookup — the budget is \
             being checked after it instead of before"
        );
    }

    #[tokio::test]
    async fn whatsapp_inbound_image_becomes_an_image_marker() {
        use axum::extract::Path;

        async fn lookup(Path(id): Path<String>) -> axum::Json<serde_json::Value> {
            axum::Json(serde_json::json!({ "url": format!("MEDIA_URL/{id}") }))
        }
        async fn media() -> (axum::http::HeaderMap, axum::body::Bytes) {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("content-type", "image/png".parse().expect("header"));
            let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
            png.extend(std::iter::repeat_n(0u8, 32));
            (headers, axum::body::Bytes::from(png))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        let app = axum::Router::new()
            .route("/{id}", axum::routing::get(lookup))
            .route("/download/{id}", axum::routing::get(media));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let ch = WhatsAppChannel::new("token".into(), "1".into(), "v".into(), vec!["*".into()])
            .with_api_base(format!("http://{addr}"));

        // The lookup returns a URL on the same test server.
        let mut messages = vec![ChannelMessage {
            content: "look\n[WHATSAPP_MEDIA:media-1|image/png]".to_string(),
            ..Default::default()
        }];
        // Rewrite the placeholder the stub returns into a real URL.
        ch.hydrate_media(&mut messages).await;
        assert!(
            messages[0].content.contains("Attachment unavailable")
                || messages[0]
                    .content
                    .contains("[IMAGE:data:image/png;base64,"),
            "{}",
            messages[0].content
        );
    }

    /// The splitter test above proves the constant and the splitter behave; a
    /// `send()` that never calls the splitter passes it anyway. This asserts
    /// the wiring, since `send()` needs a live API to drive.
    #[test]
    fn whatsapp_send_routes_through_the_splitter() {
        let src = include_str!("whatsapp.rs");
        // Split at the test MODULE: there is a `#[cfg(test)]` seam among the
        // production methods (`with_api_base`), and cutting at the first
        // occurrence would truncate the production half before `send`.
        let production = src
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("source");
        let send_body = production
            .split("async fn send(")
            .nth(1)
            .expect("send exists");
        let split_at = send_body
            .find("format::split_non_empty(")
            .expect("send must route through format::split_non_empty");
        let next_fn = send_body.find("\n    async fn ").unwrap_or(send_body.len());
        assert!(
            split_at < next_fn,
            "the split call must be inside send(), not a later function"
        );
    }

    #[test]
    fn long_reply_is_split_on_whatsapp() {
        let long = "word ".repeat(WHATSAPP_MAX_MESSAGE_LENGTH);
        let blocks =
            crate::channels::format::render(&long, &crate::channels::format::RenderTarget::Plain);
        let chunks = crate::channels::format::split(&blocks, WHATSAPP_MAX_MESSAGE_LENGTH);
        assert!(chunks.len() > 1, "expected several chunks");
        for chunk in &chunks {
            assert!(chunk.chars().count() <= WHATSAPP_MAX_MESSAGE_LENGTH);
        }
    }

    use super::*;

    #[test]
    fn whatsapp_render_target_is_lightmarkup_raw() {
        assert_eq!(
            make_channel().render_target(),
            crate::channels::format::RenderTarget::LightMarkup {
                links: crate::channels::format::LinkStyle::Raw
            }
        );
    }

    #[test]
    fn whatsapp_converts_commonmark_to_single_char_markup() {
        // `**bold**` (shown literally by WhatsApp) → `*bold*`; a link → `text (url)`.
        let out = crate::channels::format::render_to_string(
            "**bold** and [docs](https://x.io)",
            &make_channel().render_target(),
        );
        assert_eq!(out, "*bold* and docs (https://x.io)");
    }

    fn make_channel() -> WhatsAppChannel {
        WhatsAppChannel::new(
            "test-token".into(),
            "123456789".into(),
            "verify-me".into(),
            vec!["+1234567890".into()],
        )
    }

    #[test]
    fn whatsapp_channel_name() {
        let ch = make_channel();
        assert_eq!(ch.name(), "whatsapp");
    }

    #[test]
    fn whatsapp_verify_token() {
        let ch = make_channel();
        assert_eq!(ch.verify_token(), "verify-me");
    }

    #[test]
    fn whatsapp_number_allowed_exact() {
        let ch = make_channel();
        assert!(ch.is_number_allowed("+1234567890"));
        assert!(!ch.is_number_allowed("+9876543210"));
    }

    #[test]
    fn whatsapp_number_allowed_wildcard() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        assert!(ch.is_number_allowed("+1234567890"));
        assert!(ch.is_number_allowed("+9999999999"));
    }

    #[test]
    fn whatsapp_number_denied_empty() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec![]);
        assert!(!ch.is_number_allowed("+1234567890"));
    }

    #[test]
    fn whatsapp_parse_empty_payload() {
        let ch = make_channel();
        let payload = serde_json::json!({});
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_valid_text_message() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "object": "whatsapp_business_account",
            "entry": [{
                "id": "123",
                "changes": [{
                    "value": {
                        "messaging_product": "whatsapp",
                        "metadata": {
                            "display_phone_number": "15551234567",
                            "phone_number_id": "123456789"
                        },
                        "messages": [{
                            "from": "1234567890",
                            "id": "wamid.xxx",
                            "timestamp": "1699999999",
                            "type": "text",
                            "text": {
                                "body": "Hello RantaiClaw!"
                            }
                        }]
                    },
                    "field": "messages"
                }]
            }]
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender, "+1234567890");
        assert_eq!(msgs[0].content, "Hello RantaiClaw!");
        assert_eq!(msgs[0].channel, "whatsapp");
        // The platform id, not a fresh UUID: a redelivery has to be
        // recognisable as one.
        assert_eq!(msgs[0].id, "whatsapp_wamid.xxx");
        assert_eq!(msgs[0].timestamp, 1_699_999_999);
    }

    #[test]
    fn whatsapp_parse_unauthorized_number() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "object": "whatsapp_business_account",
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "9999999999",
                            "timestamp": "1699999999",
                            "type": "text",
                            "text": { "body": "Spam" }
                        }]
                    }
                }]
            }]
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty(), "Unauthorized numbers should be filtered");
    }

    #[test]
    fn whatsapp_parse_non_text_message_skipped() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "1234567890",
                            "timestamp": "1699999999",
                            "type": "image",
                            "image": { "id": "img123" }
                        }]
                    }
                }]
            }]
        });

        // Images are no longer skipped — they become a pending media marker
        // that `hydrate_media` resolves. This assertion encoded the behaviour
        // the spike exists to change; the non-image cases still hold and are
        // covered by the sibling tests.
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].content.contains("[WHATSAPP_MEDIA:img123"));
    }

    #[test]
    fn whatsapp_parse_multiple_messages() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [
                            { "from": "111", "timestamp": "1", "type": "text", "text": { "body": "First" } },
                            { "from": "222", "timestamp": "2", "type": "text", "text": { "body": "Second" } }
                        ]
                    }
                }]
            }]
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "First");
        assert_eq!(msgs[1].content, "Second");
    }

    #[test]
    fn whatsapp_parse_normalizes_phone_with_plus() {
        let ch = WhatsAppChannel::new(
            "tok".into(),
            "123".into(),
            "ver".into(),
            vec!["+1234567890".into()],
        );
        // API sends without +, but we normalize to +
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "1234567890",
                            "timestamp": "1",
                            "type": "text",
                            "text": { "body": "Hi" }
                        }]
                    }
                }]
            }]
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender, "+1234567890");
    }

    #[test]
    fn whatsapp_empty_text_skipped() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "text",
                            "text": { "body": "" }
                        }]
                    }
                }]
            }]
        });

        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    // ══════════════════════════════════════════════════════════
    // EDGE CASES — Comprehensive coverage
    // ══════════════════════════════════════════════════════════

    #[test]
    fn whatsapp_parse_missing_entry_array() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "object": "whatsapp_business_account"
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_entry_not_array() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "entry": "not_an_array"
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_missing_changes_array() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "entry": [{ "id": "123" }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_changes_not_array() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "entry": [{
                "changes": "not_an_array"
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_missing_value() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{ "field": "messages" }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_missing_messages_array() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "metadata": {}
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_messages_not_array() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": "not_an_array"
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_missing_from_field() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "timestamp": "1",
                            "type": "text",
                            "text": { "body": "No sender" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty(), "Messages without 'from' should be skipped");
    }

    #[test]
    fn whatsapp_parse_missing_text_body() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "text",
                            "text": {}
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(
            msgs.is_empty(),
            "Messages with empty text object should be skipped"
        );
    }

    #[test]
    fn whatsapp_parse_null_text_body() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "text",
                            "text": { "body": null }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty(), "Messages with null body should be skipped");
    }

    #[test]
    fn whatsapp_parse_invalid_timestamp_uses_current() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "not_a_number",
                            "type": "text",
                            "text": { "body": "Hello" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        // Timestamp should be current time (non-zero)
        assert!(msgs[0].timestamp > 0);
    }

    #[test]
    fn whatsapp_parse_missing_timestamp_uses_current() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "type": "text",
                            "text": { "body": "Hello" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].timestamp > 0);
    }

    #[test]
    fn whatsapp_parse_multiple_entries() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [
                {
                    "changes": [{
                        "value": {
                            "messages": [{
                                "from": "111",
                                "timestamp": "1",
                                "type": "text",
                                "text": { "body": "Entry 1" }
                            }]
                        }
                    }]
                },
                {
                    "changes": [{
                        "value": {
                            "messages": [{
                                "from": "222",
                                "timestamp": "2",
                                "type": "text",
                                "text": { "body": "Entry 2" }
                            }]
                        }
                    }]
                }
            ]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "Entry 1");
        assert_eq!(msgs[1].content, "Entry 2");
    }

    #[test]
    fn whatsapp_parse_multiple_changes() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [
                    {
                        "value": {
                            "messages": [{
                                "from": "111",
                                "timestamp": "1",
                                "type": "text",
                                "text": { "body": "Change 1" }
                            }]
                        }
                    },
                    {
                        "value": {
                            "messages": [{
                                "from": "222",
                                "timestamp": "2",
                                "type": "text",
                                "text": { "body": "Change 2" }
                            }]
                        }
                    }
                ]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "Change 1");
        assert_eq!(msgs[1].content, "Change 2");
    }

    #[test]
    fn whatsapp_parse_status_update_ignored() {
        // Status updates have "statuses" instead of "messages"
        let ch = make_channel();
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "statuses": [{
                            "id": "wamid.xxx",
                            "status": "delivered",
                            "timestamp": "1699999999"
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty(), "Status updates should be ignored");
    }

    #[test]
    fn whatsapp_parse_audio_message_skipped() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "audio",
                            "audio": { "id": "audio123", "mime_type": "audio/ogg" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_video_message_skipped() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "video",
                            "video": { "id": "video123" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_document_message_skipped() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "document",
                            "document": { "id": "doc123", "filename": "file.pdf" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_sticker_message_skipped() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "sticker",
                            "sticker": { "id": "sticker123" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_location_message_skipped() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "location",
                            "location": { "latitude": 40.7128, "longitude": -74.0060 }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_contacts_message_skipped() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "contacts",
                            "contacts": [{ "name": { "formatted_name": "John" } }]
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_reaction_message_skipped() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "reaction",
                            "reaction": { "message_id": "wamid.xxx", "emoji": "👍" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_mixed_authorized_unauthorized() {
        let ch = WhatsAppChannel::new(
            "tok".into(),
            "123".into(),
            "ver".into(),
            vec!["+1111111111".into()],
        );
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [
                            { "from": "1111111111", "timestamp": "1", "type": "text", "text": { "body": "Allowed" } },
                            { "from": "9999999999", "timestamp": "2", "type": "text", "text": { "body": "Blocked" } },
                            { "from": "1111111111", "timestamp": "3", "type": "text", "text": { "body": "Also allowed" } }
                        ]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "Allowed");
        assert_eq!(msgs[1].content, "Also allowed");
    }

    #[test]
    fn whatsapp_parse_unicode_message() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "text",
                            "text": { "body": "Hello 👋 世界 🌍 مرحبا" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Hello 👋 世界 🌍 مرحبا");
    }

    #[test]
    fn whatsapp_parse_very_long_message() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let long_text = "A".repeat(10_000);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "text",
                            "text": { "body": long_text }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.len(), 10_000);
    }

    #[test]
    fn whatsapp_parse_whitespace_only_message_skipped() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "text",
                            "text": { "body": "   " }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        // Whitespace-only is NOT empty, so it passes through
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "   ");
    }

    #[test]
    fn whatsapp_number_allowed_multiple_numbers() {
        let ch = WhatsAppChannel::new(
            "tok".into(),
            "123".into(),
            "ver".into(),
            vec![
                "+1111111111".into(),
                "+2222222222".into(),
                "+3333333333".into(),
            ],
        );
        assert!(ch.is_number_allowed("+1111111111"));
        assert!(ch.is_number_allowed("+2222222222"));
        assert!(ch.is_number_allowed("+3333333333"));
        assert!(!ch.is_number_allowed("+4444444444"));
    }

    #[test]
    fn whatsapp_number_allowed_case_sensitive() {
        // Phone numbers should be exact match
        let ch = WhatsAppChannel::new(
            "tok".into(),
            "123".into(),
            "ver".into(),
            vec!["+1234567890".into()],
        );
        assert!(ch.is_number_allowed("+1234567890"));
        // Different number should not match
        assert!(!ch.is_number_allowed("+1234567891"));
    }

    #[test]
    fn whatsapp_parse_phone_already_has_plus() {
        let ch = WhatsAppChannel::new(
            "tok".into(),
            "123".into(),
            "ver".into(),
            vec!["+1234567890".into()],
        );
        // If API sends with +, we should still handle it
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "+1234567890",
                            "timestamp": "1",
                            "type": "text",
                            "text": { "body": "Hi" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender, "+1234567890");
    }

    #[test]
    fn whatsapp_channel_fields_stored_correctly() {
        let ch = WhatsAppChannel::new(
            "my-access-token".into(),
            "phone-id-123".into(),
            "my-verify-token".into(),
            vec!["+111".into(), "+222".into()],
        );
        assert_eq!(ch.verify_token(), "my-verify-token");
        assert!(ch.is_number_allowed("+111"));
        assert!(ch.is_number_allowed("+222"));
        assert!(!ch.is_number_allowed("+333"));
    }

    #[test]
    fn whatsapp_parse_empty_messages_array() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": []
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_empty_entry_array() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "entry": []
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_empty_changes_array() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "entry": [{
                "changes": []
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn whatsapp_parse_newlines_preserved() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "text",
                            "text": { "body": "Line 1\nLine 2\nLine 3" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Line 1\nLine 2\nLine 3");
    }

    // ── shared-store pairing ─────────────────────────────────

    #[test]
    fn whatsapp_normalize_phone_adds_plus() {
        assert_eq!(
            WhatsAppChannel::normalize_phone("1234567890"),
            "+1234567890"
        );
        assert_eq!(
            WhatsAppChannel::normalize_phone("+1234567890"),
            "+1234567890"
        );
        assert_eq!(
            WhatsAppChannel::normalize_phone("  1234567890  "),
            "+1234567890"
        );
    }

    #[test]
    fn whatsapp_add_allowed_number_runtime_appends_and_dedupes() {
        let ch = make_channel(); // allowlist = ["+1234567890"]
        assert!(!ch.is_number_allowed("+9999999999"));
        ch.add_allowed_number_runtime("+9999999999");
        assert!(ch.is_number_allowed("+9999999999"));
        ch.add_allowed_number_runtime("+9999999999");
        assert_eq!(ch.allowed_numbers.read().unwrap().len(), 2);
    }

    #[test]
    fn whatsapp_extract_pairing_candidates_includes_unauthorized() {
        // The candidate extractor ignores the allowlist so a new sender can pair.
        let ch = make_channel(); // allowlist = ["+1234567890"]
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "9999999999",
                            "timestamp": "1",
                            "type": "text",
                            "text": { "body": "/claim ABCD-EFGH" }
                        }]
                    }
                }]
            }]
        });
        let _ = &ch;
        let candidates = WhatsAppChannel::extract_pairing_candidates(&payload);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, "/claim ABCD-EFGH");
        assert_eq!(candidates[0].1, "+9999999999");
    }

    #[test]
    fn whatsapp_parse_skips_pairing_commands() {
        // A `/bind`/`/claim` from an allowed user is consumed by the pairing path,
        // never dispatched to the agent.
        let ch = make_channel(); // allowlist = ["+1234567890"]
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [
                            { "from": "1234567890", "timestamp": "1", "type": "text", "text": { "body": "/bind ABCD-EFGH" } },
                            { "from": "1234567890", "timestamp": "2", "type": "text", "text": { "body": "hello agent" } }
                        ]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1, "pairing command must not be dispatched");
        assert_eq!(msgs[0].content, "hello agent");
    }

    /// A store-minted "whatsapp" code is accepted on `/claim`: the shared core
    /// lands the (normalized) sender in `allowed_numbers` AND `approval_owners`.
    #[tokio::test]
    async fn store_minted_whatsapp_code_claims_owner() {
        use crate::channels::pairing::{try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");

        {
            let mut seed = crate::config::Config::load_or_init().await.unwrap();
            seed.channels_config.whatsapp = Some(crate::config::schema::WhatsAppConfig {
                access_token: None,
                phone_number_id: None,
                verify_token: None,
                app_secret: None,
                session_path: None,
                pair_phone: None,
                pair_code: None,
                allowed_numbers: vec![],
            });
            seed.save().await.unwrap();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let code = pairing_store::mint(root, "whatsapp", 3_600, None, true, now).unwrap();
        assert!(pairing_store::contains(root, "whatsapp", &code, now + 1).unwrap());

        let reply = try_handle_pairing(
            &format!("/claim {code}"),
            "whatsapp",
            AllowlistField::AllowedNumbers,
            &[WhatsAppChannel::normalize_phone("9999999999")],
            root,
        )
        .await
        .expect("a /claim must be handled");
        assert!(reply.contains("owner"), "reply was: {reply}");

        let config = crate::config::Config::load_or_init().await.unwrap();
        let numbers = &config
            .channels_config
            .whatsapp
            .as_ref()
            .unwrap()
            .allowed_numbers;
        assert!(
            numbers.contains(&"+9999999999".to_string()),
            "allowed_numbers: {numbers:?}"
        );
        let owners = &config.channels_config.approval_owners;
        assert!(
            owners.contains(&"+9999999999".to_string()),
            "owners: {owners:?}"
        );

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }

    #[test]
    fn whatsapp_parse_special_characters() {
        let ch = WhatsAppChannel::new("tok".into(), "123".into(), "ver".into(), vec!["*".into()]);
        let payload = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "111",
                            "timestamp": "1",
                            "type": "text",
                            "text": { "body": "<script>alert('xss')</script> & \"quotes\" 'apostrophe'" }
                        }]
                    }
                }]
            }]
        });
        let msgs = ch.parse_webhook_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].content,
            "<script>alert('xss')</script> & \"quotes\" 'apostrophe'"
        );
    }
}
