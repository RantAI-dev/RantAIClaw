//! WhatsApp Web channel using wa-rs (native Rust implementation)
//!
//! This channel provides direct WhatsApp Web integration with:
//! - QR code and pair code linking
//! - End-to-end encryption via Signal Protocol
//! - Full Baileys parity (groups, media, presence, reactions, editing/deletion)
//!
//! # Feature Flag
//!
//! This channel requires the `whatsapp-web` feature flag:
//! ```sh
//! cargo build --features whatsapp-web
//! ```
//!
//! # Configuration
//!
//! ```toml
//! [channels_config.whatsapp]
//! session_path = "~/.rantaiclaw/whatsapp-session.db"  # Required for Web mode
//! pair_phone = "15551234567"  # Optional: for pair code linking
//! allowed_numbers = ["+1234567890", "*"]  # Same as Cloud API
//! ```
//!
//! # Runtime Negotiation
//!
//! This channel is automatically selected when `session_path` is set in the config.
//! The Cloud API channel is used when `phone_number_id` is set.

use super::traits::{Channel, ChannelMessage, SendMessage};
use super::whatsapp_storage::RusqliteStore;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::sync::Arc;
#[cfg(feature = "whatsapp-web")]
use std::sync::RwLock;
use tokio::select;

/// WhatsApp Web channel using wa-rs with custom rusqlite storage
///
/// # Status: Functional Implementation
///
/// This implementation uses the wa-rs Bot with our custom RusqliteStore backend.
///
/// # Configuration
///
/// ```toml
/// [channels_config.whatsapp]
/// session_path = "~/.rantaiclaw/whatsapp-session.db"
/// pair_phone = "15551234567"  # Optional
/// allowed_numbers = ["+1234567890", "*"]
/// ```
#[cfg(feature = "whatsapp-web")]
pub struct WhatsAppWebChannel {
    /// Session database path
    session_path: String,
    /// Phone number for pair code linking (optional)
    pair_phone: Option<String>,
    /// Custom pair code (optional)
    pair_code: Option<String>,
    /// Allowed phone numbers (E.164 format) or "*" for all. Behind a lock so an
    /// in-chat `/bind`/`/claim` can extend it at runtime without a restart.
    allowed_numbers: Arc<RwLock<Vec<String>>>,
    /// Bot handle for shutdown
    bot_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Client handle for sending messages and typing indicators
    client: Arc<Mutex<Option<Arc<wa_rs::Client>>>>,
    /// Message sender channel
    tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<ChannelMessage>>>>,
}

