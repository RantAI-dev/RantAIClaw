use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

const QQ_API_BASE: &str = "https://api.sgroup.qq.com";
/// Token endpoint the App ID + App Secret are exchanged at.
///
/// `pub(crate)` on purpose: the setup provisioner validates the operator's
/// credentials against the same endpoint. Keeping one constant is what stops the
/// provisioner probing a host the channel never contacts.
pub(crate) const QQ_AUTH_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

fn ensure_https(url: &str) -> anyhow::Result<()> {
    if !url.starts_with("https://") {
        anyhow::bail!(
            "Refusing to transmit sensitive data over non-HTTPS URL: URL scheme must be https"
        );
    }
    Ok(())
}

/// Deduplication set capacity — evict half of entries when full.
const DEDUP_CAPACITY: usize = 10_000;

/// QQ Official Bot channel — uses Tencent's official QQ Bot API with
/// OAuth2 authentication and a Discord-like WebSocket gateway protocol.
pub struct QQChannel {
    app_id: String,
    app_secret: String,
    /// Runtime-mutable so a `/bind`/`/claim` — or a console allowlist edit —
    /// reaches the running channel instead of waiting for a restart.
    allowed_users: Arc<std::sync::RwLock<Vec<String>>>,
    /// Cached access token + expiry timestamp.
    token_cache: Arc<RwLock<Option<(String, u64)>>>,
    /// Message deduplication: the set answers membership in O(1), the deque
    /// records arrival order so eviction drops the *oldest* id. It used to be
    /// a bare `HashSet` evicted by `iter().take(..)` — whose order is
    /// unspecified — so a just-inserted id could be dropped, and a dedup miss
    /// costs a complete extra LLM turn plus a duplicate reply.
    dedup: Arc<RwLock<(VecDeque<String>, HashSet<String>)>>,
}

impl QQChannel {
    pub fn new(app_id: String, app_secret: String, allowed_users: Vec<String>) -> Self {
        Self {
            app_id,
            app_secret,
            allowed_users: Arc::new(std::sync::RwLock::new(allowed_users)),
            token_cache: Arc::new(RwLock::new(None)),
            dedup: Arc::new(RwLock::new((VecDeque::new(), HashSet::new()))),
        }
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client("channel.qq")
    }

    fn is_user_allowed(&self, user_id: &str) -> bool {
        let Ok(users) = self.allowed_users.read() else {
            return false;
        };
        users.iter().any(|u| u == "*" || u == user_id)
    }

