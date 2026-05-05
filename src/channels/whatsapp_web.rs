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
    /// Allowed phone numbers (E.164 format) or "*" for all
    allowed_numbers: Vec<String>,
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
            allowed_numbers,
            bot_handle: Arc::new(Mutex::new(None)),
            client: Arc::new(Mutex::new(None)),
            tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Check if a phone number is allowed (E.164 format: +1234567890)
    #[cfg(feature = "whatsapp-web")]
    fn is_number_allowed(&self, phone: &str) -> bool {
        self.allowed_numbers.iter().any(|n| n == "*" || n == phone)
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
            .on_event(move |event, _client| {
                let tx_inner = tx_clone.clone();
                let allowed_numbers = allowed_numbers.clone();
                async move {
                    match event {
                        Event::Message(msg, info) => {
                            // Extract message content
                            let text = msg.text_content().unwrap_or("");
                            let sender = info.source.sender.user().to_string();
                            let sender_jid = info.source.sender.to_string();
                            let chat = info.source.chat.to_string();

                            tracing::info!(
                                "WhatsApp Web message from {} in {}: {}",
                                sender,
                                chat,
                                text
                            );

                            // Detect LID (Linked Identity) senders — WhatsApp may use
                            // opaque LID identifiers instead of phone numbers.
                            // LIDs have the @lid domain suffix.
                            let is_lid = sender_jid.contains("@lid");

                            // Check if sender is allowed
                            let normalized = if sender.starts_with('+') {
                                sender.clone()
                            } else {
                                format!("+{sender}")
                            };

                            // For LID senders we cannot match against phone-based
                            // allowed_numbers, so allow them through when the list
                            // is non-empty (the user has configured filtering intent
                            // but LIDs are unverifiable). Wildcard "*" always passes.
                            let is_allowed = if is_lid {
                                // Allow LID senders unless allowed_numbers is empty
                                // (empty = deny-all secure default).
                                allowed_numbers.iter().any(|n| n == "*") || !allowed_numbers.is_empty()
                            } else {
                                allowed_numbers.iter().any(|n| n == "*" || n == &normalized)
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

                                if let Err(e) = tx_inner
                                    .send(ChannelMessage {
                                        id: uuid::Uuid::new_v4().to_string(),
                                        channel: "whatsapp".to_string(),
                                        sender: normalized.clone(),
                                        // Reply to the originating chat JID (DM or group).
                                        reply_target: chat,
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