impl WhatsAppWebChannel {
    /// Create a new WhatsApp Web channel
    ///
    /// # Arguments
    ///
    /// * `session_path` - Path to the SQLite session database
    /// * `pair_phone` - Optional phone number for pair code linking (format: "15551234567")
    /// * `pair_code` - Optional custom pair code (leave empty for auto-generated)
    /// * `allowed_numbers` - Phone numbers allowed to interact (E.164 format) or "*" for all
    #[cfg(feature = "whatsapp-web")]
    pub fn new(
        session_path: String,
        pair_phone: Option<String>,
        pair_code: Option<String>,
        allowed_numbers: Vec<String>,
    ) -> Self {
        Self {
            session_path,
            pair_phone,
            pair_code,
            allowed_numbers: Arc::new(RwLock::new(allowed_numbers)),
            bot_handle: Arc::new(Mutex::new(None)),
            client: Arc::new(Mutex::new(None)),
            tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Check if a phone number is allowed (E.164 format: +1234567890)
    #[cfg(feature = "whatsapp-web")]
    fn is_number_allowed(&self, phone: &str) -> bool {
        Self::number_allowed_in(&self.allowed_numbers, phone)
    }

    /// Whether `phone` is permitted by the given allowlist snapshot. Shared by
    /// the channel method and the event loop (which holds an `Arc` clone).
    #[cfg(feature = "whatsapp-web")]
    fn number_allowed_in(allowed: &Arc<RwLock<Vec<String>>>, phone: &str) -> bool {
        let Ok(allowed) = allowed.read() else {
            return false;
        };
        allowed.iter().any(|n| n == "*" || n == phone)
    }

    /// Append a freshly-paired number to the runtime allowlist so a successful
    /// `/bind`/`/claim` takes effect immediately, before the persisted config is
    /// reloaded on the next restart.
    #[cfg(feature = "whatsapp-web")]
    fn add_allowed_number_in(allowed: &Arc<RwLock<Vec<String>>>, phone: &str) {
        let phone = phone.trim();
        if phone.is_empty() {
            return;
        }
        if let Ok(mut allowed) = allowed.write() {
            if !allowed.iter().any(|n| n == phone) {
                allowed.push(phone.to_string());
            }
        }
    }

    /// Resolve the active profile root for the shared pairing-code store.
    #[cfg(feature = "whatsapp-web")]
    fn pairing_profile_root() -> Option<std::path::PathBuf> {
        match crate::profile::ProfileManager::active() {
            Ok(p) => Some(p.root),
            Err(e) => {
                tracing::warn!("WhatsApp Web pairing: couldn't resolve profile root: {e:#}");
                None
            }
        }
    }

    /// Try to handle `text` from `phone` (already normalized to `+E.164`) as a
    /// `/bind`/`/claim` against the shared pairing store at `root` (surface
    /// `"whatsapp"`).
    ///
    /// Returns `Some(reply)` when the message WAS a live pairing command — the
    /// caller must then send the reply and NOT forward the message — and `None`
    /// otherwise (normal message, or no live store code). On a hit it appends the
    /// sender to `allowed_numbers` (+ `approval_owners` for an owner-capable
    /// `/claim`) and persists `config.toml` via the shared core, then extends the
    /// supplied runtime allowlist for immediate effect. Extracted as a free-
    /// standing helper (takes `root` explicitly) so the wa-rs event loop stays
    /// thin and this stays unit-testable against a tempdir store.
    #[cfg(feature = "whatsapp-web")]
    async fn handle_pairing_for(
        allowed_numbers: &Arc<RwLock<Vec<String>>>,
        text: &str,
        phone: &str,
        root: &std::path::Path,
    ) -> Option<String> {
        use crate::channels::pairing::{parse_pairing_command, try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        let cmd = parse_pairing_command(text)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        match pairing_store::contains(root, "whatsapp", &cmd.code, now) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(e) => {
                tracing::warn!("WhatsApp Web pairing store probe failed: {e:#}");
                return None;
            }
        }

        let reply = try_handle_pairing(
            text,
            "whatsapp",
            AllowlistField::AllowedNumbers,
            &[phone.to_string()],
            root,
        )
        .await?;

        Self::add_allowed_number_in(allowed_numbers, phone);
        Some(reply)
    }

    /// Run the full pairing branch for one inbound message and send the reply via
    /// the live wa-rs `client`. Returns `true` when the message WAS a pairing
    /// command (caller must not forward it). Kept as its own `async fn` (rather
    /// than inlined into the event-loop closure) so its sizeable future —
    /// `Config` load/save + a wa-rs `send_message` — does not bloat the closure's
    /// future; the caller `Box::pin`s this.
    #[cfg(feature = "whatsapp-web")]
    async fn try_reply_pairing(
        allowed_numbers: &Arc<RwLock<Vec<String>>>,
        client: &Arc<wa_rs::Client>,
        text: &str,
        phone: &str,
        chat_jid: wa_rs_binary::jid::Jid,
    ) -> bool {
        let Some(root) = Self::pairing_profile_root() else {
            return false;
        };
        let Some(reply) = Self::handle_pairing_for(allowed_numbers, text, phone, &root).await
        else {
            return false;
        };
        let outgoing = wa_rs_proto::whatsapp::Message {
            conversation: Some(reply),
            ..Default::default()
        };
        // `send_message` returns a large future; box it so it doesn't bloat this
        // fn's (already boxed) future further.
        if let Err(e) = Box::pin(client.send_message(chat_jid, outgoing)).await {
            tracing::error!("WhatsApp Web pairing reply send failed: {e}");
        }
        true
    }

    /// Normalize phone number to E.164 format (strips JID domain, ensures + prefix)
    #[cfg(feature = "whatsapp-web")]
    fn normalize_phone(&self, phone: &str) -> String {
        let trimmed = phone.trim();
        let user_part = trimmed
            .split_once('@')
            .map(|(user, _)| user)
            .unwrap_or(trimmed);
        if user_part.starts_with('+') {
            user_part.to_string()
        } else {
            format!("+{user_part}")
        }
    }

    /// Whether the recipient string is a WhatsApp JID (contains a domain suffix).
    #[cfg(feature = "whatsapp-web")]
    fn is_jid(recipient: &str) -> bool {
        recipient.trim().contains('@')
    }

    /// Convert a recipient to a wa-rs JID.
    ///
    /// Supports:
    /// - Full JIDs (e.g. "12345@s.whatsapp.net")
    /// - E.164-like numbers (e.g. "+1234567890")
    #[cfg(feature = "whatsapp-web")]
    fn recipient_to_jid(&self, recipient: &str) -> Result<wa_rs_binary::jid::Jid> {
        let trimmed = recipient.trim();
        if trimmed.is_empty() {
            anyhow::bail!("Recipient cannot be empty");
        }

        if trimmed.contains('@') {
            return trimmed
                .parse::<wa_rs_binary::jid::Jid>()
                .map_err(|e| anyhow!("Invalid WhatsApp JID `{trimmed}`: {e}"));
        }

        let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            anyhow::bail!("Recipient `{trimmed}` does not contain a valid phone number");
        }

        Ok(wa_rs_binary::jid::Jid::pn(digits))
    }

    /// Resolve an inbound chat JID to the addressing WhatsApp actually delivers
    /// 1:1 replies on.
    ///
    /// WhatsApp hands many direct chats to us **LID-addressed** (`<id>@lid`, a
    /// privacy identifier — not a phone number). Replying to the bare LID lands
    /// in a separate thread the recipient never sees (the "bot types but never
    /// answers" symptom): wa-rs preserves a LID target as-is and only resolves
    /// PN→LID for the encryption session, so a LID `to` is delivered to the LID
    /// thread rather than the visible phone-number chat.
    ///
    /// When the chat is a LID and wa-rs has learned the phone-number mapping
    /// from the inbound message (its `lid_pn_cache`), reply on the phone-number
    /// (PN) thread instead. Falls back to the original JID for groups,
    /// broadcasts, and unmapped LIDs so nothing regresses.
    #[cfg(feature = "whatsapp-web")]
    async fn resolve_reply_target(client: &wa_rs::Client, chat: &wa_rs_binary::jid::Jid) -> String {
        use wa_rs_binary::jid::{JidExt as _, DEFAULT_USER_SERVER, HIDDEN_USER_SERVER};
        if chat.server() == HIDDEN_USER_SERVER {
            if let Some(pn) = client.get_phone_number_from_lid(chat.user()).await {
                return format!("{pn}@{DEFAULT_USER_SERVER}");
            }
        }
        chat.to_string()
    }

    /// Normalize an inbound sender to the E.164 `+` form used for allowlist and
    /// owner matching. `resolved_pn` is the phone number a LID resolved to (when
    /// known); otherwise the raw user part is used. Pure so it is unit-testable
    /// without a live wa-rs client.
    #[cfg(feature = "whatsapp-web")]
    fn normalize_sender(resolved_pn: Option<&str>, sender_user: &str) -> String {
        match resolved_pn {
            Some(pn) => format!("+{pn}"),
            None if sender_user.starts_with('+') => sender_user.to_string(),
            None => format!("+{sender_user}"),
        }
    }
}

#[cfg(feature = "whatsapp-web")]
#[async_trait]
impl Channel for WhatsAppWebChannel {
    fn name(&self) -> &str {
        "whatsapp"
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        let client = self.client.lock().clone();
        let Some(client) = client else {
            anyhow::bail!("WhatsApp Web client not connected. Initialize the bot first.");
        };

        // Validate recipient allowlist only for direct phone-number targets.
        if !Self::is_jid(&message.recipient) {
            let normalized = self.normalize_phone(&message.recipient);
            if !self.is_number_allowed(&normalized) {
                tracing::warn!(
                    "WhatsApp Web: recipient {} not in allowed list",
                    message.recipient
                );
                return Ok(());
            }
        }

        let to = self.recipient_to_jid(&message.recipient)?;
        let outgoing = wa_rs_proto::whatsapp::Message {
            conversation: Some(message.content.clone()),
            ..Default::default()
        };

        let message_id = client.send_message(to, outgoing).await?;
        tracing::debug!(
            "WhatsApp Web: sent message to {} (id: {})",
            message.recipient,
            message_id
        );
        Ok(())
    }

    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        // Store the sender channel for incoming messages
        *self.tx.lock() = Some(tx.clone());

