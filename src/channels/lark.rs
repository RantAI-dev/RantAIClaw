use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use uuid::Uuid;

const FEISHU_BASE_URL: &str = "https://open.feishu.cn/open-apis";
const FEISHU_WS_BASE_URL: &str = "https://open.feishu.cn";
const LARK_BASE_URL: &str = "https://open.larksuite.com/open-apis";
const LARK_WS_BASE_URL: &str = "https://open.larksuite.com";
const ACK_REACTION_EMOJI_TYPE: &str = "SMILE";

// ─────────────────────────────────────────────────────────────────────────────
// Feishu WebSocket long-connection: pbbp2.proto frame codec
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, prost::Message)]
struct PbHeader {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

/// Feishu WS frame (pbbp2.proto).
/// method=0 → CONTROL (ping/pong)  method=1 → DATA (events)
#[derive(Clone, PartialEq, prost::Message)]
struct PbFrame {
    #[prost(uint64, tag = "1")]
    pub seq_id: u64,
    #[prost(uint64, tag = "2")]
    pub log_id: u64,
    #[prost(int32, tag = "3")]
    pub service: i32,
    #[prost(int32, tag = "4")]
    pub method: i32,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<PbHeader>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub payload: Option<Vec<u8>>,
}

impl PbFrame {
    fn header_value<'a>(&'a self, key: &str) -> &'a str {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
            .unwrap_or("")
    }
}

/// Server-sent client config (parsed from pong payload)
#[derive(Debug, serde::Deserialize, Default, Clone)]
struct WsClientConfig {
    #[serde(rename = "PingInterval")]
    ping_interval: Option<u64>,
}

/// POST /callback/ws/endpoint response
#[derive(Debug, serde::Deserialize)]
struct WsEndpointResp {
    code: i32,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    data: Option<WsEndpoint>,
}

#[derive(Debug, serde::Deserialize)]
struct WsEndpoint {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "ClientConfig")]
    client_config: Option<WsClientConfig>,
}

/// LarkEvent envelope (method=1 / type=event payload)
#[derive(Debug, serde::Deserialize)]
struct LarkEvent {
    header: LarkEventHeader,
    event: serde_json::Value,
}

#[derive(Debug, serde::Deserialize)]
struct LarkEventHeader {
    event_type: String,
    #[allow(dead_code)]
    event_id: String,
}

#[derive(Debug, serde::Deserialize)]
struct MsgReceivePayload {
    sender: LarkSender,
    message: LarkMessage,
}

#[derive(Debug, serde::Deserialize)]
struct LarkSender {
    sender_id: LarkSenderId,
    #[serde(default)]
    sender_type: String,
}

#[derive(Debug, serde::Deserialize, Default)]
struct LarkSenderId {
    open_id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct LarkMessage {
    message_id: String,
    chat_id: String,
    chat_type: String,
    message_type: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    mentions: Vec<serde_json::Value>,
}

/// Heartbeat timeout for WS connection — must be larger than ping_interval (default 120 s).
/// If no binary frame (pong or event) is received within this window, reconnect.
const WS_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh tenant token this many seconds before the announced expiry.
const LARK_TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(120);
/// Fallback tenant token TTL when `expire`/`expires_in` is absent.
const LARK_DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(7200);
/// Feishu/Lark API business code for expired/invalid tenant access token.
const LARK_INVALID_ACCESS_TOKEN_CODE: i64 = 99_991_663;

/// Returns true when the WebSocket frame indicates live traffic that should
/// refresh the heartbeat watchdog.
fn should_refresh_last_recv(msg: &WsMsg) -> bool {
    matches!(msg, WsMsg::Binary(_) | WsMsg::Ping(_) | WsMsg::Pong(_))
}

#[derive(Debug, Clone)]
struct CachedTenantToken {
    value: String,
    refresh_after: Instant,
}

fn extract_lark_response_code(body: &serde_json::Value) -> Option<i64> {
    body.get("code").and_then(|c| c.as_i64())
}

fn is_lark_invalid_access_token(body: &serde_json::Value) -> bool {
    extract_lark_response_code(body) == Some(LARK_INVALID_ACCESS_TOKEN_CODE)
}

fn should_refresh_lark_tenant_token(status: reqwest::StatusCode, body: &serde_json::Value) -> bool {
    status == reqwest::StatusCode::UNAUTHORIZED || is_lark_invalid_access_token(body)
}

fn extract_lark_token_ttl_seconds(body: &serde_json::Value) -> u64 {
    let ttl = body
        .get("expire")
        .or_else(|| body.get("expires_in"))
        .and_then(|v| v.as_u64())
        .or_else(|| {
            body.get("expire")
                .or_else(|| body.get("expires_in"))
                .and_then(|v| v.as_i64())
                .and_then(|v| u64::try_from(v).ok())
        })
        .unwrap_or(LARK_DEFAULT_TOKEN_TTL.as_secs());
    ttl.max(1)
}

fn next_token_refresh_deadline(now: Instant, ttl_seconds: u64) -> Instant {
    let ttl = Duration::from_secs(ttl_seconds.max(1));
    let refresh_in = ttl
        .checked_sub(LARK_TOKEN_REFRESH_SKEW)
        .unwrap_or(Duration::from_secs(1));
    now + refresh_in
}

fn ensure_lark_send_success(
    status: reqwest::StatusCode,
    body: &serde_json::Value,
    context: &str,
) -> anyhow::Result<()> {
    if !status.is_success() {
        anyhow::bail!("Lark send failed {context}: status={status}, body={body}");
    }

    let code = extract_lark_response_code(body).unwrap_or(0);
    if code != 0 {
        anyhow::bail!("Lark send failed {context}: code={code}, body={body}");
    }

    Ok(())
}

/// Lark/Feishu channel.
///
/// Supports two receive modes (configured via `receive_mode` in config):
/// - **`websocket`** (default): persistent WSS long-connection; no public URL needed.
/// - **`webhook`**: HTTP callback server; requires a public HTTPS endpoint.
pub struct LarkChannel {
    app_id: String,
    app_secret: String,
    verification_token: String,
    port: Option<u16>,
    /// Runtime-mutable so a successful `/bind`/`/claim` takes effect without a
    /// daemon restart, and so `apply_allowed_senders` can push a config edit
    /// into the live channel. `std::sync::RwLock`, matching Slack and Telegram:
    /// `is_user_allowed` is called from sync context and an async lock would
    /// force it — and every caller — to become async for no gain.
    allowed_users: Arc<std::sync::RwLock<Vec<String>>>,
    /// When true, use Feishu (CN) endpoints; when false, use Lark (international).
    use_feishu: bool,
    /// How to receive events: WebSocket long-connection or HTTP webhook.
    receive_mode: crate::config::schema::LarkReceiveMode,
    /// Event-body encryption key, if the tenant configured one. Carried so
    /// `listen_http` can refuse to start rather than accept a key it does not
    /// use — see the startup check there.
    encrypt_key: Option<String>,
    /// Cached tenant access token
    tenant_token: Arc<RwLock<Option<CachedTenantToken>>>,
    /// Who this bot is, resolved once per listener start. `None` until
    /// resolved, or permanently if the lookup failed.
    bot_identity: Arc<RwLock<Option<BotIdentity>>>,
    /// Dedup set: WS message_ids seen in last ~30 min to prevent double-dispatch
    ws_seen_ids: Arc<RwLock<HashMap<String, Instant>>>,
}

impl LarkChannel {
    pub fn new(
        app_id: String,
        app_secret: String,
        verification_token: String,
        port: Option<u16>,
        allowed_users: Vec<String>,
    ) -> Self {
        Self {
            app_id,
            app_secret,
            verification_token,
            port,
            allowed_users: Arc::new(std::sync::RwLock::new(allowed_users)),
            use_feishu: true,
            receive_mode: crate::config::schema::LarkReceiveMode::default(),
            encrypt_key: None,
            tenant_token: Arc::new(RwLock::new(None)),
            bot_identity: Arc::new(RwLock::new(None)),
            ws_seen_ids: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Build from `LarkConfig` (preserves `use_feishu` and `receive_mode`).
    pub fn from_config(config: &crate::config::schema::LarkConfig) -> Self {
        let mut ch = Self::new(
            config.app_id.clone(),
            config.app_secret.clone(),
            config.verification_token.clone().unwrap_or_default(),
            config.port,
            config.allowed_users.clone(),
        );
        ch.use_feishu = config.use_feishu;
        ch.receive_mode = config.receive_mode.clone();
        ch.encrypt_key = config.encrypt_key.clone();
        ch
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client("channel.lark")
    }

    fn api_base(&self) -> &'static str {
        if self.use_feishu {
            FEISHU_BASE_URL
        } else {
            LARK_BASE_URL
        }
    }

    fn ws_base(&self) -> &'static str {
        if self.use_feishu {
            FEISHU_WS_BASE_URL
        } else {
            LARK_WS_BASE_URL
        }
    }