    /// Append a freshly-paired openid to the runtime allowlist so access is
    /// effective immediately. The persisted config (saved by the pairing core)
    /// stays the source of truth across restarts.
    fn add_allowed_identity_runtime(&self, id: &str) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        if let Ok(mut users) = self.allowed_users.write() {
            if !users.iter().any(|u| u == id) {
                users.push(id.to_string());
            }
        }
    }

    /// Self-onboarding hook: if `content` is a `/bind`/`/claim` command, validate
    /// it against the shared [`crate::security::pairing_store`] (appending the
    /// sender openid to `allowed_users` and, for an owner-capable `/claim`, to
    /// `approval_owners`, then persisting `config.toml`) and reply to the chat.
    /// Shared by the C2C and group reject points; `chat_id` is the pre-formatted
    /// `user:`/`group:` reply target.
    ///
    /// Returns `true` when the message WAS a pairing command (handled here — must
    /// NOT be forwarded to the agent), `false` otherwise (normal message → gate).
    async fn try_handle_store_pairing(&self, content: &str, user_id: &str, chat_id: &str) -> bool {
        use crate::channels::pairing::{parse_pairing_command, try_handle_pairing, AllowlistField};

        if parse_pairing_command(content).is_none() {
            return false;
        }
        let Some(root) = crate::channels::pairing::profile_root("qq") else {
            return false;
        };

        let Some(reply) = try_handle_pairing(
            content,
            "qq",
            AllowlistField::AllowedUsers,
            &[user_id.to_string()],
            &root,
        )
        .await
        else {
            return false;
        };

        // Effective immediately; the pairing core persists it for next start.
        self.add_allowed_identity_runtime(user_id);

        if let Err(e) = self.send(&SendMessage::new(reply, chat_id)).await {
            tracing::warn!("QQ pairing: failed to send reply: {e:#}");
        }
        true
    }

    /// Fetch an access token from QQ's OAuth2 endpoint.
    async fn fetch_access_token(&self) -> anyhow::Result<(String, u64)> {
        let body = json!({
            "appId": self.app_id,
            "clientSecret": self.app_secret,
        });

        let resp = self
            .http_client()
            .post(QQ_AUTH_URL)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("QQ token request failed ({status}): {err}");
        }

        let data: serde_json::Value = resp.json().await?;
        let token = data
            .get("access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing access_token in QQ response"))?
            .to_string();

        let expires_in = data
            .get("expires_in")
            .and_then(|e| e.as_str())
            .and_then(|e| e.parse::<u64>().ok())
            .unwrap_or(7200);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Expire 60 seconds early to avoid edge cases
        let expiry = now + expires_in.saturating_sub(60);

        Ok((token, expiry))
    }

    /// Get a valid access token, refreshing if expired.
    async fn get_token(&self) -> anyhow::Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        {
            let cache = self.token_cache.read().await;
            if let Some((ref token, expiry)) = *cache {
                if now < expiry {
                    return Ok(token.clone());
                }
            }
        }

        let (token, expiry) = self.fetch_access_token().await?;
        {
            let mut cache = self.token_cache.write().await;
            *cache = Some((token.clone(), expiry));
        }
        Ok(token)
    }

    /// Get the WebSocket gateway URL.
    async fn get_gateway_url(&self, token: &str) -> anyhow::Result<String> {
        let resp = self
            .http_client()
            .get(format!("{QQ_API_BASE}/gateway"))
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("QQ gateway request failed ({status}): {err}");
        }

        let data: serde_json::Value = resp.json().await?;
        let url = data
            .get("url")
            .and_then(|u| u.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing gateway URL in QQ response"))?
            .to_string();

        Ok(url)
    }

    /// Check and insert message ID for deduplication.
    async fn is_duplicate(&self, msg_id: &str) -> bool {
        if msg_id.is_empty() {
            return false;
        }

        let mut guard = self.dedup.write().await;
        let (order, seen) = &mut *guard;

        if seen.contains(msg_id) {
            return true;
        }

        seen.insert(msg_id.to_string());
        order.push_back(msg_id.to_string());
        while order.len() > DEDUP_CAPACITY {
            if let Some(oldest) = order.pop_front() {
                seen.remove(&oldest);
            }
        }
        false
    }

    /// Decide who sent a dispatch event, where a reply to it goes, and whether
    /// it is worth routing at all.
    ///
    /// Pure, and deliberately so: the two message events differ only in where
    /// the sender identity lives and how the reply anchor is spelled, and that
    /// difference is the whole of what a test needs to reach. Everything the
    /// caller does with the result — dedup, pairing, the allowlist gate — needs
    /// the network or shared state, which is why it stays in `listen`.
    fn classify_inbound(event_type: &str, d: &serde_json::Value) -> QqInbound {
        let content = d
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            return QqInbound::Ignore;
        }
        let author = |field: &str| {
            d.get("author")
                .and_then(|a| a.get(field))
                .and_then(serde_json::Value::as_str)
        };

        let (kind, sender, chat_id) = match event_type {
            "C2C_MESSAGE_CREATE" => {
                // For QQ, user_openid is the identifier; `author.id` is the
                // fallback the event carries when it is absent.
                let author_id = author("id").unwrap_or("unknown");
                let user_openid = author("user_openid").unwrap_or(author_id);
                ("C2C", user_openid, format!("user:{user_openid}"))
            }
            "GROUP_AT_MESSAGE_CREATE" => {
                let member_openid = author("member_openid").unwrap_or("unknown");
                let group_openid = d
                    .get("group_openid")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                ("group", member_openid, format!("group:{group_openid}"))
            }
            _ => return QqInbound::Ignore,
        };

        QqInbound::Route(QqRouted {
            kind,
            sender: sender.to_string(),
            chat_id,
            content: content.to_string(),
        })
    }
}

/// What [`QQChannel::classify_inbound`] decided about one dispatch event.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum QqInbound {
    /// Not a message this channel routes: an event type we do not handle, or a
    /// message with nothing in it.
    Ignore,
    Route(QqRouted),
}

/// A routable QQ message, before dedup, pairing, and the allowlist gate.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct QqRouted {
    /// How to name this event in an operator-facing log line.
    pub kind: &'static str,
    /// The openid the allowlist and the agent both key on.
    pub sender: String,
    /// The reply anchor: `user:<openid>` for C2C, `group:<openid>` for a group.
    pub chat_id: String,
    pub content: String,
}