        use wa_rs::bot::Bot;
        use wa_rs::pair_code::PairCodeOptions;
        use wa_rs::store::{Device, DeviceStore};
        use wa_rs_binary::jid::JidExt as _;
        use wa_rs_core::proto_helpers::MessageExt;
        use wa_rs_core::types::events::Event;
        use wa_rs_tokio_transport::TokioWebSocketTransportFactory;
        use wa_rs_ureq_http::UreqHttpClient;

        tracing::info!(
            "WhatsApp Web channel starting (session: {})",
            self.session_path
        );

        // Initialize storage backend
        let storage = RusqliteStore::new(&self.session_path)?;
        let backend = Arc::new(storage);

        // Check if we have a saved device to load
        let mut device = Device::new(backend.clone());
        if backend.exists().await? {
            tracing::info!("WhatsApp Web: found existing session, loading device");
            if let Some(core_device) = backend.load().await? {
                device.load_from_serializable(core_device);
            } else {
                anyhow::bail!("Device exists but failed to load");
            }
        } else {
            tracing::info!(
                "WhatsApp Web: no existing session, new device will be created during pairing"
            );
        };

        // Create transport factory
        let mut transport_factory = TokioWebSocketTransportFactory::new();
        if let Ok(ws_url) = std::env::var("WHATSAPP_WS_URL") {
            transport_factory = transport_factory.with_url(ws_url);
        }