    fn tenant_access_token_url(&self) -> String {
        format!("{}/auth/v3/tenant_access_token/internal", self.api_base())
    }

    fn send_message_url(&self) -> String {
        format!("{}/im/v1/messages?receive_id_type=chat_id", self.api_base())
    }

    fn message_reaction_url(&self, message_id: &str) -> String {
        format!("{}/im/v1/messages/{message_id}/reactions", self.api_base())
    }

    async fn post_message_reaction_with_token(
        &self,
        message_id: &str,
        token: &str,
    ) -> anyhow::Result<reqwest::Response> {
        let url = self.message_reaction_url(message_id);
        let body = serde_json::json!({
            "reaction_type": {
                "emoji_type": ACK_REACTION_EMOJI_TYPE
            }
        });

        let response = self
            .http_client()
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json; charset=utf-8")
            .json(&body)
            .send()
            .await?;

        Ok(response)
    }

    /// Best-effort "received" signal for incoming messages.
    /// Failures are logged and never block normal message handling.
    async fn try_add_ack_reaction(&self, message_id: &str) {
        if message_id.is_empty() {
            return;
        }

        let mut token = match self.get_tenant_access_token().await {
            Ok(token) => token,
            Err(err) => {
                tracing::warn!("Lark: failed to fetch token for reaction: {err}");
                return;
            }
        };

        let mut retried = false;
        loop {
            let response = match self
                .post_message_reaction_with_token(message_id, &token)
                .await
            {
                Ok(resp) => resp,
                Err(err) => {
                    tracing::warn!("Lark: failed to add reaction for {message_id}: {err}");
                    return;
                }
            };

            if response.status().as_u16() == 401 && !retried {
                self.invalidate_token().await;
                token = match self.get_tenant_access_token().await {
                    Ok(new_token) => new_token,
                    Err(err) => {
                        tracing::warn!(
                            "Lark: failed to refresh token for reaction on {message_id}: {err}"
                        );
                        return;
                    }
                };
                retried = true;
                continue;
            }

            if !response.status().is_success() {
                let status = response.status();
                let err_body = response.text().await.unwrap_or_default();
                tracing::warn!(
                    "Lark: add reaction failed for {message_id}: status={status}, body={err_body}"
                );
                return;
            }

            let payload: serde_json::Value = match response.json().await {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!("Lark: add reaction decode failed for {message_id}: {err}");
                    return;
                }
            };

            let code = payload.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
            if code != 0 {
                let msg = payload
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                tracing::warn!("Lark: add reaction returned code={code} for {message_id}: {msg}");
            }
            return;
        }
    }

    /// POST /callback/ws/endpoint → (wss_url, client_config)
    async fn get_ws_endpoint(&self) -> anyhow::Result<(String, WsClientConfig)> {
        let resp = self
            .http_client()
            .post(format!("{}/callback/ws/endpoint", self.ws_base()))
            .header("locale", if self.use_feishu { "zh" } else { "en" })
            .json(&serde_json::json!({
                "AppID": self.app_id,
                "AppSecret": self.app_secret,
            }))
            .send()
            .await?
            .json::<WsEndpointResp>()
            .await?;
        if resp.code != 0 {
            anyhow::bail!(
                "Lark WS endpoint failed: code={} msg={}",
                resp.code,
                resp.msg.as_deref().unwrap_or("(none)")
            );
        }
        let ep = resp
            .data
            .ok_or_else(|| anyhow::anyhow!("Lark WS endpoint: empty data"))?;
        Ok((ep.url, ep.client_config.unwrap_or_default()))
    }