#[async_trait]
impl Channel for QQChannel {
    fn name(&self) -> &str {
        "qq"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let token = self.get_token().await?;

        // QQ text bubbles render no markup — strip to readable text. Both the
        // group and user bind points use the same rendered string.
        let rendered = crate::channels::format::render_to_string(
            &message.content,
            &crate::channels::format::RenderTarget::Plain,
        );

        // Determine if this is a group or private message based on recipient format
        // Format: "user:{openid}" or "group:{group_openid}"
        let (url, body) = if let Some(group_id) = message.recipient.strip_prefix("group:") {
            (
                format!("{QQ_API_BASE}/v2/groups/{group_id}/messages"),
                json!({
                    "content": &rendered,
                    "msg_type": 0,
                }),
            )
        } else {
            let user_id = message
                .recipient
                .strip_prefix("user:")
                .unwrap_or(&message.recipient);
            (
                format!("{QQ_API_BASE}/v2/users/{user_id}/messages"),
                json!({
                    "content": &rendered,
                    "msg_type": 0,
                }),
            )
        };

        ensure_https(&url)?;

        let resp = self
            .http_client()
            .post(&url)
            .header("Authorization", format!("QQBot {token}"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("QQ send message failed ({status}): {err}");
        }

        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        tracing::info!("QQ: authenticating...");
        let token = self.get_token().await?;

        tracing::info!("QQ: fetching gateway URL...");
        let gw_url = self.get_gateway_url(&token).await?;

        tracing::info!("QQ: connecting to gateway WebSocket...");
        let (ws_stream, _) = tokio_tungstenite::connect_async(&gw_url).await?;
        let (mut write, mut read) = ws_stream.split();

        // Read Hello (opcode 10)
        let hello = read
            .next()
            .await
            .ok_or(anyhow::anyhow!("QQ: no hello frame"))??;
        let hello_data: serde_json::Value = serde_json::from_str(&hello.to_string())?;
        let heartbeat_interval = hello_data
            .get("d")
            .and_then(|d| d.get("heartbeat_interval"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(41250);

        // Send Identify (opcode 2)
        // Intents: PUBLIC_GUILD_MESSAGES (1<<30) | C2C_MESSAGE_CREATE & GROUP_AT_MESSAGE_CREATE (1<<25)
        let intents: u64 = (1 << 25) | (1 << 30);
        let identify = json!({
            "op": 2,
            "d": {
                "token": format!("QQBot {token}"),
                "intents": intents,
                "properties": {
                    "os": "linux",
                    "browser": "rantaiclaw",
                    "device": "rantaiclaw",
                }
            }
        });
        write
            .send(Message::Text(identify.to_string().into()))
            .await?;

        tracing::info!("QQ: connected and identified");

        let mut sequence: i64 = -1;

        // Spawn heartbeat timer
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

        loop {
            tokio::select! {
                _ = hb_rx.recv() => {
                    let d = if sequence >= 0 { json!(sequence) } else { json!(null) };
                    let hb = json!({"op": 1, "d": d});
                    if write
                        .send(Message::Text(hb.to_string().into()))
                        .await
                        .is_err()
                    {
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

                    // Track sequence number
                    if let Some(s) = event.get("s").and_then(serde_json::Value::as_i64) {
                        sequence = s;
                    }

                    let op = event.get("op").and_then(serde_json::Value::as_u64).unwrap_or(0);

                    match op {
                        // Server requests immediate heartbeat
                        1 => {
                            let d = if sequence >= 0 { json!(sequence) } else { json!(null) };
                            let hb = json!({"op": 1, "d": d});
                            if write
                                .send(Message::Text(hb.to_string().into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                        // Reconnect
                        7 => {
                            tracing::warn!("QQ: received Reconnect (op 7)");
                            break;
                        }
                        // Invalid Session
                        9 => {
                            tracing::warn!("QQ: received Invalid Session (op 9)");
                            break;
                        }
                        _ => {}
                    }

                    // Only process dispatch events (op 0)
                    if op != 0 {
                        continue;
                    }

                    let event_type = event.get("t").and_then(|t| t.as_str()).unwrap_or("");
                    let d = match event.get("d") {
                        Some(d) => d,
                        None => continue,
                    };

                    let msg_id = d.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    if self.is_duplicate(msg_id).await {
                        continue;
                    }

                    // Who sent it and where a reply goes — the only part of this
                    // that a test can reach without a gateway socket.
                    let QqInbound::Route(routed) = Self::classify_inbound(event_type, d) else {
                        continue;
                    };

                    // Intercept on-demand store-minted `/bind`/`/claim`
                    // pairing codes before the allowlist gate so unenrolled
                    // users can self-onboard without a daemon restart.
                    if self
                        .try_handle_store_pairing(&routed.content, &routed.sender, &routed.chat_id)
                        .await
                    {
                        continue;
                    }

                    if !self.is_user_allowed(&routed.sender) {
                        tracing::warn!(
                            "QQ: ignoring {} message from unauthorized user: {}",
                            routed.kind,
                            routed.sender
                        );
                        continue;
                    }

                    let channel_msg = ChannelMessage {
                        sender_aliases: Vec::new(),
                        id: Uuid::new_v4().to_string(),
                        sender: routed.sender,
                        reply_target: routed.chat_id,
                        content: routed.content,
                        channel: "qq".to_string(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        thread_ts: None,
                    };

                    if tx.send(channel_msg).await.is_err() {
                        tracing::warn!("QQ: message channel closed");
                        break;
                    }
                }
            }
        }

        anyhow::bail!("QQ WebSocket connection closed")
    }

    fn apply_allowed_senders(&self, allowed: &[String]) {
        if let Ok(mut users) = self.allowed_users.write() {
            *users = allowed.to_vec();
        }
    }

    async fn health_check(&self) -> bool {
        self.fetch_access_token().await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_name() {
        let ch = QQChannel::new("id".into(), "secret".into(), vec![]);
        assert_eq!(ch.name(), "qq");
    }

    /// A direct message is keyed on `user_openid`, not on `author.id`. Getting
    /// this wrong points the allowlist and the conversation at an identity the
    /// operator never enrolled, so an allowed user is silently refused.
    #[test]
    fn classify_inbound_keys_a_direct_message_on_the_user_openid() {
        let d = serde_json::json!({
            "content": " hello ",
            "author": {"id": "author-id", "user_openid": "openid-1"},
        });
        assert_eq!(
            QQChannel::classify_inbound("C2C_MESSAGE_CREATE", &d),
            QqInbound::Route(QqRouted {
                kind: "C2C",
                sender: "openid-1".into(),
                chat_id: "user:openid-1".into(),
                content: "hello".into(),
            })
        );
    }

    /// `author.id` is the documented fallback when the event omits the openid.
    #[test]
    fn classify_inbound_falls_back_to_the_author_id_without_an_openid() {
        let d = serde_json::json!({"content": "hi", "author": {"id": "author-id"}});
        let QqInbound::Route(routed) = QQChannel::classify_inbound("C2C_MESSAGE_CREATE", &d) else {
            panic!("a direct message with content must route");
        };
        assert_eq!(routed.sender, "author-id");
        assert_eq!(routed.chat_id, "user:author-id");
    }

    /// A group message is keyed on `member_openid`, and its reply anchor is the
    /// group — answering into `user:` would send a group reply to one member.
    #[test]
    fn classify_inbound_anchors_a_group_message_on_the_group() {
        let d = serde_json::json!({
            "content": "hello",
            "author": {"id": "author-id", "member_openid": "member-1"},
            "group_openid": "group-1",
        });
        assert_eq!(
            QQChannel::classify_inbound("GROUP_AT_MESSAGE_CREATE", &d),
            QqInbound::Route(QqRouted {
                kind: "group",
                sender: "member-1".into(),
                chat_id: "group:group-1".into(),
                content: "hello".into(),
            })
        );
    }

    #[test]
    fn classify_inbound_ignores_empty_content_and_unhandled_events() {
        let empty = serde_json::json!({"content": "   ", "author": {"user_openid": "openid-1"}});
        assert_eq!(
            QQChannel::classify_inbound("C2C_MESSAGE_CREATE", &empty),
            QqInbound::Ignore,
            "a whitespace-only message has nothing to answer"
        );

        let full = serde_json::json!({"content": "hi", "author": {"user_openid": "openid-1"}});
        assert_eq!(
            QQChannel::classify_inbound("GUILD_MEMBER_ADD", &full),
            QqInbound::Ignore,
            "only the two message events route"
        );
        // Control: the same payload under a handled event type does route, so
        // the two ignores above are the filter and not an inert classifier.
        assert!(matches!(
            QQChannel::classify_inbound("C2C_MESSAGE_CREATE", &full),
            QqInbound::Route(_)
        ));
    }

    #[test]
    fn test_user_allowed_wildcard() {
        let ch = QQChannel::new("id".into(), "secret".into(), vec!["*".into()]);
        assert!(ch.is_user_allowed("anyone"));
    }

    #[test]
    fn test_user_allowed_specific() {
        let ch = QQChannel::new("id".into(), "secret".into(), vec!["user123".into()]);
        assert!(ch.is_user_allowed("user123"));
        assert!(!ch.is_user_allowed("other"));
    }

    #[test]
    fn test_user_denied_empty() {
        let ch = QQChannel::new("id".into(), "secret".into(), vec![]);
        assert!(!ch.is_user_allowed("anyone"));
    }

    #[tokio::test]
    async fn test_dedup() {
        let ch = QQChannel::new("id".into(), "secret".into(), vec![]);
        assert!(!ch.is_duplicate("msg1").await);
        assert!(ch.is_duplicate("msg1").await);
        assert!(!ch.is_duplicate("msg2").await);
    }

    /// The set used to be evicted with `iter().take(CAPACITY/2)`, whose order
    /// is unspecified — so a just-inserted id could be dropped, and a dedup
    /// miss costs a complete extra LLM turn plus a duplicate reply.
    #[tokio::test]
    async fn qq_dedup_evicts_the_oldest() {
        let ch = QQChannel::new("id".into(), "secret".into(), vec![]);

        // Fill exactly to capacity, then push one more.
        for i in 0..DEDUP_CAPACITY {
            assert!(!ch.is_duplicate(&format!("msg-{i}")).await);
        }
        assert!(!ch.is_duplicate("newest").await);

        {
            let guard = ch.dedup.read().await;
            assert_eq!(guard.0.len(), DEDUP_CAPACITY);
            assert_eq!(guard.1.len(), DEDUP_CAPACITY);
            assert!(
                guard.1.contains("newest"),
                "the most recent id must survive eviction"
            );
            assert!(
                !guard.1.contains("msg-0"),
                "the oldest id is the one that goes"
            );
            assert!(
                guard.1.contains(&format!("msg-{}", DEDUP_CAPACITY - 1)),
                "everything newer than the evicted head must stay"
            );
        }

        // And the practical consequence: the newest id is still deduped.
        assert!(ch.is_duplicate("newest").await);
    }

    /// Pairing used to append to the persisted config only, so a freshly-paired
    /// user stayed locked out until the daemon restarted.
    #[test]
    fn qq_pairing_grants_immediate_access() {
        let ch = QQChannel::new("id".into(), "secret".into(), vec![]);
        assert!(!ch.is_user_allowed("openid-abc"));
        ch.add_allowed_identity_runtime("openid-abc");
        assert!(ch.is_user_allowed("openid-abc"));
    }

    #[test]
    fn qq_allowlist_edit_reaches_the_channel() {
        let ch = QQChannel::new("id".into(), "secret".into(), vec!["*".into()]);
        assert!(ch.is_user_allowed("anyone"));
        ch.apply_allowed_senders(&["openid-abc".to_string()]);
        assert!(ch.is_user_allowed("openid-abc"));
        assert!(!ch.is_user_allowed("anyone"));
    }

    #[tokio::test]
    async fn test_dedup_empty_id() {
        let ch = QQChannel::new("id".into(), "secret".into(), vec![]);
        // Empty IDs should never be considered duplicates
        assert!(!ch.is_duplicate("").await);
        assert!(!ch.is_duplicate("").await);
    }

    #[test]
    fn test_config_serde() {
        let toml_str = r#"
app_id = "12345"
app_secret = "secret_abc"
allowed_users = ["user1"]
"#;
        let config: crate::config::schema::QQConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.app_id, "12345");
        assert_eq!(config.app_secret, "secret_abc");
        assert_eq!(config.allowed_users, vec!["user1"]);
    }

    /// A store-minted owner code consumed for the `qq` surface appends the sender
    /// openid to `allowed_users` and `approval_owners` and persists the config —
    /// the shared-core path `try_handle_store_pairing` invokes before both (C2C +
    /// group) allowlist gates (the API reply send is exercised in integration, so
    /// we assert the store + config mutation here).
    #[tokio::test]
    async fn qq_store_minted_claim_grants_owner() {
        use crate::channels::pairing::{try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        let _guard = crate::test_env::ENV_LOCK.lock().await;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");
        {
            let mut seed = crate::config::Config::load_or_init().await.unwrap();
            seed.channels_config.qq = Some(crate::config::schema::QQConfig {
                app_id: "id".into(),
                app_secret: "secret".into(),
                allowed_users: vec![],
            });
            seed.save().await.unwrap();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let code = pairing_store::mint(root, "qq", 900, None, true, now).unwrap();

        let reply = try_handle_pairing(
            &format!("/claim {code}"),
            "qq",
            AllowlistField::AllowedUsers,
            &["openid_xyz".to_string()],
            root,
        )
        .await
        .expect("pairing command should be handled");
        assert!(reply.contains("owner"), "reply was: {reply}");

        let config = crate::config::Config::load_or_init().await.unwrap();
        let users = &config.channels_config.qq.as_ref().unwrap().allowed_users;
        assert!(users.contains(&"openid_xyz".to_string()));
        assert!(config
            .channels_config
            .approval_owners
            .contains(&"openid_xyz".to_string()));

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }
}