        // Create HTTP client for media operations
        let http_client = UreqHttpClient::new();

        // Build the bot
        let tx_clone = tx.clone();
        let allowed_numbers = self.allowed_numbers.clone();

        let mut builder = Bot::builder()
            .with_backend(backend)
            .with_transport_factory(transport_factory)
            .with_http_client(http_client)
            .on_event(move |event, client| {
                let tx_inner = tx_clone.clone();
                let allowed_numbers = allowed_numbers.clone();
                async move {
                    match event {
                        Event::Message(msg, info) => {
                            // Extract message content
                            let text = msg.text_content().unwrap_or("");
                            let sender = info.source.sender.user().to_string();
                            let sender_jid = info.source.sender.to_string();
                            let chat_jid = info.source.chat.clone();
                            let chat = chat_jid.to_string();

                            tracing::info!(
                                "WhatsApp Web message from {} in {}: {}",
                                sender,
                                chat,
                                text
                            );

                            // Detect LID (Linked Identity) senders — WhatsApp often
                            // addresses 1:1 chats by an opaque LID instead of the
                            // phone number. Resolve it to the phone number (learned
                            // in wa-rs's `lid_pn_cache`, including from this very
                            // message) so owner/allowlist matching runs on the REAL
                            // number. Without this the sender never equals an entry
                            // in `approval_owners`, so the user is silently treated
                            // as a guest and every owner-only feature (cron,
                            // permissions, owner commands) is gated off.
                            let is_lid = sender_jid.contains("@lid");
                            let resolved_pn = if is_lid {
                                client.get_phone_number_from_lid(&sender).await
                            } else {
                                None
                            };

                            // Sender in E.164 `+` form (resolved phone number when
                            // available, else the raw user part).
                            let normalized =
                                Self::normalize_sender(resolved_pn.as_deref(), &sender);

                            // Intercept on-demand store-minted pairing codes
                            // (`/bind`/`/claim`) BEFORE the allowlist gate so an
                            // unknown number can self-onboard without a restart.
                            // Never forwarded to the agent. Boxed so the pairing
                            // future (config I/O + send) doesn't bloat this loop.
                            let handled = Box::pin(Self::try_reply_pairing(
                                &allowed_numbers,
                                &client,
                                text,
                                &normalized,
                                chat_jid.clone(),
                            ))
                            .await;
                            if handled {
                                return;
                            }

                            // A LID resolved to a phone number is matched like any
                            // number. An *unmapped* LID is unverifiable, so allow it
                            // through when the list is non-empty / has "*" (the user
                            // configured filtering intent). Wildcard "*" always passes.
                            let is_allowed = if is_lid && resolved_pn.is_none() {
                                let allowed = allowed_numbers.read().ok();
                                allowed.is_some_and(|a| a.iter().any(|n| n == "*") || !a.is_empty())
                            } else {
                                Self::number_allowed_in(&allowed_numbers, &normalized)
                            };

                            if is_allowed {
                                let trimmed = text.trim();
                                if trimmed.is_empty() {
                                    tracing::debug!(
                                        "WhatsApp Web: ignoring empty or non-text message from {}",
                                        normalized
                                    );
                                    return;
                                }

                                // Reply on the chat WhatsApp actually delivers to:
                                // for LID-addressed DMs that means the phone-number
                                // thread, not the bare `@lid` (which silently lands
                                // in a thread the user never sees). Typing reuses
                                // this target, so it follows the reply.
                                let reply_target =
                                    Self::resolve_reply_target(&client, &chat_jid).await;
                                if let Err(e) = tx_inner
                                    .send(ChannelMessage { sender_aliases: Vec::new(),
                                        id: uuid::Uuid::new_v4().to_string(),
                                        channel: "whatsapp".to_string(),
                                        sender: normalized.clone(),
                                        reply_target,
                                        content: trimmed.to_string(),
                                        timestamp: chrono::Utc::now().timestamp() as u64,
                                        thread_ts: None,
                                    })
                                    .await
                                {
                                    tracing::error!("Failed to send message to channel: {}", e);
                                }
                            } else {
                                tracing::warn!("WhatsApp Web: message from {} not in allowed list", normalized);
                            }
                        }
                        Event::Connected(_) => {
                            tracing::info!("WhatsApp Web connected successfully");
                        }
                        Event::LoggedOut(_) => {
                            tracing::warn!("WhatsApp Web was logged out");
                        }
                        Event::StreamError(stream_error) => {
                            tracing::error!("WhatsApp Web stream error: {:?}", stream_error);
                        }
                        Event::PairingCode { code, .. } => {
                            crate::channels::qr_terminal::render_pair_code(&code);
                        }
                        Event::PairingQrCode { code, .. } => {
                            // The wa-rs `Event::PairingQrCode` payload IS the
                            // raw QR text WhatsApp expects you to scan. Render
                            // it as actual block characters so the user can
                            // point a phone at the terminal — printing only
                            // the base64 payload (the previous behaviour) is
                            // useless even at INFO level.
                            crate::channels::qr_terminal::render_qr_with_header(
                                &code,
                                "WhatsApp Web — scan with WhatsApp > Linked Devices > Link a Device",
                            );
                        }
                        _ => {}
                    }
                }
            })
            ;