    /// WS long-connection event loop.  Returns Ok(()) when the connection closes
    /// (the caller reconnects).
    #[allow(clippy::too_many_lines)]
    async fn listen_ws(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        // Resolve who we are before the first group message arrives, so the
        // mention gate has something to compare against.
        self.ensure_bot_identity().await;

        let (wss_url, client_config) = self.get_ws_endpoint().await?;
        let service_id = wss_url
            .split('?')
            .nth(1)
            .and_then(|qs| {
                qs.split('&')
                    .find(|kv| kv.starts_with("service_id="))
                    .and_then(|kv| kv.split('=').nth(1))
                    .and_then(|v| v.parse::<i32>().ok())
            })
            .unwrap_or(0);
        tracing::info!("Lark: connecting to {wss_url}");

        let (ws_stream, _) = tokio_tungstenite::connect_async(&wss_url).await?;
        let (mut write, mut read) = ws_stream.split();
        tracing::info!("Lark: WS connected (service_id={service_id})");

        let mut ping_secs = client_config.ping_interval.unwrap_or(120).max(10);
        let mut hb_interval = tokio::time::interval(Duration::from_secs(ping_secs));
        let mut timeout_check = tokio::time::interval(Duration::from_secs(10));
        hb_interval.tick().await; // consume immediate tick

        let mut seq: u64 = 0;
        let mut last_recv = Instant::now();

        // Send initial ping immediately (like the official SDK) so the server
        // starts responding with pongs and we can calibrate the ping_interval.
        seq = seq.wrapping_add(1);
        let initial_ping = PbFrame {
            seq_id: seq,
            log_id: 0,
            service: service_id,
            method: 0,
            headers: vec![PbHeader {
                key: "type".into(),
                value: "ping".into(),
            }],
            payload: None,
        };
        if write
            .send(WsMsg::Binary(initial_ping.encode_to_vec().into()))
            .await
            .is_err()
        {
            anyhow::bail!("Lark: initial ping failed");
        }
        // message_id → (fragment_slots, created_at) for multi-part reassembly
        type FragEntry = (Vec<Option<Vec<u8>>>, Instant);
        let mut frag_cache: HashMap<String, FragEntry> = HashMap::new();

        loop {
            tokio::select! {
                biased;

                _ = hb_interval.tick() => {
                    seq = seq.wrapping_add(1);
                    let ping = PbFrame {
                        seq_id: seq, log_id: 0, service: service_id, method: 0,
                        headers: vec![PbHeader { key: "type".into(), value: "ping".into() }],
                        payload: None,
                    };
                    if write.send(WsMsg::Binary(ping.encode_to_vec().into())).await.is_err() {
                        tracing::warn!("Lark: ping failed, reconnecting");
                        break;
                    }
                    // GC stale fragments > 5 min
                    let cutoff = Instant::now().checked_sub(Duration::from_secs(300)).unwrap_or(Instant::now());
                    frag_cache.retain(|_, (_, ts)| *ts > cutoff);
                }

                _ = timeout_check.tick() => {
                    if last_recv.elapsed() > WS_HEARTBEAT_TIMEOUT {
                        tracing::warn!("Lark: heartbeat timeout, reconnecting");
                        break;
                    }
                }

                msg = read.next() => {
                    let raw = match msg {
                        Some(Ok(ws_msg)) => {
                            if should_refresh_last_recv(&ws_msg) {
                                last_recv = Instant::now();
                            }
                            match ws_msg {
                                WsMsg::Binary(b) => b,
                                WsMsg::Ping(d) => { let _ = write.send(WsMsg::Pong(d)).await; continue; }
                                WsMsg::Pong(_) => continue,
                                WsMsg::Close(_) => { tracing::info!("Lark: WS closed — reconnecting"); break; }
                                _ => continue,
                            }
                        }
                        None => { tracing::info!("Lark: WS closed — reconnecting"); break; }
                        Some(Err(e)) => { tracing::error!("Lark: WS read error: {e}"); break; }
                    };

                    let frame = match PbFrame::decode(&raw[..]) {
                        Ok(f) => f,
                        Err(e) => { tracing::error!("Lark: proto decode: {e}"); continue; }
                    };

                    // CONTROL frame
                    if frame.method == 0 {
                        if frame.header_value("type") == "pong" {
                            if let Some(p) = &frame.payload {
                                if let Ok(cfg) = serde_json::from_slice::<WsClientConfig>(p) {
                                    if let Some(secs) = cfg.ping_interval {
                                        let secs = secs.max(10);
                                        if secs != ping_secs {
                                            ping_secs = secs;
                                            hb_interval = tokio::time::interval(Duration::from_secs(ping_secs));
                                            tracing::info!("Lark: ping_interval → {ping_secs}s");
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }

                    // DATA frame
                    let msg_type = frame.header_value("type").to_string();
                    let msg_id   = frame.header_value("message_id").to_string();
                    let sum      = frame.header_value("sum").parse::<usize>().unwrap_or(1);
                    let seq_num  = frame.header_value("seq").parse::<usize>().unwrap_or(0);

                    // ACK immediately (Feishu requires within 3 s)
                    {
                        let mut ack = frame.clone();
                        ack.payload = Some(br#"{"code":200,"headers":{},"data":[]}"#.to_vec());
                        ack.headers.push(PbHeader { key: "biz_rt".into(), value: "0".into() });
                        let _ = write.send(WsMsg::Binary(ack.encode_to_vec().into())).await;
                    }

                    // Fragment reassembly
                    let sum = if sum == 0 { 1 } else { sum };
                    let payload: Vec<u8> = if sum == 1 || msg_id.is_empty() || seq_num >= sum {
                        frame.payload.clone().unwrap_or_default()
                    } else {
                        let entry = frag_cache.entry(msg_id.clone())
                            .or_insert_with(|| (vec![None; sum], Instant::now()));
                        if entry.0.len() != sum { *entry = (vec![None; sum], Instant::now()); }
                        entry.0[seq_num] = frame.payload.clone();
                        if entry.0.iter().all(|s| s.is_some()) {
                            let full: Vec<u8> = entry.0.iter()
                                .flat_map(|s| s.as_deref().unwrap_or(&[]))
                                .copied().collect();
                            frag_cache.remove(&msg_id);
                            full
                        } else { continue; }
                    };

                    if msg_type != "event" { continue; }

                    let event: LarkEvent = match serde_json::from_slice(&payload) {
                        Ok(e) => e,
                        Err(e) => { tracing::error!("Lark: event JSON: {e}"); continue; }
                    };
                    if event.header.event_type != "im.message.receive_v1" { continue; }

                    let recv: MsgReceivePayload = match serde_json::from_value(event.event) {
                        Ok(r) => r,
                        Err(e) => { tracing::error!("Lark: payload parse: {e}"); continue; }
                    };

                    if recv.sender.sender_type == "app" || recv.sender.sender_type == "bot" { continue; }

                    let sender_open_id = recv.sender.sender_id.open_id.as_deref().unwrap_or("");

                    let lark_msg = &recv.message;

                    // Dedup
                    {
                        let now = Instant::now();
                        let mut seen = self.ws_seen_ids.write().await;
                        // GC
                        seen.retain(|_, t| now.duration_since(*t) < Duration::from_secs(30 * 60));
                        if seen.contains_key(&lark_msg.message_id) {
                            tracing::debug!("Lark WS: dup {}", lark_msg.message_id);
                            continue;
                        }
                        seen.insert(lark_msg.message_id.clone(), now);
                    }

                    // Decode content by type (mirrors clawdbot-feishu parsing)
                    let text = match lark_msg.message_type.as_str() {
                        "text" => {
                            let v: serde_json::Value = match serde_json::from_str(&lark_msg.content) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            match v.get("text").and_then(|t| t.as_str()).filter(|s| !s.is_empty()) {
                                Some(t) => t.to_string(),
                                None => continue,
                            }
                        }
                        "post" => match parse_post_content(&lark_msg.content) {
                            Some(t) => t,
                            None => continue,
                        },
                        _ => { tracing::debug!("Lark WS: skipping unsupported type '{}'", lark_msg.message_type); continue; }
                    };

                    // Strip @_user_N placeholders
                    let text = strip_at_placeholders(&text);
                    let text = text.trim().to_string();
                    if text.is_empty() { continue; }

                    // Group-chat: only respond when explicitly @-mentioned
                    let me = self.bot_identity.read().await.clone();
                    if lark_msg.chat_type == "group"
                        && !should_respond_in_group(&lark_msg.mentions, me.as_ref())
                    {
                        continue;
                    }

                    // Intercept on-demand store-minted `/bind`/`/claim` pairing
                    // codes before the allowlist gate so unenrolled users can
                    // self-onboard without a daemon restart. Identity is the
                    // sender open_id; the reply routes to the chat_id.
                    if self
                        .try_handle_store_pairing(&text, sender_open_id, &lark_msg.chat_id)
                        .await
                    {
                        continue;
                    }

                    if !self.is_user_allowed(sender_open_id) {
                        tracing::warn!("Lark WS: ignoring {sender_open_id} (not in allowed_users)");
                        continue;
                    }

                    self.try_add_ack_reaction(&lark_msg.message_id).await;

                    let channel_msg = ChannelMessage {
                        // The person, not the room. Every other channel reports
                        // the individual here, and the owner gate compares
                        // against it — reporting the chat id broke owner
                        // authority outright, and the obvious workaround
                        // (pasting the chat id into `approval_owners`) silently
                        // promoted every member of that group to owner.
                        // `reply_target` stays the chat: that part was right.
                        sender_aliases: vec![lark_msg.chat_id.clone()],
                        // Carry the platform id: a UUID minted here makes a
                        // redelivery undetectable. Lark's own `ws_seen_ids`
                        // dedup exists because of exactly that.
                        id: format!("lark_{}", lark_msg.message_id),
                        sender: sender_open_id.to_string(),
                        reply_target: lark_msg.chat_id.clone(),
                        content: text,
                        channel: "lark".to_string(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        thread_ts: None,
                    };

                    tracing::debug!("Lark WS: message in {}", lark_msg.chat_id);
                    if tx.send(channel_msg).await.is_err() { break; }
                }
            }
        }
        Ok(())
    }

    /// Warn about `approval_owners` entries that name a conversation rather
    /// than a person.
    ///
    /// While `sender` reported the chat id, the obvious way to make owner
    /// approval work on Lark was to paste that chat id into `approval_owners`.
    /// That silently granted owner authority to **every member** of the
    /// conversation. Now that `sender` is the person, such an entry matches
    /// nobody — so the operator has a broken gate either way and needs telling.
    pub(crate) fn warn_on_chat_shaped_owners(owners: &[String]) -> Vec<String> {
        owners
            .iter()
            .filter(|o| o.starts_with("oc_"))
            .cloned()
            .collect()
    }

    /// Ask the platform who this bot is, so a group mention can be matched
    /// against it.
    ///
    /// Called once when a listener starts, not per message: the answer does not
    /// change while the process runs, and a per-message lookup would put a
    /// network round trip in front of every group message.
    async fn fetch_bot_identity(&self) -> anyhow::Result<BotIdentity> {
        let token = self.get_tenant_access_token().await?;
        let url = format!("{}/bot/v3/info", self.api_base());
        let resp: serde_json::Value = self
            .http_client()
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await?
            .json()
            .await?;

        let bot = resp.get("bot").unwrap_or(&serde_json::Value::Null);
        let identity = BotIdentity {
            open_id: bot
                .get("open_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            name: bot
                .get("app_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        };

        if identity.open_id.is_empty() && identity.name.is_empty() {
            anyhow::bail!("bot info carried neither open_id nor app_name");
        }
        Ok(identity)
    }

    /// Resolve and cache the bot identity, warning — but not failing — when the
    /// lookup does not work. See [`should_respond_in_group`] for why this
    /// degrades permissively rather than going silent.
    async fn ensure_bot_identity(&self) {
        if self.bot_identity.read().await.is_some() {
            return;
        }
        match self.fetch_bot_identity().await {
            Ok(id) => {
                tracing::info!(
                    open_id = %id.open_id,
                    name = %id.name,
                    "Lark: resolved bot identity for group mention matching"
                );
                *self.bot_identity.write().await = Some(id);
            }
            Err(e) => {
                tracing::warn!(
                    "Lark: could not resolve the bot's own identity ({e}). Group mention \
                     matching falls back to responding whenever anyone is mentioned — \
                     noisier than intended, but the channel keeps working."
                );
            }
        }
    }

    /// Append a freshly-paired identity to the runtime allowlist (deduped) so
    /// access is effective immediately. The persisted config, written by the
    /// pairing core, remains the source of truth across restarts.
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

    /// Check if a user open_id is allowed
    fn is_user_allowed(&self, open_id: &str) -> bool {
        self.allowed_users
            .read()
            .map(|users| users.iter().any(|u| u == "*" || u == open_id))
            .unwrap_or(false)
    }

    /// Self-onboarding hook: if `text` is a `/bind`/`/claim` command, validate it
    /// against the shared [`crate::security::pairing_store`] (appending the sender
    /// open_id to `allowed_users` and, for an owner-capable `/claim`, to
    /// `approval_owners`, then persisting `config.toml`) and reply to the chat.
    /// Shared by the websocket and webhook inbound paths; `chat_id` is the reply
    /// target (Lark sends with `receive_id_type=chat_id`).
    ///
    /// Returns `true` when the message WAS a pairing command (handled here — must
    /// NOT be forwarded to the agent), `false` otherwise (normal message → gate).
    async fn try_handle_store_pairing(&self, text: &str, open_id: &str, chat_id: &str) -> bool {
        use crate::channels::pairing::{parse_pairing_command, try_handle_pairing, AllowlistField};

        if parse_pairing_command(text).is_none() {
            return false;
        }
        let Some(root) = crate::channels::pairing::profile_root("lark") else {
            return false;
        };

        let Some(reply) = try_handle_pairing(
            text,
            "lark",
            AllowlistField::AllowedUsers,
            &[open_id.to_string()],
            &root,
        )
        .await
        else {
            return false;
        };

        // The pairing core persisted the identity; mirror it into the live
        // allowlist so the very next message from this person is accepted
        // instead of waiting for a daemon restart.
        self.add_allowed_identity_runtime(open_id);

        if let Err(e) = self.send(&SendMessage::new(reply, chat_id)).await {
            tracing::warn!("Lark pairing: failed to send reply: {e:#}");
        }
        true
    }

    /// Extract `(text, open_id, chat_id)` from a webhook event payload for the
    /// shared pairing path, *without* the allowlist gate (so an unenrolled user's
    /// `/bind`/`/claim` is still seen). Returns `None` when the payload carries no
    /// actionable text message.
    fn extract_pairing_context(payload: &serde_json::Value) -> Option<(String, String, String)> {
        let event_type = payload
            .pointer("/header/event_type")
            .and_then(|e| e.as_str())
            .unwrap_or("");
        if event_type != "im.message.receive_v1" {
            return None;
        }
        let event = payload.get("event")?;

        let open_id = event
            .pointer("/sender/sender_id/open_id")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())?;

        let msg_type = event
            .pointer("/message/message_type")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let content_str = event
            .pointer("/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or("");

        let text: String = match msg_type {
            "text" => serde_json::from_str::<serde_json::Value>(content_str)
                .ok()
                .and_then(|v| {
                    v.get("text")
                        .and_then(|t| t.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                })?,
            "post" => parse_post_content(content_str)?,
            _ => return None,
        };
        let text = strip_at_placeholders(&text).trim().to_string();
        if text.is_empty() {
            return None;
        }

        let chat_id = event
            .pointer("/message/chat_id")
            .and_then(|c| c.as_str())
            .unwrap_or(open_id);

        Some((text, open_id.to_string(), chat_id.to_string()))
    }

    /// Webhook-payload wrapper around [`Self::try_handle_store_pairing`]. Returns
    /// `true` when the payload WAS a pairing command (handled here — must NOT be
    /// parsed/dispatched), `false` otherwise.
    pub async fn try_handle_store_pairing_payload(&self, payload: &serde_json::Value) -> bool {
        let Some((text, open_id, chat_id)) = Self::extract_pairing_context(payload) else {
            return false;
        };
        self.try_handle_store_pairing(&text, &open_id, &chat_id)
            .await
    }

    /// Get or refresh tenant access token
    async fn get_tenant_access_token(&self) -> anyhow::Result<String> {
        // Check cache first
        {
            let cached = self.tenant_token.read().await;
            if let Some(ref token) = *cached {
                if Instant::now() < token.refresh_after {
                    return Ok(token.value.clone());
                }
            }
        }

        let url = self.tenant_access_token_url();
        let body = serde_json::json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret,
        });

        let resp = self.http_client().post(&url).json(&body).send().await?;
        let status = resp.status();
        let data: serde_json::Value = resp.json().await?;

        if !status.is_success() {
            anyhow::bail!("Lark tenant_access_token request failed: status={status}, body={data}");
        }

        let code = data.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = data
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("Lark tenant_access_token failed: {msg}");
        }

        let token = data
            .get("tenant_access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing tenant_access_token in response"))?
            .to_string();

        let ttl_seconds = extract_lark_token_ttl_seconds(&data);
        let refresh_after = next_token_refresh_deadline(Instant::now(), ttl_seconds);

        // Cache it with proactive refresh metadata.
        {
            let mut cached = self.tenant_token.write().await;
            *cached = Some(CachedTenantToken {
                value: token.clone(),
                refresh_after,
            });
        }

        Ok(token)
    }

    /// Invalidate cached token (called when API reports an expired tenant token).
    async fn invalidate_token(&self) {
        let mut cached = self.tenant_token.write().await;
        *cached = None;
    }

    async fn send_text_once(
        &self,
        url: &str,
        token: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<(reqwest::StatusCode, serde_json::Value)> {
        let resp = self
            .http_client()
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json; charset=utf-8")
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        let raw = resp.text().await.unwrap_or_default();
        let parsed = serde_json::from_str::<serde_json::Value>(&raw)
            .unwrap_or_else(|_| serde_json::json!({ "raw": raw }));
        Ok((status, parsed))
    }

    /// Parse an event callback payload and extract text messages
    pub fn parse_event_payload(&self, payload: &serde_json::Value) -> Vec<ChannelMessage> {
        let mut messages = Vec::new();

        // Lark event v2 structure:
        // { "header": { "event_type": "im.message.receive_v1" }, "event": { "message": { ... }, "sender": { ... } } }
        let event_type = payload
            .pointer("/header/event_type")
            .and_then(|e| e.as_str())
            .unwrap_or("");

        if event_type != "im.message.receive_v1" {
            return messages;
        }

        let event = match payload.get("event") {
            Some(e) => e,
            None => return messages,
        };

        // Extract sender open_id
        let open_id = event
            .pointer("/sender/sender_id/open_id")
            .and_then(|s| s.as_str())
            .unwrap_or("");

        if open_id.is_empty() {
            return messages;
        }

        // Check allowlist
        if !self.is_user_allowed(open_id) {
            tracing::warn!("Lark: ignoring message from unauthorized user: {open_id}");
            return messages;
        }

        // Extract message content (text and post supported)
        let msg_type = event
            .pointer("/message/message_type")
            .and_then(|t| t.as_str())
            .unwrap_or("");

        let content_str = event
            .pointer("/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or("");

        let text: String = match msg_type {
            "text" => {
                let extracted = serde_json::from_str::<serde_json::Value>(content_str)
                    .ok()
                    .and_then(|v| {
                        v.get("text")
                            .and_then(|t| t.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                    });
                match extracted {
                    Some(t) => t,
                    None => return messages,
                }
            }
            "post" => match parse_post_content(content_str) {
                Some(t) => t,
                None => return messages,
            },
            _ => {
                tracing::debug!("Lark: skipping unsupported message type: {msg_type}");
                return messages;
            }
        };

        let timestamp = event
            .pointer("/message/create_time")
            .and_then(|t| t.as_str())
            .and_then(|t| t.parse::<u64>().ok())
            // Lark timestamps are in milliseconds
            .map(|ms| ms / 1000)
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            });

        let chat_id = event
            .pointer("/message/chat_id")
            .and_then(|c| c.as_str())
            .unwrap_or(open_id);

        messages.push(ChannelMessage {
            // The person, not the room — see the websocket path for why. The
            // chat id stays reachable as an alias for anything that matched on
            // the old value.
            sender_aliases: vec![chat_id.to_string()],
            // Carry the platform id: a UUID minted here makes a redelivery
            // undetectable, and Lark retries a callback it considers unacked.
            id: event
                .pointer("/message/message_id")
                .and_then(|v| v.as_str())
                .map_or_else(|| Uuid::new_v4().to_string(), |id| format!("lark_{id}")),
            sender: open_id.to_string(),
            reply_target: chat_id.to_string(),
            content: text,
            channel: "lark".to_string(),
            timestamp,
            thread_ts: None,
        });

        messages
    }
}

#[async_trait]
impl Channel for LarkChannel {
    fn name(&self) -> &str {
        "lark"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let token = self.get_tenant_access_token().await?;
        let url = self.send_message_url();

        // This sends Lark's `text` msg_type, which renders plain — no markup — so
        // strip the agent's markdown. (A future `post`/`interactive` rich type
        // would need a different target; the Plain choice rests on `text` here.)
        let rendered = crate::channels::format::render_to_string(
            &message.content,
            &crate::channels::format::RenderTarget::Plain,
        );
        let content = serde_json::json!({ "text": rendered }).to_string();
        let body = serde_json::json!({
            "receive_id": message.recipient,
            "msg_type": "text",
            "content": content,
        });

        let (status, response) = self.send_text_once(&url, &token, &body).await?;

        if should_refresh_lark_tenant_token(status, &response) {
            // Token expired/invalid, invalidate and retry once.
            self.invalidate_token().await;
            let new_token = self.get_tenant_access_token().await?;
            let (retry_status, retry_response) =
                self.send_text_once(&url, &new_token, &body).await?;

            if should_refresh_lark_tenant_token(retry_status, &retry_response) {
                anyhow::bail!(
                    "Lark send failed after token refresh: status={retry_status}, body={retry_response}"
                );
            }

            ensure_lark_send_success(retry_status, &retry_response, "after token refresh")?;
            return Ok(());
        }

        ensure_lark_send_success(status, &response, "without token refresh")?;
        Ok(())
    }

    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        use crate::config::schema::LarkReceiveMode;
        match self.receive_mode {
            LarkReceiveMode::Websocket => self.listen_ws(tx).await,
            LarkReceiveMode::Webhook => self.listen_http(tx).await,
        }
    }

    /// Push an allowlist edit from `config.toml` into the running channel, so
    /// an operator adding a user does not have to bounce the daemon. The trait
    /// method plan 115 added; Lark was the last channel not implementing it.
    fn apply_allowed_senders(&self, allowed: &[String]) {
        if let Ok(mut users) = self.allowed_users.write() {
            if users.as_slice() != allowed {
                tracing::info!(
                    target: "channels",
                    channel = "lark",
                    count = allowed.len(),
                    "applied updated allowlist from config"
                );
                *users = allowed.to_vec();
            }
        }
    }

    async fn health_check(&self) -> bool {
        self.get_tenant_access_token().await.is_ok()
    }
}

impl LarkChannel {
    /// HTTP callback server (legacy — requires a public endpoint).
    /// Use `listen()` (WS long-connection) for new deployments.
    pub async fn listen_http(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
    ) -> anyhow::Result<()> {
        use axum::{extract::State, routing::post, Json, Router};

        #[derive(Clone)]
        struct AppState {
            verification_token: String,
            channel: Arc<LarkChannel>,
            tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        }

        async fn handle_event(
            State(state): State<AppState>,
            body: axum::body::Bytes,
        ) -> axum::response::Response {
            use axum::http::StatusCode;
            use axum::response::IntoResponse;

            // Parse the bytes we were handed, so what gets authenticated below
            // is what gets acted on. The previous `Json(payload)` extractor
            // discarded the raw body, which is the same verify-what-you-parse
            // mistake plan 130 fixes in the gateway.
            let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
                return (StatusCode::BAD_REQUEST, "malformed body").into_response();
            };

            // Authenticity first — above the challenge reply, above the pairing
            // interception, above `parse_event_payload`. The pairing path used
            // to sit in front of the allowlist gate with nothing in front of
            // *it*, so unlimited code guesses were free to anyone who could
            // reach the port.
            if let Err(rejection) = verify_callback(
                payload.get("token").and_then(|t| t.as_str()),
                &state.verification_token,
                payload.get("encrypt").is_some(),
            ) {
                tracing::warn!(
                    reason = rejection.as_str(),
                    "Lark: refused an event callback"
                );
                return match rejection {
                    CallbackRejection::EncryptedPayloadUnsupported => {
                        (StatusCode::NOT_IMPLEMENTED, rejection.as_str()).into_response()
                    }
                    _ => (StatusCode::FORBIDDEN, "invalid token").into_response(),
                };
            }

            // URL verification challenge — now behind the gate above.
            if let Some(challenge) = payload.get("challenge").and_then(|c| c.as_str()) {
                let resp = serde_json::json!({ "challenge": challenge });
                return (StatusCode::OK, Json(resp)).into_response();
            }

            // Intercept on-demand store-minted `/bind`/`/claim` pairing codes
            // before the allowlist gate (in `parse_event_payload`) so unenrolled
            // users can self-onboard without a daemon restart.
            if state
                .channel
                .try_handle_store_pairing_payload(&payload)
                .await
            {
                return (StatusCode::OK, "ok").into_response();
            }

            // Parse event messages
            let messages = state.channel.parse_event_payload(&payload);
            if !messages.is_empty() {
                if let Some(message_id) = payload
                    .pointer("/event/message/message_id")
                    .and_then(|m| m.as_str())
                {
                    state.channel.try_add_ack_reaction(message_id).await;
                }
            }

            for msg in messages {
                if state.tx.send(msg).await.is_err() {
                    tracing::warn!("Lark: message channel closed");
                    break;
                }
            }

            (StatusCode::OK, "ok").into_response()
        }

        let port = self.port.ok_or_else(|| {
            anyhow::anyhow!("Lark webhook mode requires `port` to be set in [channels_config.lark]")
        })?;

        if let Ok(cfg) = crate::config::Config::load_or_init().await {
            let chat_shaped =
                Self::warn_on_chat_shaped_owners(&cfg.channels_config.approval_owners);
            if !chat_shaped.is_empty() {
                tracing::warn!(
                    entries = chat_shaped.join(", "),
                    "Lark: [channels_config] approval_owners contains conversation ids \
                     (oc_…) rather than user ids (ou_…). While this channel reported the \
                     chat as the sender, that was the only way to make owner approval \
                     work — and it granted owner authority to every member of those \
                     conversations. It now matches nobody. Replace them with the ou_… of \
                     each person who should approve."
                );
            }
        }

        // Fail closed at startup rather than serving an endpoint that trusts
        // anyone. Mirrors the gateway's refusal to run without pairing material.
        if self.verification_token.trim().is_empty() {
            anyhow::bail!(
                "Lark webhook mode refuses to start without `verification_token` in \
                 [channels_config.lark] — the event endpoint would authenticate nothing, \
                 so anyone who can reach the port could drive the agent. Copy the token \
                 from the Lark developer console's Event Subscription page, or switch \
                 `receive_mode` to \"websocket\", which needs no inbound endpoint."
            );
        }

        // `encrypt_key` governs event-body encryption, which this build does not
        // implement. Accepting the key while ignoring it is what let a tenant
        // enable encryption and see silence: every event arrives as an `encrypt`
        // envelope this code cannot read. Say so at startup instead.
        if self
            .encrypt_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty())
        {
            anyhow::bail!(
                "Lark `encrypt_key` is set but this build does not decrypt event bodies. \
                 With encryption enabled in the developer console every callback arrives \
                 encrypted and none can be read. Clear `encrypt_key` and turn encryption \
                 off in the console, or use `receive_mode = \"websocket\"`."
            );
        }

        let state = AppState {
            verification_token: self.verification_token.clone(),
            channel: Arc::new(LarkChannel::new(
                self.app_id.clone(),
                self.app_secret.clone(),
                self.verification_token.clone(),
                None,
                self.allowed_users
                    .read()
                    .map(|u| u.clone())
                    .unwrap_or_default(),
            )),
            tx,
        };

        // Body cap and request timeout, matching the gateway's router. An
        // endpoint that fronts a pairing-code path needs a ceiling on both:
        // without them a single large or slow request ties up the handler.
        let app = Router::new()
            .route("/lark", post(handle_event))
            .layer(tower_http::limit::RequestBodyLimitLayer::new(
                crate::gateway::MAX_BODY_SIZE,
            ))
            .layer(tower_http::timeout::TimeoutLayer::with_status_code(
                axum::http::StatusCode::REQUEST_TIMEOUT,
                std::time::Duration::from_secs(crate::gateway::REQUEST_TIMEOUT_SECS),
            ))
            .with_state(state);

        // Loopback unless the operator has explicitly opted into public bind —
        // the same gate `run_gateway` applies. This endpoint used to bind
        // 0.0.0.0 unconditionally, so every interface served an endpoint that
        // authenticated nothing.
        let allow_public = crate::config::Config::load_or_init()
            .await
            .map(|c| c.gateway.allow_public_bind)
            .unwrap_or(false);

        let ip = if allow_public {
            std::net::Ipv4Addr::UNSPECIFIED
        } else {
            std::net::Ipv4Addr::LOCALHOST
        };
        let addr = std::net::SocketAddr::from((ip, port));

        if allow_public {
            tracing::warn!(
                "Lark callback server binding {addr} — [gateway] allow_public_bind is on, \
                 so this endpoint is reachable from every interface."
            );
        }
        tracing::info!("Lark event callback server listening on {addr}");

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Callback authenticity
// ─────────────────────────────────────────────────────────────────────────────

/// Why a callback was refused. Distinct variants because they need different
/// operator responses: a missing token points at a misconfigured subscription,
/// a mismatched one at traffic that is not from your tenant, and an encrypted
/// body at a tenant setting this build cannot serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallbackRejection {
    /// No `token` field in the body. Previously accepted — `map_or(true, …)`
    /// treated an absent token as valid, so an unsigned, tokenless POST from
    /// anyone who could reach the port drove the agent.
    TokenMissing,
    TokenMismatch,
    /// The tenant sent an encrypted envelope. This build does not decrypt, so
    /// it fails loudly instead of being dropped as unparseable — an operator
    /// who set `encrypt_key` needs to know their events are not getting through.
    EncryptedPayloadUnsupported,
}

impl CallbackRejection {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TokenMissing => "no verification token in the callback body",
            Self::TokenMismatch => "verification token does not match",
            Self::EncryptedPayloadUnsupported => {
                "callback body is encrypted; this build verifies signatures but \
                 does not decrypt event bodies"
            }
        }
    }
}

/// Decide whether a callback is authentic.
///
/// The verification token, compared in constant time over the body we actually
/// received. An absent token is a rejection — it used to be treated as valid,
/// so an unsigned, tokenless POST from anyone who could reach the port drove
/// the agent. An encrypted envelope is rejected rather than silently dropped.
///
/// **Signature verification (`X-Lark-Signature`) is deliberately not here.**
/// Its digest construction cannot be derived from this repository — no
/// implementation, no fixture, nothing to check an implementation against — and
/// a construction that is subtly wrong rejects every legitimate callback while
/// presenting as a working gate. That failure is worse than the gate's absence,
/// because it looks like success. It needs the scheme confirmed against a live
/// tenant, which is a follow-up plan, not a guess made here.
pub(crate) fn verify_callback(
    body_token: Option<&str>,
    expected_token: &str,
    body_has_encrypt_field: bool,
) -> Result<(), CallbackRejection> {
    if body_has_encrypt_field {
        return Err(CallbackRejection::EncryptedPayloadUnsupported);
    }

    match body_token {
        None => Err(CallbackRejection::TokenMissing),
        Some(t) if crate::security::pairing::constant_time_eq(t, expected_token) => Ok(()),
        Some(_) => Err(CallbackRejection::TokenMismatch),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// WS helper functions
// ─────────────────────────────────────────────────────────────────────────────

/// Flatten a Feishu `post` rich-text message to plain text.
///
/// Returns `None` when the content cannot be parsed or yields no usable text,
/// so callers can simply `continue` rather than forwarding a meaningless
/// placeholder string to the agent.
fn parse_post_content(content: &str) -> Option<String> {
    let parsed = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let locale = parsed
        .get("zh_cn")
        .or_else(|| parsed.get("en_us"))
        .or_else(|| {
            parsed
                .as_object()
                .and_then(|m| m.values().find(|v| v.is_object()))
        })?;

    let mut text = String::new();

    if let Some(title) = locale
        .get("title")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
    {
        text.push_str(title);
        text.push_str("\n\n");
    }

    if let Some(paragraphs) = locale.get("content").and_then(|c| c.as_array()) {
        for para in paragraphs {
            if let Some(elements) = para.as_array() {
                for el in elements {
                    match el.get("tag").and_then(|t| t.as_str()).unwrap_or("") {
                        "text" => {
                            if let Some(t) = el.get("text").and_then(|t| t.as_str()) {
                                text.push_str(t);
                            }
                        }
                        "a" => {
                            text.push_str(
                                el.get("text")
                                    .and_then(|t| t.as_str())
                                    .filter(|s| !s.is_empty())
                                    .or_else(|| el.get("href").and_then(|h| h.as_str()))
                                    .unwrap_or(""),
                            );
                        }
                        "at" => {
                            let n = el
                                .get("user_name")
                                .and_then(|n| n.as_str())
                                .or_else(|| el.get("user_id").and_then(|i| i.as_str()))
                                .unwrap_or("user");
                            text.push('@');
                            text.push_str(n);
                        }
                        _ => {}
                    }
                }
                text.push('\n');
            }
        }
    }

    let result = text.trim().to_string();
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Remove `@_user_N` placeholder tokens injected by Feishu in group chats.
fn strip_at_placeholders(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch == '@' {
            // Look ahead into the original string by byte offset. This used to
            // collect the whole remaining text into a fresh `String` on every
            // '@', which is an allocation and a copy per mention.
            let rest = &text[idx + ch.len_utf8()..];
            // A placeholder always carries an index. Without the digit check
            // `@_user_` followed by anything at all was treated as one, so a
            // literal `@_user_x` lost its prefix and the text was silently
            // altered. Feishu never emits an unindexed placeholder.
            if let Some(after) = rest
                .strip_prefix("_user_")
                .filter(|a| a.starts_with(|c: char| c.is_ascii_digit()))
            {
                let skip =
                    "_user_".len() + after.chars().take_while(|c| c.is_ascii_digit()).count();
                // Exactly the placeholder, nothing more. `0..=skip` consumed one
                // character too many. With a space after the mention the extra
                // character was that space, so the result looked right; with no
                // space — the normal case in CJK, Lark's primary market — it ate
                // the first character of the message itself.
                for _ in 0..skip {
                    chars.next();
                }
                if chars.peek().is_some_and(|(_, c)| *c == ' ') {
                    chars.next();
                }
                continue;
            }
        }
        result.push(ch);
    }
    result
}

/// Who the bot is, as the platform reports it. Used to tell a mention *of the
/// bot* from a mention of anyone else.
#[derive(Debug, Clone, Default)]
pub(crate) struct BotIdentity {
    pub open_id: String,
    pub name: String,
}

/// In group chats, only respond when **the bot** is explicitly @-mentioned.
///
/// This used to be `!mentions.is_empty()`, so the bot answered whenever anyone
/// at all was mentioned — in an active group that is close to answering
/// everything.
///
/// Two sources, mirroring `contains_bot_mention_mm`: the mention's `id.open_id`
/// and its display `name`. Lark populates the id for app mentions and the name
/// for both, and neither is reliable alone.
///
/// `identity` is `None` when the bot-info lookup failed at startup. That falls
/// back to the old permissive behaviour on purpose: the alternative — treating
/// an unknown identity as "never mentioned" — silences the channel completely
/// on a transient API error, which reads as a broken bot rather than a
/// degraded one. The caller warns when it happens.
fn should_respond_in_group(mentions: &[serde_json::Value], identity: Option<&BotIdentity>) -> bool {
    let Some(me) = identity else {
        return !mentions.is_empty();
    };

    mentions.iter().any(|m| {
        let id_match = !me.open_id.is_empty()
            && m.pointer("/id/open_id")
                .and_then(|v| v.as_str())
                .is_some_and(|v| v == me.open_id);

        let name_match = !me.name.is_empty()
            && m.get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|v| v.eq_ignore_ascii_case(&me.name));

        id_match || name_match
    })
}

#[cfg(test)]
mod allowlist_runtime_tests {
    use super::LarkChannel;
    use crate::channels::traits::Channel;

    fn ch(users: Vec<&str>) -> LarkChannel {
        LarkChannel::new(
            "cli_test_app_id".into(),
            "test_app_secret".into(),
            "test_verification_token".into(),
            None,
            users.into_iter().map(String::from).collect(),
        )
    }

    /// A freshly paired identity must work on the very next message. Before
    /// this the allowlist was a plain `Vec` fixed at construction, so pairing
    /// persisted to `config.toml` and then did nothing until a restart.
    #[test]
    fn a_paired_identity_is_allowed_immediately() {
        let c = ch(vec![]);
        assert!(!c.is_user_allowed("ou_newcomer"));
        c.add_allowed_identity_runtime("ou_newcomer");
        assert!(c.is_user_allowed("ou_newcomer"));
    }

    #[test]
    fn granting_twice_does_not_duplicate() {
        let c = ch(vec![]);
        c.add_allowed_identity_runtime("ou_newcomer");
        c.add_allowed_identity_runtime("ou_newcomer");
        assert_eq!(c.allowed_users.read().unwrap().len(), 1);
    }

    #[test]
    fn blank_identities_are_ignored() {
        let c = ch(vec![]);
        c.add_allowed_identity_runtime("   ");
        assert!(c.allowed_users.read().unwrap().is_empty());
    }

    /// A config edit reaches the live channel, including a removal — the point
    /// of the trait method, not just additions.
    #[test]
    fn apply_allowed_senders_replaces_the_live_list() {
        let c = ch(vec!["ou_old"]);
        c.apply_allowed_senders(&["ou_new".to_string()]);
        assert!(c.is_user_allowed("ou_new"));
        assert!(
            !c.is_user_allowed("ou_old"),
            "a removal must take effect too, or revoking access needs a restart"
        );
    }
}

#[cfg(test)]
mod mention_gate_tests {
    use super::{should_respond_in_group, BotIdentity};

    fn me() -> BotIdentity {
        BotIdentity {
            open_id: "ou_rantaiclaw_bot".into(),
            name: "RantaiClawAgent".into(),
        }
    }

    fn mention(open_id: &str, name: &str) -> serde_json::Value {
        serde_json::json!({ "key": "@_user_1", "id": { "open_id": open_id }, "name": name })
    }

    /// The defect: the gate was `!mentions.is_empty()`, so mentioning any
    /// colleague in a busy group made the bot answer.
    #[test]
    fn someone_elses_mention_does_not_wake_the_bot() {
        let others = vec![mention("ou_someone_else", "Another Person")];
        assert!(!should_respond_in_group(&others, Some(&me())));
    }

    #[test]
    fn the_bots_own_open_id_wakes_it() {
        let m = vec![mention("ou_rantaiclaw_bot", "")];
        assert!(should_respond_in_group(&m, Some(&me())));
    }

    /// Lark populates the id for app mentions and the name for both, so
    /// neither source is sufficient alone.
    #[test]
    fn the_bots_display_name_also_wakes_it() {
        let m = vec![mention("", "rantaiclawagent")];
        assert!(
            should_respond_in_group(&m, Some(&me())),
            "name match must be case-insensitive"
        );
    }

    #[test]
    fn the_bot_is_found_among_several_mentions() {
        let m = vec![
            mention("ou_someone_else", "Another Person"),
            mention("ou_rantaiclaw_bot", "RantaiClawAgent"),
        ];
        assert!(should_respond_in_group(&m, Some(&me())));
    }

    #[test]
    fn no_mentions_never_wakes_it() {
        assert!(!should_respond_in_group(&[], Some(&me())));
    }

    /// When the identity lookup failed we keep the old permissive behaviour
    /// rather than going silent — a degraded bot beats one that looks broken.
    #[test]
    fn an_unknown_identity_falls_back_to_permissive() {
        let others = vec![mention("ou_someone_else", "Another Person")];
        assert!(should_respond_in_group(&others, None));
        assert!(!should_respond_in_group(&[], None));
    }
}

#[cfg(test)]
mod placeholder_tests {
    use super::strip_at_placeholders;

    /// The regression this fixes. Feishu injects `@_user_1` with no trailing
    /// space in front of CJK text, and the old `0..=skip` ate the first real
    /// character with it — so every mention-prefixed Chinese message arrived
    /// missing its opening character.
    #[test]
    fn strips_the_placeholder_without_eating_the_next_character() {
        assert_eq!(strip_at_placeholders("@_user_1你好世界"), "你好世界");
        assert_eq!(strip_at_placeholders("@_user_12今天天气"), "今天天气");
    }

    /// The case that masked it: with a space after the mention the extra
    /// consumed character happened to be that space, so the output looked
    /// correct and the bug stayed invisible in ASCII testing.
    #[test]
    fn a_trailing_space_is_still_swallowed_exactly_once() {
        assert_eq!(strip_at_placeholders("@_user_1 hello"), "hello");
        assert_eq!(strip_at_placeholders("@_user_1  hello"), " hello");
    }

    #[test]
    fn multiple_placeholders_are_all_removed() {
        assert_eq!(
            strip_at_placeholders("@_user_1 @_user_2 ping"),
            "ping",
            "each mention consumes itself and one following space"
        );
    }

    /// A bare `@` or an unrelated handle must survive untouched.
    #[test]
    fn non_placeholder_at_signs_are_left_alone() {
        assert_eq!(strip_at_placeholders("email me @ work"), "email me @ work");
        assert_eq!(strip_at_placeholders("@someone hi"), "@someone hi");
        assert_eq!(strip_at_placeholders("@_user_x hi"), "@_user_x hi");
    }

    /// Multi-byte input must not panic on the byte-offset lookahead.
    #[test]
    fn multibyte_text_without_placeholders_round_trips() {
        let s = "こんにちは、世界！ 🎉";
        assert_eq!(strip_at_placeholders(s), s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel() -> LarkChannel {
        LarkChannel::new(
            "cli_test_app_id".into(),
            "test_app_secret".into(),
            "test_verification_token".into(),
            None,
            vec!["ou_testuser123".into()],
        )
    }

    #[test]
    fn lark_channel_name() {
        let ch = make_channel();
        assert_eq!(ch.name(), "lark");
    }

    #[test]
    fn lark_ws_activity_refreshes_heartbeat_watchdog() {
        assert!(should_refresh_last_recv(&WsMsg::Binary(
            vec![1, 2, 3].into()
        )));
        assert!(should_refresh_last_recv(&WsMsg::Ping(vec![9, 9].into())));
        assert!(should_refresh_last_recv(&WsMsg::Pong(vec![8, 8].into())));
    }

    #[test]
    fn lark_ws_non_activity_frames_do_not_refresh_heartbeat_watchdog() {
        assert!(!should_refresh_last_recv(&WsMsg::Text("hello".into())));
        assert!(!should_refresh_last_recv(&WsMsg::Close(None)));
    }

    #[test]
    fn lark_should_refresh_token_on_http_401() {
        let body = serde_json::json!({ "code": 0 });
        assert!(should_refresh_lark_tenant_token(
            reqwest::StatusCode::UNAUTHORIZED,
            &body
        ));
    }

    #[test]
    fn lark_should_refresh_token_on_body_code_99991663() {
        let body = serde_json::json!({
            "code": LARK_INVALID_ACCESS_TOKEN_CODE,
            "msg": "Invalid access token for authorization."
        });
        assert!(should_refresh_lark_tenant_token(
            reqwest::StatusCode::OK,
            &body
        ));
    }

    #[test]
    fn lark_should_not_refresh_token_on_success_body() {
        let body = serde_json::json!({ "code": 0, "msg": "ok" });
        assert!(!should_refresh_lark_tenant_token(
            reqwest::StatusCode::OK,
            &body
        ));
    }

    #[test]
    fn lark_extract_token_ttl_seconds_supports_expire_and_expires_in() {
        let body_expire = serde_json::json!({ "expire": 7200 });
        let body_expires_in = serde_json::json!({ "expires_in": 3600 });
        let body_missing = serde_json::json!({});
        assert_eq!(extract_lark_token_ttl_seconds(&body_expire), 7200);
        assert_eq!(extract_lark_token_ttl_seconds(&body_expires_in), 3600);
        assert_eq!(
            extract_lark_token_ttl_seconds(&body_missing),
            LARK_DEFAULT_TOKEN_TTL.as_secs()
        );
    }

    #[test]
    fn lark_next_token_refresh_deadline_reserves_refresh_skew() {
        let now = Instant::now();
        let regular = next_token_refresh_deadline(now, 7200);
        let short_ttl = next_token_refresh_deadline(now, 60);

        assert_eq!(regular.duration_since(now), Duration::from_secs(7080));
        assert_eq!(short_ttl.duration_since(now), Duration::from_secs(1));
    }

    #[test]
    fn lark_ensure_send_success_rejects_non_zero_code() {
        let ok = serde_json::json!({ "code": 0 });
        let bad = serde_json::json!({ "code": 12345, "msg": "bad request" });

        assert!(ensure_lark_send_success(reqwest::StatusCode::OK, &ok, "test").is_ok());
        assert!(ensure_lark_send_success(reqwest::StatusCode::OK, &bad, "test").is_err());
    }

    #[test]
    fn lark_user_allowed_exact() {
        let ch = make_channel();
        assert!(ch.is_user_allowed("ou_testuser123"));
        assert!(!ch.is_user_allowed("ou_other"));
    }

    #[test]
    fn lark_user_allowed_wildcard() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            "token".into(),
            None,
            vec!["*".into()],
        );
        assert!(ch.is_user_allowed("ou_anyone"));
    }

    #[test]
    fn lark_user_denied_empty() {
        let ch = LarkChannel::new("id".into(), "secret".into(), "token".into(), None, vec![]);
        assert!(!ch.is_user_allowed("ou_anyone"));
    }

    #[test]
    fn lark_parse_challenge() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "challenge": "abc123",
            "token": "test_verification_token",
            "type": "url_verification"
        });
        // Challenge payloads should not produce messages
        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    /// The gate that decides this lives inside `parse_event_payload`; the
    /// suite only ever exercised `is_user_allowed` directly, so the two could
    /// drift apart with everything green.
    #[test]
    fn lark_parse_event_payload_drops_an_unlisted_sender() {
        let ch = make_channel();
        let payload = |open_id: &str| {
            serde_json::json!({
                "header": { "event_type": "im.message.receive_v1" },
                "event": {
                    "sender": { "sender_id": { "open_id": open_id } },
                    "message": {
                        "message_type": "text",
                        "content": "{\"text\":\"status please\"}",
                        "chat_id": "oc_chat123",
                        "message_id": "om_msg123",
                        "create_time": "1699999999000"
                    }
                }
            })
        };

        assert!(
            ch.parse_event_payload(&payload("ou_stranger")).is_empty(),
            "a sender outside allowed_users must not produce a message"
        );
        // Control on the same fixture: the allowlisted sender does.
        assert_eq!(ch.parse_event_payload(&payload("ou_testuser123")).len(), 1);
    }

    #[test]
    fn lark_parse_valid_text_message() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "header": {
                "event_type": "im.message.receive_v1"
            },
            "event": {
                "sender": {
                    "sender_id": {
                        "open_id": "ou_testuser123"
                    }
                },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"Hello RantaiClaw!\"}",
                    "chat_id": "oc_chat123",
                    "message_id": "om_msg123",
                    "create_time": "1699999999000"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Hello RantaiClaw!");
        assert_eq!(msgs[0].channel, "lark");
        assert_eq!(msgs[0].timestamp, 1_699_999_999);
        // The platform id, not a fresh UUID: Lark retries a callback it
        // considers unacked, and a UUID makes that redelivery undetectable.
        assert_eq!(msgs[0].id, "lark_om_msg123");

        // Identity is the person; the reply still routes to the room. This
        // assertion used to read `sender == "oc_chat123"` — it encoded the bug
        // rather than the contract, so the owner gate comparing against a chat
        // id looked correct to the suite.
        assert_eq!(
            msgs[0].sender, "ou_testuser123",
            "sender must be the person, or `approval_owners` matches a whole room"
        );
        assert_eq!(
            msgs[0].reply_target, "oc_chat123",
            "the reply still goes to the chat"
        );
        assert!(
            msgs[0].sender_aliases.contains(&"oc_chat123".to_string()),
            "the chat id stays reachable as an alias: {:?}",
            msgs[0].sender_aliases
        );
    }

    #[test]
    fn lark_parse_unauthorized_user() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_unauthorized" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"spam\"}",
                    "chat_id": "oc_chat",
                    "create_time": "1000"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_non_text_message_skipped() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            "token".into(),
            None,
            vec!["*".into()],
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "image",
                    "content": "{}",
                    "chat_id": "oc_chat"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_empty_text_skipped() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            "token".into(),
            None,
            vec!["*".into()],
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"\"}",
                    "chat_id": "oc_chat"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_wrong_event_type() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "header": { "event_type": "im.chat.disbanded_v1" },
            "event": {}
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_missing_sender() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            "token".into(),
            None,
            vec!["*".into()],
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"hello\"}",
                    "chat_id": "oc_chat"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_unicode_message() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            "token".into(),
            None,
            vec!["*".into()],
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"Hello world 🌍\"}",
                    "chat_id": "oc_chat",
                    "create_time": "1000"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Hello world 🌍");
    }

    #[test]
    fn lark_parse_missing_event() {
        let ch = make_channel();
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_parse_invalid_content_json() {
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            "token".into(),
            None,
            vec!["*".into()],
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "not valid json",
                    "chat_id": "oc_chat"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert!(msgs.is_empty());
    }

    #[test]
    fn lark_config_serde() {
        use crate::config::schema::{LarkConfig, LarkReceiveMode};
        let lc = LarkConfig {
            app_id: "cli_app123".into(),
            app_secret: "secret456".into(),
            encrypt_key: None,
            verification_token: Some("vtoken789".into()),
            allowed_users: vec!["ou_user1".into(), "ou_user2".into()],
            use_feishu: false,
            receive_mode: LarkReceiveMode::default(),
            port: None,
        };
        let json = serde_json::to_string(&lc).unwrap();
        let parsed: LarkConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.app_id, "cli_app123");
        assert_eq!(parsed.app_secret, "secret456");
        assert_eq!(parsed.verification_token.as_deref(), Some("vtoken789"));
        assert_eq!(parsed.allowed_users.len(), 2);
    }

    #[test]
    fn lark_config_toml_roundtrip() {
        use crate::config::schema::{LarkConfig, LarkReceiveMode};
        let lc = LarkConfig {
            app_id: "app".into(),
            app_secret: "secret".into(),
            encrypt_key: None,
            verification_token: Some("tok".into()),
            allowed_users: vec!["*".into()],
            use_feishu: false,
            receive_mode: LarkReceiveMode::Webhook,
            port: Some(9898),
        };
        let toml_str = toml::to_string(&lc).unwrap();
        let parsed: LarkConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.app_id, "app");
        assert_eq!(parsed.verification_token.as_deref(), Some("tok"));
        assert_eq!(parsed.allowed_users, vec!["*"]);
    }

    #[test]
    fn lark_config_defaults_optional_fields() {
        use crate::config::schema::{LarkConfig, LarkReceiveMode};
        let json = r#"{"app_id":"a","app_secret":"s"}"#;
        let parsed: LarkConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.verification_token.is_none());
        assert!(parsed.allowed_users.is_empty());
        assert_eq!(parsed.receive_mode, LarkReceiveMode::Websocket);
        assert!(parsed.port.is_none());
    }