        // Configure pair-code flow when a phone number is provided.
        if let Some(ref phone) = self.pair_phone {
            tracing::info!("WhatsApp Web: pair-code flow enabled for configured phone number");
            builder = builder.with_pair_code(PairCodeOptions {
                phone_number: phone.clone(),
                custom_code: self.pair_code.clone(),
                ..Default::default()
            });
        } else if self.pair_code.is_some() {
            tracing::warn!(
                "WhatsApp Web: pair_code is set but pair_phone is missing; pair code config is ignored"
            );
        }

        let mut bot = builder.build().await?;
        *self.client.lock() = Some(bot.client());

        // Run the bot
        let bot_handle = bot.run().await?;

        // Store the bot handle for later shutdown
        *self.bot_handle.lock() = Some(bot_handle);

        // Wait for cancellation or shutdown signal
        select! {
            _ = cancel.cancelled() => {
                tracing::info!("WhatsApp Web channel shutting down");
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("WhatsApp Web channel received Ctrl+C");
            }
        }

        *self.client.lock() = None;
        if let Some(handle) = self.bot_handle.lock().take() {
            handle.abort();
        }

        Ok(())
    }

    async fn health_check(&self) -> bool {
        let bot_handle_guard = self.bot_handle.lock();
        bot_handle_guard.is_some()
    }

    async fn start_typing(&self, recipient: &str) -> Result<()> {
        let client = self.client.lock().clone();
        let Some(client) = client else {
            anyhow::bail!("WhatsApp Web client not connected. Initialize the bot first.");
        };

        if !Self::is_jid(recipient) {
            let normalized = self.normalize_phone(recipient);
            if !self.is_number_allowed(&normalized) {
                tracing::warn!(
                    "WhatsApp Web: typing target {} not in allowed list",
                    recipient
                );
                return Ok(());
            }
        }

        let to = self.recipient_to_jid(recipient)?;
        client
            .chatstate()
            .send_composing(&to)
            .await
            .map_err(|e| anyhow!("Failed to send typing state (composing): {e}"))?;

        tracing::debug!("WhatsApp Web: start typing for {}", recipient);
        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> Result<()> {
        let client = self.client.lock().clone();
        let Some(client) = client else {
            anyhow::bail!("WhatsApp Web client not connected. Initialize the bot first.");
        };

        if !Self::is_jid(recipient) {
            let normalized = self.normalize_phone(recipient);
            if !self.is_number_allowed(&normalized) {
                tracing::warn!(
                    "WhatsApp Web: typing target {} not in allowed list",
                    recipient
                );
                return Ok(());
            }
        }

        let to = self.recipient_to_jid(recipient)?;
        client
            .chatstate()
            .send_paused(&to)
            .await
            .map_err(|e| anyhow!("Failed to send typing state (paused): {e}"))?;

        tracing::debug!("WhatsApp Web: stop typing for {}", recipient);
        Ok(())
    }
}

// Stub implementation when feature is not enabled
#[cfg(not(feature = "whatsapp-web"))]
pub struct WhatsAppWebChannel {
    _private: (),
}

#[cfg(not(feature = "whatsapp-web"))]
impl WhatsAppWebChannel {
    pub fn new(
        _session_path: String,
        _pair_phone: Option<String>,
        _pair_code: Option<String>,
        _allowed_numbers: Vec<String>,
    ) -> Self {
        Self { _private: () }
    }
}

#[cfg(not(feature = "whatsapp-web"))]
#[async_trait]
impl Channel for WhatsAppWebChannel {
    fn name(&self) -> &str {
        "whatsapp"
    }

    async fn send(&self, _message: &SendMessage) -> Result<()> {
        anyhow::bail!(
            "WhatsApp Web channel requires the 'whatsapp-web' feature. \
            Enable with: cargo build --features whatsapp-web"
        );
    }

    async fn listen(
        &self,
        _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        anyhow::bail!(
            "WhatsApp Web channel requires the 'whatsapp-web' feature. \
            Enable with: cargo build --features whatsapp-web"
        );
    }

    async fn health_check(&self) -> bool {
        false
    }

    async fn start_typing(&self, _recipient: &str) -> Result<()> {
        anyhow::bail!(
            "WhatsApp Web channel requires the 'whatsapp-web' feature. \
            Enable with: cargo build --features whatsapp-web"
        );
    }