    #[test]
    fn lark_from_config_preserves_mode_and_region() {
        use crate::config::schema::{LarkConfig, LarkReceiveMode};

        let cfg = LarkConfig {
            app_id: "cli_app123".into(),
            app_secret: "secret456".into(),
            encrypt_key: None,
            verification_token: Some("vtoken789".into()),
            allowed_users: vec!["*".into()],
            use_feishu: false,
            receive_mode: LarkReceiveMode::Webhook,
            port: Some(9898),
        };

        let ch = LarkChannel::from_config(&cfg);

        assert_eq!(ch.api_base(), LARK_BASE_URL);
        assert_eq!(ch.ws_base(), LARK_WS_BASE_URL);
        assert_eq!(ch.receive_mode, LarkReceiveMode::Webhook);
        assert_eq!(ch.port, Some(9898));
    }

    #[test]
    fn lark_parse_fallback_sender_to_open_id() {
        // `sender` is the open_id regardless; this covers the case where the
        // event carries no chat_id, so `reply_target` has to fall back to it
        // too. The comment here used to read "sender should fall back to
        // open_id", which described the inverted model — the person was the
        // fallback and the room was the identity.
        let ch = LarkChannel::new(
            "id".into(),
            "secret".into(),
            "token".into(),
            None,
            vec!["*".into()],
        );
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_user" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"hello\"}",
                    "create_time": "1000"
                }
            }
        });

        let msgs = ch.parse_event_payload(&payload);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender, "ou_user");
    }

    #[test]
    fn lark_reaction_url_matches_region() {
        let ch_cn = make_channel();
        assert_eq!(
            ch_cn.message_reaction_url("om_test_message_id"),
            "https://open.feishu.cn/open-apis/im/v1/messages/om_test_message_id/reactions"
        );

        let mut ch_intl = make_channel();
        ch_intl.use_feishu = false;
        assert_eq!(
            ch_intl.message_reaction_url("om_test_message_id"),
            "https://open.larksuite.com/open-apis/im/v1/messages/om_test_message_id/reactions"
        );
    }

    /// The pairing context is extracted even for a user not (yet) in the
    /// allowlist, so an unenrolled user's `/bind`/`/claim` reaches the shared core;
    /// the reply routes to the chat_id, the identity is the open_id.
    #[test]
    fn lark_extract_pairing_context_ignores_allowlist() {
        let payload = serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": "ou_not_allowed" } },
                "message": {
                    "message_type": "text",
                    "content": "{\"text\":\"/bind ABCD-EFGH\"}",
                    "chat_id": "oc_chat"
                }
            }
        });
        let (text, open_id, chat_id) =
            LarkChannel::extract_pairing_context(&payload).expect("should extract");
        assert_eq!(text, "/bind ABCD-EFGH");
        assert_eq!(open_id, "ou_not_allowed");
        assert_eq!(chat_id, "oc_chat");
    }

    /// A store-minted owner code consumed for the `lark` surface appends the
    /// sender open_id to `allowed_users` and `approval_owners` and persists the
    /// config — the shared-core path `try_handle_store_pairing` invokes before the
    /// allowlist gate (the API reply send is exercised in integration, so we assert
    /// the store + config mutation here).
    #[tokio::test]
    async fn lark_store_minted_claim_grants_owner() {
        use crate::channels::pairing::{try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        let _guard = crate::test_env::ENV_LOCK.lock().await;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");
        {
            let mut seed = crate::config::Config::load_or_init().await.unwrap();
            seed.channels_config.lark = Some(crate::config::schema::LarkConfig {
                app_id: "id".into(),
                app_secret: "secret".into(),
                encrypt_key: None,
                verification_token: None,
                allowed_users: vec![],
                use_feishu: false,
                receive_mode: crate::config::schema::LarkReceiveMode::default(),
                port: None,
            });
            seed.save().await.unwrap();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let code = pairing_store::mint(root, "lark", 900, None, true, now).unwrap();

        let reply = try_handle_pairing(
            &format!("/claim {code}"),
            "lark",
            AllowlistField::AllowedUsers,
            &["ou_new".to_string()],
            root,
        )
        .await
        .expect("pairing command should be handled");
        assert!(reply.contains("owner"), "reply was: {reply}");

        let config = crate::config::Config::load_or_init().await.unwrap();
        let users = &config.channels_config.lark.as_ref().unwrap().allowed_users;
        assert!(users.contains(&"ou_new".to_string()));
        assert!(config
            .channels_config
            .approval_owners
            .contains(&"ou_new".to_string()));

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }
}