    async fn stop_typing(&self, _recipient: &str) -> Result<()> {
        anyhow::bail!(
            "WhatsApp Web channel requires the 'whatsapp-web' feature. \
            Enable with: cargo build --features whatsapp-web"
        );
    }
}

#[derive(Debug, Clone)]
pub struct PairOptions {
    pub session_path: std::path::PathBuf,
    pub pair_phone: Option<String>,
    pub timeout: std::time::Duration,
}

impl Default for PairOptions {
    fn default() -> Self {
        Self {
            session_path: std::path::PathBuf::from("wa.db"),
            pair_phone: None,
            timeout: std::time::Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone)]
pub enum PairEvent {
    Qr(String),
    PairCode(String),
    Connected,
    Timeout,
    Failed(String),
}

#[cfg(feature = "whatsapp-web")]
pub fn pair_once(opts: PairOptions) -> impl futures::Stream<Item = PairEvent> + Send {
    use async_stream::stream;
    use tokio::sync::mpsc;
    use wa_rs::bot::Bot;
    use wa_rs::pair_code::PairCodeOptions;
    use wa_rs::store::{Device, DeviceStore};
    use wa_rs_core::types::events::Event;
    use wa_rs_tokio_transport::TokioWebSocketTransportFactory;
    use wa_rs_ureq_http::UreqHttpClient;

    let opts = std::sync::Arc::new(opts);
    let (tx, rx) = mpsc::channel::<PairEvent>(32);

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            tracing::info!(
                "pair_once: thread started, opening storage at {}",
                opts.session_path.display()
            );
            let storage = match super::whatsapp_storage::RusqliteStore::new(&opts.session_path) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx
                        .send(PairEvent::Failed(format!("storage init failed: {e}")))
                        .await;
                    return;
                }
            };
            tracing::info!("pair_once: storage opened, building bot");
            let backend = std::sync::Arc::new(storage);
            let mut device = Device::new(backend.clone());
            if let Ok(exists) = backend.exists().await {
                if exists {
                    if let Ok(Some(core_device)) = backend.load().await {
                        device.load_from_serializable(core_device);
                    }
                }
            }
            let mut transport_factory = TokioWebSocketTransportFactory::new();
            if let Ok(ws_url) = std::env::var("WHATSAPP_WS_URL") {
                transport_factory = transport_factory.with_url(ws_url);
            }
            let tx_clone = tx.clone();
            let builder = Bot::builder()
                .with_backend(backend)
                .with_transport_factory(transport_factory)
                .with_http_client(UreqHttpClient::new())
                .with_pair_code(PairCodeOptions {
                    phone_number: opts.pair_phone.clone().unwrap_or_default(),
                    ..Default::default()
                })
                .on_event(move |ev, _client| {
                    let tx = tx_clone.clone();
                    async move {
                        match ev {
                            Event::PairingQrCode { code, .. } => {
                                let _ = tx.send(PairEvent::Qr(code)).await;
                            }
                            Event::PairingCode { code, .. } => {
                                let _ = tx.send(PairEvent::PairCode(code)).await;
                            }
                            Event::Connected(_) => {
                                let _ = tx.send(PairEvent::Connected).await;
                            }
                            Event::LoggedOut(_) => {
                                let _ = tx.send(PairEvent::Failed("logged out".into())).await;
                            }
                            Event::StreamError(e) => {
                                let _ = tx
                                    .send(PairEvent::Failed(format!("stream error: {e:?}")))
                                    .await;
                            }
                            _ => {}
                        }
                    }
                });
            let mut bot = match builder.build().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("pair_once: bot build failed: {e}");
                    let _ = tx
                        .send(PairEvent::Failed(format!("bot build failed: {e}")))
                        .await;
                    return;
                }
            };
            tracing::info!("pair_once: bot built, calling run() to spawn event loop");
            // wa-rs `Bot::run()` SPAWNS the event loop on a background
            // tokio task and returns the JoinHandle immediately. We must
            // await the handle to keep the runtime alive while the loop
            // runs — discarding it lets the runtime drop, which kills the
            // task before it ever connects (symptom: user sees "Starting
            // WhatsApp Web pairing…" forever, no QR).
            let join_handle = match bot.run().await {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!("pair_once: bot.run() failed to spawn: {e}");
                    let _ = tx
                        .send(PairEvent::Failed(format!("bot run failed: {e}")))
                        .await;
                    return;
                }
            };
            tracing::info!("pair_once: event loop spawned, awaiting JoinHandle");
            if let Err(e) = join_handle.await {
                tracing::error!("pair_once: bot task join failed: {e}");
                let _ = tx
                    .send(PairEvent::Failed(format!("bot task panicked: {e}")))
                    .await;
            }
            tracing::info!("pair_once: thread exiting");
        });
    });

    Box::pin(stream! {
        let mut rx = rx;
        while let Some(ev) = rx.recv().await {
            yield ev;
        }
        yield PairEvent::Failed("channel closed".into());
    })
}

#[cfg(all(test, feature = "whatsapp-web"))]
mod tests {
    use super::*;

    fn make_channel(allowed: Vec<String>) -> WhatsAppWebChannel {
        WhatsAppWebChannel::new("/tmp/wa-test.db".into(), None, None, allowed)
    }

    #[test]
    fn normalize_phone_strips_jid_and_adds_plus() {
        let ch = make_channel(vec![]);
        assert_eq!(ch.normalize_phone("1234567890"), "+1234567890");
        assert_eq!(ch.normalize_phone("+1234567890"), "+1234567890");
        // JID form: strip the domain suffix, then prefix +.
        assert_eq!(
            ch.normalize_phone("1234567890@s.whatsapp.net"),
            "+1234567890"
        );
    }

    #[test]
    fn is_number_allowed_reads_through_lock() {
        let ch = make_channel(vec!["+1234567890".into()]);
        assert!(ch.is_number_allowed("+1234567890"));
        assert!(!ch.is_number_allowed("+9999999999"));
    }

    #[test]
    fn normalize_sender_uses_resolved_phone_number() {
        // A LID sender resolved to its phone number matches owner/allowlist on
        // the real number, not the opaque LID.
        assert_eq!(
            WhatsAppWebChannel::normalize_sender(Some("6285228485826"), "207550217756908"),
            "+6285228485826"
        );
    }

    #[test]
    fn normalize_sender_falls_back_to_raw_user() {
        assert_eq!(
            WhatsAppWebChannel::normalize_sender(None, "1234567890"),
            "+1234567890"
        );
    }

    #[test]
    fn normalize_sender_keeps_existing_plus() {
        assert_eq!(
            WhatsAppWebChannel::normalize_sender(None, "+1234567890"),
            "+1234567890"
        );
    }

    #[test]
    fn add_allowed_number_in_appends_and_dedupes() {
        let allowed: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(vec!["+1234567890".into()]));
        WhatsAppWebChannel::add_allowed_number_in(&allowed, "+9999999999");
        assert!(WhatsAppWebChannel::number_allowed_in(
            &allowed,
            "+9999999999"
        ));
        WhatsAppWebChannel::add_allowed_number_in(&allowed, "+9999999999");
        assert_eq!(allowed.read().unwrap().len(), 2);
        // Blank input is ignored.
        WhatsAppWebChannel::add_allowed_number_in(&allowed, "   ");
        assert_eq!(allowed.read().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn handle_pairing_for_non_command_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let allowed: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(vec![]));
        let reply = WhatsAppWebChannel::handle_pairing_for(
            &allowed,
            "hello agent",
            "+9999999999",
            dir.path(),
        )
        .await;
        assert!(reply.is_none());
    }

    #[tokio::test]
    async fn handle_pairing_for_falls_through_when_no_store_code() {
        // A `/bind` with no live store code returns None (not owned).
        let dir = tempfile::TempDir::new().unwrap();
        let allowed: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(vec![]));
        let reply = WhatsAppWebChannel::handle_pairing_for(
            &allowed,
            "/bind ABCD-EFGH",
            "+9999999999",
            dir.path(),
        )
        .await;
        assert!(reply.is_none());
    }

    /// A store-minted "whatsapp" code is accepted on `/claim` via the extracted
    /// helper: the shared core lands the sender in `allowed_numbers` AND
    /// `approval_owners`, and `handle_pairing_for` extends the runtime allowlist.
    #[tokio::test]
    async fn store_minted_whatsapp_code_claims_owner_and_extends_runtime() {
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
                session_path: Some("/tmp/wa.db".into()),
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

        let allowed: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(vec![]));
        let reply = WhatsAppWebChannel::handle_pairing_for(
            &allowed,
            &format!("/claim {code}"),
            "+9999999999",
            root,
        )
        .await
        .expect("a /claim must be handled");
        assert!(reply.contains("owner"), "reply was: {reply}");

        // Runtime allowlist extended immediately.
        assert!(WhatsAppWebChannel::number_allowed_in(
            &allowed,
            "+9999999999"
        ));

        // Config persisted.
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
}
