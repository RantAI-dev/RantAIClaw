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
        let Some(root) = crate::channels::pairing::profile_root("whatsapp_web") else {
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

    /// Whether an outbound recipient is permitted by the allowlist.
    ///
    /// The gate used to run only when the recipient was NOT a JID — and
    /// `resolve_reply_target` always produces a JID, which comes back as
    /// `SendMessage.recipient`, so every agent-driven reply took the bypass and
    /// the allowlist provided zero outbound containment.
    ///
    /// Groups (`@g.us`) and broadcast lists are a **documented exemption**: the
    /// allowlist holds phone numbers, a group JID is not one, and gating on it
    /// would break every group reply. Containment for groups is the inbound
    /// gate — the agent only replies where it was addressed.
    #[cfg(feature = "whatsapp-web")]
    fn allow_recipient(recipient: &str, allowed: &Arc<RwLock<Vec<String>>>) -> RecipientDecision {
        let trimmed = recipient.trim();
        if trimmed.is_empty() {
            return RecipientDecision::Deny("recipient is empty".to_string());
        }

        let (user, server) = match trimmed.split_once('@') {
            Some((user, server)) => (user, server),
            // Bare number: normalise and gate.
            None => {
                let normalized = Self::normalize_e164(trimmed);
                return if Self::number_allowed_in(allowed, &normalized) {
                    RecipientDecision::Allow
                } else {
                    RecipientDecision::Deny(format!("{normalized} is not in allowed_numbers"))
                };
            }
        };

        match server {
            // Groups and broadcasts: exempt, deliberately. See above.
            "g.us" | "broadcast" | "status" => RecipientDecision::Allow,
            // A LID is not a phone number, so it can only match an explicit
            // `lid:` entry or the wildcard — never a numeric entry.
            "lid" => {
                let entry = format!("lid:{user}");
                if Self::number_allowed_in(allowed, &entry) {
                    RecipientDecision::Allow
                } else {
                    RecipientDecision::Deny(format!(
                        "{entry} is not in allowed_numbers (an unmapped LID is not a phone number)"
                    ))
                }
            }
            // Everything else is a user JID whose user part is the number.
            _ => {
                let normalized = Self::normalize_e164(user);
                if Self::number_allowed_in(allowed, &normalized) {
                    RecipientDecision::Allow
                } else {
                    RecipientDecision::Deny(format!("{normalized} is not in allowed_numbers"))
                }
            }
        }
    }

    /// `+`-prefixed form of a bare user part.
    #[cfg(feature = "whatsapp-web")]
    fn normalize_e164(user: &str) -> String {
        let user = user.trim();
        if user.starts_with('+') {
            user.to_string()
        } else {
            format!("+{user}")
        }
    }

    /// Classify a terminal wa-rs event by name.
    ///
    /// Keyed on the variant name rather than the type so this stays testable
    /// without constructing wa-rs payloads, and so a variant added upstream
    /// falls into the explicit unknown arm instead of being silently ignored.
    #[cfg(feature = "whatsapp-web")]
    fn classify_terminal_event(variant: &str) -> TerminalAction {
        match variant {
            // Re-pairing required: restarting cannot fix either of these, and
            // restarting into a ban makes it worse.
            "LoggedOut" => TerminalAction::Stop("the device was logged out; re-pair to continue"),
            "TemporaryBan" => {
                TerminalAction::Stop("the account is temporarily banned; do not reconnect")
            }
            // Recoverable by a fresh connection.
            "StreamError" | "StreamReplaced" | "Disconnected" | "ConnectFailure"
            | "ClientOutdated" | "PairError" => TerminalAction::Restart("the stream ended"),
            _ => TerminalAction::Continue,
        }
    }

    /// The `ChannelMessage.id` for an inbound message.
    ///
    /// A UUID minted per message made a redelivery undetectable, so the agent
    /// ran again on a message it had already answered.
    #[cfg(feature = "whatsapp-web")]
    fn inbound_message_id(platform_id: &str) -> String {
        let trimmed = platform_id.trim();
        if trimmed.is_empty() {
            return uuid::Uuid::new_v4().to_string();
        }
        format!("whatsapp_{trimmed}")
    }

    /// The message's own timestamp as unix seconds, checked.
    ///
    /// `Utc::now()` stamped the moment the loop happened to process it, and an
    /// `as u64` cast turns a negative (pre-epoch, or a malformed payload) into
    /// an enormous positive.
    #[cfg(feature = "whatsapp-web")]
    fn inbound_timestamp(message_ts: i64) -> u64 {
        u64::try_from(message_ts).unwrap_or_else(|_| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        })
    }

    /// Whether an inbound sender may reach the agent.
    ///
    /// An unmapped LID used to be admitted whenever the allowlist was
    /// *non-empty* — `!a.is_empty()` subsumes the wildcard test, so configuring
    /// any entry at all admitted every unmapped-LID sender.
    #[cfg(feature = "whatsapp-web")]
    fn allow_inbound(
        allowed: &Arc<RwLock<Vec<String>>>,
        is_lid: bool,
        resolved_pn: Option<&str>,
        sender_user: &str,
    ) -> bool {
        if is_lid && resolved_pn.is_none() {
            // Only an explicit wildcard or an explicit `lid:` entry.
            return Self::number_allowed_in(allowed, &format!("lid:{sender_user}"));
        }
        Self::number_allowed_in(allowed, &Self::normalize_sender(resolved_pn, sender_user))
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

    /// The identity to report for an inbound sender.
    ///
    /// An unmapped LID is NOT a phone number, and reporting it as `+digits`
    /// made it indistinguishable from one in logs and in `approval_owners`. It
    /// now carries a `lid:` prefix so the two can never be confused.
    #[cfg(feature = "whatsapp-web")]
    fn inbound_identity(is_lid: bool, resolved_pn: Option<&str>, sender_user: &str) -> String {
        if is_lid && resolved_pn.is_none() {
            return format!("lid:{sender_user}");
        }
        Self::normalize_sender(resolved_pn, sender_user)
    }
}

/// What the event loop should do about a terminal wa-rs event.
///
/// The match used to end in `_ => {}`, which swallowed every terminal variant:
/// `Disconnected`, `ConnectFailure`, `StreamReplaced`, `TemporaryBan`,
/// `ClientOutdated`, `PairError` and `UndecryptableMessage` all read as
/// "nothing happened", so a dead channel kept reporting healthy.
#[cfg(feature = "whatsapp-web")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalAction {
    /// Keep running; nothing is wrong.
    Continue,
    /// The session is gone. Mark unhealthy and let the supervisor restart.
    Restart(&'static str),
    /// Re-pairing is required; a restart cannot fix it.
    Stop(&'static str),
}

/// Outcome of the outbound allowlist gate.
#[cfg(feature = "whatsapp-web")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum RecipientDecision {
    Allow,
    /// Carries the reason, so the refusal the agent sees names the number.
    Deny(String),
}

#[cfg(feature = "whatsapp-web")]
#[async_trait]
impl Channel for WhatsAppWebChannel {
    fn name(&self) -> &str {
        "whatsapp"
    }

    fn render_target(&self) -> crate::channels::format::RenderTarget {
        // Same WhatsApp app as the Cloud channel: single-char markup, no
        // CommonMark. LightMarkup{Raw} converts `**bold**`→`*bold*` etc. and
        // renders links as `text (url)` without entity escaping.
        crate::channels::format::RenderTarget::LightMarkup {
            links: crate::channels::format::LinkStyle::Raw,
        }
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        let client = self.client.lock().clone();
        let Some(client) = client else {
            anyhow::bail!("WhatsApp Web client not connected. Initialize the bot first.");
        };

        // Gate EVERY recipient form. This used to run only for non-JID
        // recipients, and `resolve_reply_target` always yields a JID — so every
        // agent-driven reply bypassed the allowlist entirely.
        if let RecipientDecision::Deny(reason) =
            Self::allow_recipient(&message.recipient, &self.allowed_numbers)
        {
            // Was `return Ok(())`: the agent recorded a delivered reply that
            // was never transmitted.
            anyhow::bail!(
                "WhatsApp Web refused to send to {}: {reason}",
                message.recipient
            );
        }

        let to = self.recipient_to_jid(&message.recipient)?;
        // `rendered`, not `outgoing`: `outgoing` is the wa-rs Message struct.
        let rendered =
            crate::channels::format::render_to_string(&message.content, &self.render_target());
        let outgoing = wa_rs_proto::whatsapp::Message {
            conversation: Some(rendered),
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
        // Re-entry guard: every restart used to build a new client while the
        // old one was still running, leaving N live clients, N sync workers and
        // N device-savers writing to one SQLite session file.
        // The guard is dropped before the await below — holding a parking_lot
        // lock across an await point is not allowed here.
        let previous = self.bot_handle.lock().take();
        if let Some(previous) = previous {
            tracing::warn!("WhatsApp Web: a previous listener is still live; aborting it first");
            previous.abort();
            // Awaiting the abort is the point: without it the old socket is
            // still draining when the new one dials.
            let _ = previous.await;
        }
        *self.client.lock() = None;

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

        // A terminal event inside the wa-rs event loop has to reach `listen()`,
        // which is parked in the `select!` below. The token wakes it; the slot
        // carries why.
        let session_ended = tokio_util::sync::CancellationToken::new();
        let session_end_reason: Arc<Mutex<Option<TerminalAction>>> = Arc::new(Mutex::new(None));
        // The closure below takes ownership of its clones; these two stay here
        // for the `select!` and the return path.
        let session_ended_outer = session_ended.clone();
        let session_end_outer = Arc::clone(&session_end_reason);

        let mut builder = Bot::builder()
            .with_backend(backend)
            .with_transport_factory(transport_factory)
            .with_http_client(http_client)
            .on_event(move |event, client| {
                let tx_inner = tx_clone.clone();
                let allowed_numbers = allowed_numbers.clone();
                let session_ended_inner = session_ended.clone();
                let session_end_inner = Arc::clone(&session_end_reason);
                async move {
                    match event {
                        Event::Message(msg, info) => {
                            // Extract message content
                            let text = msg.text_content().unwrap_or("");
                            let sender = info.source.sender.user().to_string();
                            let sender_jid = info.source.sender.to_string();
                            let chat_jid = info.source.chat.clone();
                            let chat = chat_jid.to_string();

                            // Message bodies are NOT logged. They used to be
                            // logged at INFO — including `/claim` pairing codes,
                            // which promote their holder to owner, and which were
                            // logged BEFORE the pairing handler ran.
                            tracing::debug!(
                                "WhatsApp Web message from {} in {} ({} chars)",
                                sender,
                                chat,
                                text.chars().count()
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

                            // Sender identity: a resolved phone number in E.164
                            // form, or a `lid:`-prefixed LID that can never be
                            // mistaken for one.
                            let normalized =
                                Self::inbound_identity(is_lid, resolved_pn.as_deref(), &sender);

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

                            // An unmapped LID is unverifiable, so it needs an
                            // explicit `"*"` or an explicit `lid:` entry. It used
                            // to be admitted whenever the allowlist was merely
                            // NON-EMPTY, which let any configured list admit every
                            // unmapped-LID sender.
                            let is_allowed = Self::allow_inbound(
                                &allowed_numbers,
                                is_lid,
                                resolved_pn.as_deref(),
                                &sender,
                            );

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
                                let inbound = ChannelMessage {
                                    sender_aliases: Vec::new(),
                                    // The platform id, not a fresh UUID: a
                                    // redelivery has to be recognisable.
                                    id: Self::inbound_message_id(&info.id),
                                    channel: "whatsapp".to_string(),
                                    sender: normalized.clone(),
                                    reply_target,
                                    content: trimmed.to_string(),
                                    // The message's own timestamp, checked —
                                    // `Utc::now()` stamped the moment we
                                    // happened to process it.
                                    timestamp: Self::inbound_timestamp(
                                        info.timestamp.timestamp(),
                                    ),
                                    thread_ts: None,
                                };
                                // `try_send`, not `send`: a busy agent must not
                                // park the wa-rs protocol loop, which also
                                // carries acks and retries.
                                if let Err(e) = tx_inner.try_send(inbound) {
                                    tracing::warn!(
                                        "WhatsApp Web: dropping an inbound message, the agent \
                                         queue is not accepting it: {e}"
                                    );
                                }
                            } else {
                                // Name the identity so an operator can allowlist
                                // it — including the `lid:` form, which is the
                                // only thing that admits an unmapped LID.
                                tracing::warn!(
                                    "WhatsApp Web: message from {normalized} not in allowed_numbers; \
                                     add that exact value to allow it"
                                );
                            }
                        }
                        Event::Connected(_) => {
                            tracing::info!("WhatsApp Web connected successfully");
                        }
                        Event::LoggedOut(_) => {
                            tracing::warn!("WhatsApp Web was logged out");
                            *session_end_inner.lock() =
                                Some(Self::classify_terminal_event("LoggedOut"));
                            session_ended_inner.cancel();
                        }
                        Event::StreamError(stream_error) => {
                            tracing::error!("WhatsApp Web stream error: {:?}", stream_error);
                            *session_end_inner.lock() =
                                Some(Self::classify_terminal_event("StreamError"));
                            session_ended_inner.cancel();
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
                        // Every other variant used to land in `_ => {}`, which
                        // swallowed Disconnected, ConnectFailure, StreamReplaced,
                        // TemporaryBan, ClientOutdated, PairError and
                        // UndecryptableMessage alike — a dead channel kept
                        // reporting healthy. Classify by variant name so an
                        // upstream addition surfaces instead of vanishing.
                        other => {
                            let variant = format!("{other:?}");
                            let name = variant
                                .split(['(', ' ', '{'])
                                .next()
                                .unwrap_or("")
                                .to_string();
                            match Self::classify_terminal_event(&name) {
                                TerminalAction::Continue => {
                                    tracing::debug!("WhatsApp Web: unhandled event {name}");
                                }
                                action => {
                                    tracing::warn!("WhatsApp Web: terminal event {name}");
                                    *session_end_inner.lock() = Some(action);
                                    session_ended_inner.cancel();
                                }
                            }
                        }
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

        // Wait for cancellation or a terminal event.
        //
        // The `tokio::signal::ctrl_c()` arm that used to sit here returned
        // `Ok(())` independently of the app's shutdown token, which the
        // supervisor read as an unexpected exit and restarted — the passed
        // token already covers shutdown.
        select! {
            () = cancel.cancelled() => {
                tracing::info!("WhatsApp Web channel shutting down");
            }
            () = session_ended_outer.cancelled() => {
                tracing::warn!("WhatsApp Web session ended");
            }
        }

        // Clear both before returning, so `health_check` can report false and a
        // restart does not find a stale client.
        *self.client.lock() = None;
        let handle = self.bot_handle.lock().take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }

        // `Err` on a fault, per the trait contract: the supervisor escalates its
        // backoff instead of reconnecting at a fixed rate. Note the limit — the
        // supervisor restarts on `Stop` too; it backs off further, which is the
        // most this side can do about a ban without a supervisor change.
        let reason = session_end_outer.lock().take();
        match reason {
            Some(TerminalAction::Stop(why)) => {
                anyhow::bail!("WhatsApp Web stopped: {why}");
            }
            Some(TerminalAction::Restart(why)) => {
                anyhow::bail!("WhatsApp Web session fault: {why}");
            }
            _ => Ok(()),
        }
    }

    fn apply_allowed_senders(&self, allowed: &[String]) {
        if let Ok(mut numbers) = self.allowed_numbers.write() {
            *numbers = allowed.to_vec();
        }
    }

    /// Healthy means a live client, not merely a handle that was once set.
    ///
    /// The handle used to be left in place on `LoggedOut` and `StreamError`, so
    /// a dead channel reported healthy for the rest of the process's life.
    ///
    /// This probe reads **local state only** — no network round trip, unlike the
    /// sixteen channels that call their platform. It fails for the condition it
    /// exists to catch (the event loop clears `client` on every terminal event),
    /// but it cannot notice a platform that stops answering while the client
    /// object is still around. The supervisor now runs this on its heartbeat.
    async fn health_check(&self) -> bool {
        self.client.lock().is_some() && self.bot_handle.lock().is_some()
    }

    async fn start_typing(&self, recipient: &str) -> Result<()> {
        let client = self.client.lock().clone();
        let Some(client) = client else {
            anyhow::bail!("WhatsApp Web client not connected. Initialize the bot first.");
        };

        if let RecipientDecision::Deny(reason) =
            Self::allow_recipient(recipient, &self.allowed_numbers)
        {
            // Cosmetic surface: refusing quietly is right, `send` reports the
            // same target with a full error.
            tracing::debug!("WhatsApp Web: not typing at {recipient}: {reason}");
            return Ok(());
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

        if let RecipientDecision::Deny(reason) =
            Self::allow_recipient(recipient, &self.allowed_numbers)
        {
            // Cosmetic surface: refusing quietly is right, `send` reports the
            // same target with a full error.
            tracing::debug!("WhatsApp Web: not typing at {recipient}: {reason}");
            return Ok(());
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

impl PairOptions {
    /// Build options for a session file the caller has already resolved.
    ///
    /// There is deliberately no `Default`: it used to default `session_path` to
    /// the relative `wa.db`, so key material landed wherever the process
    /// happened to be running.
    #[must_use]
    pub fn new(session_path: std::path::PathBuf) -> Self {
        Self {
            session_path,
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
        // A panicking runtime start here dropped `tx`, so the operator saw
        // "Pairing failed: channel closed" with the real cause nowhere.
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.blocking_send(PairEvent::Failed(format!(
                    "could not start the pairing runtime: {e}"
                )));
                return;
            }
        };
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
            // Both results are matched. They used to be ignored, so a corrupt
            // or unreadable session DB looked identical to "no session" and the
            // wizard paired a fresh device OVER existing key material.
            match backend.exists().await {
                Ok(true) => match backend.load().await {
                    Ok(Some(core_device)) => device.load_from_serializable(core_device),
                    Ok(None) => {
                        let _ = tx
                            .send(PairEvent::Failed(
                                "the session database reports a device but could not load it; \
                                 refusing to pair over existing key material"
                                    .into(),
                            ))
                            .await;
                        return;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(PairEvent::Failed(format!(
                                "existing session could not be read ({e}); refusing to pair over \
                                 it — move or delete the session file to start fresh"
                            )))
                            .await;
                        return;
                    }
                },
                Ok(false) => {}
                Err(e) => {
                    let _ = tx
                        .send(PairEvent::Failed(format!(
                            "could not check for an existing session: {e}"
                        )))
                        .await;
                    return;
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
            // Bounded by `opts.timeout`, which the struct has always declared
            // and nothing ever read — `PairEvent::Timeout` had exactly one
            // occurrence in the repo, the arm that handles it. The bot
            // auto-reconnects, so an unbounded await never returns.
            match tokio::time::timeout(opts.timeout, join_handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::error!("pair_once: bot task join failed: {e}");
                    let _ = tx
                        .send(PairEvent::Failed(format!("bot task panicked: {e}")))
                        .await;
                }
                Err(_elapsed) => {
                    tracing::warn!("pair_once: timed out after {:?}", opts.timeout);
                    let _ = tx.send(PairEvent::Timeout).await;
                }
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

    fn allowlist(entries: &[&str]) -> Arc<RwLock<Vec<String>>> {
        Arc::new(RwLock::new(
            entries.iter().map(|e| (*e).to_string()).collect(),
        ))
    }

    /// The outbound gate used to run only when the recipient was NOT a JID —
    /// and `resolve_reply_target` always produces a JID, which comes back as
    /// `SendMessage.recipient`. So every agent-driven reply bypassed it.
    #[test]
    fn allow_recipient_applies_to_jid_form() {
        let empty = allowlist(&[]);
        let specific = allowlist(&["+15551234567"]);
        let wildcard = allowlist(&["*"]);

        for form in [
            "+15551234567",
            "15551234567@s.whatsapp.net",
            "+15551234567@s.whatsapp.net",
        ] {
            assert_eq!(
                WhatsAppWebChannel::allow_recipient(form, &empty),
                RecipientDecision::Deny("+15551234567 is not in allowed_numbers".to_string()),
                "empty allowlist must deny {form}"
            );
            assert_eq!(
                WhatsAppWebChannel::allow_recipient(form, &specific),
                RecipientDecision::Allow,
                "a listed number must be allowed via {form}"
            );
            assert_eq!(
                WhatsAppWebChannel::allow_recipient(form, &wildcard),
                RecipientDecision::Allow
            );
        }

        // A different number is denied in every form.
        assert!(matches!(
            WhatsAppWebChannel::allow_recipient("19998887777@s.whatsapp.net", &specific),
            RecipientDecision::Deny(_)
        ));

        // Groups and broadcasts are a documented exemption.
        for group in ["1234-5678@g.us", "status@broadcast"] {
            assert_eq!(
                WhatsAppWebChannel::allow_recipient(group, &empty),
                RecipientDecision::Allow,
                "{group} must stay exempt or every group reply breaks"
            );
        }

        assert!(matches!(
            WhatsAppWebChannel::allow_recipient("   ", &wildcard),
            RecipientDecision::Deny(_)
        ));
    }

    /// A LID is not a phone number: it must match `lid:<id>` or the wildcard,
    /// never a numeric entry.
    #[test]
    fn allow_recipient_treats_a_lid_as_its_own_form() {
        let numeric = allowlist(&["+15551234567"]);
        assert!(matches!(
            WhatsAppWebChannel::allow_recipient("15551234567@lid", &numeric),
            RecipientDecision::Deny(_)
        ));
        assert_eq!(
            WhatsAppWebChannel::allow_recipient(
                "15551234567@lid",
                &allowlist(&["lid:15551234567"])
            ),
            RecipientDecision::Allow
        );
        assert_eq!(
            WhatsAppWebChannel::allow_recipient("15551234567@lid", &allowlist(&["*"])),
            RecipientDecision::Allow
        );
    }

    /// `!a.is_empty()` subsumed the wildcard test, so configuring ANY entry
    /// admitted every unmapped-LID sender.
    #[test]
    fn unmapped_lid_is_rejected_when_the_allowlist_is_non_empty() {
        let configured = allowlist(&["+15551234567"]);
        assert!(
            !WhatsAppWebChannel::allow_inbound(&configured, true, None, "99887766"),
            "a non-empty allowlist must not admit an unmapped LID"
        );
        assert!(
            WhatsAppWebChannel::allow_inbound(&allowlist(&["*"]), true, None, "99887766"),
            "an explicit wildcard still admits it"
        );
        assert!(
            WhatsAppWebChannel::allow_inbound(
                &allowlist(&["lid:99887766"]),
                true,
                None,
                "99887766"
            ),
            "an explicit lid entry admits it"
        );
        // A LID that resolved to a phone number is matched as that number.
        assert!(WhatsAppWebChannel::allow_inbound(
            &configured,
            true,
            Some("15551234567"),
            "99887766"
        ));
    }

    /// An unmapped LID reported as `+digits` was indistinguishable from a phone
    /// number in logs and in `approval_owners`.
    #[test]
    fn an_unmapped_lid_is_visibly_not_a_phone_number() {
        assert_eq!(
            WhatsAppWebChannel::inbound_identity(true, None, "99887766"),
            "lid:99887766"
        );
        assert_eq!(
            WhatsAppWebChannel::inbound_identity(true, Some("15551234567"), "99887766"),
            "+15551234567"
        );
        assert_eq!(
            WhatsAppWebChannel::inbound_identity(false, None, "15551234567"),
            "+15551234567"
        );
    }

    #[test]
    fn classify_marks_terminal_events() {
        use TerminalAction::{Continue, Restart, Stop};
        assert!(matches!(
            WhatsAppWebChannel::classify_terminal_event("LoggedOut"),
            Stop(_)
        ));
        assert!(matches!(
            WhatsAppWebChannel::classify_terminal_event("TemporaryBan"),
            Stop(_)
        ));
        for recoverable in [
            "StreamError",
            "StreamReplaced",
            "Disconnected",
            "ConnectFailure",
            "ClientOutdated",
            "PairError",
        ] {
            assert!(
                matches!(
                    WhatsAppWebChannel::classify_terminal_event(recoverable),
                    Restart(_)
                ),
                "{recoverable} must end the session"
            );
        }
        assert_eq!(
            WhatsAppWebChannel::classify_terminal_event("Receipt"),
            Continue
        );
    }

    #[test]
    fn map_inbound_carries_the_platform_id_and_timestamp() {
        assert_eq!(
            WhatsAppWebChannel::inbound_message_id("3EB0ABC123"),
            "whatsapp_3EB0ABC123"
        );
        // Absent id falls back to a UUID rather than an empty string.
        assert_ne!(WhatsAppWebChannel::inbound_message_id("  "), "whatsapp_");

        assert_eq!(
            WhatsAppWebChannel::inbound_timestamp(1_700_000_000),
            1_700_000_000
        );
        // A negative timestamp used to become an enormous positive via `as u64`.
        let fallback = WhatsAppWebChannel::inbound_timestamp(-1);
        assert!(fallback < 100_000_000_000, "got {fallback}");
    }

    #[test]
    fn allowlist_edit_reaches_the_channel() {
        let ch = make_channel(vec!["*".to_string()]);
        assert!(ch.is_number_allowed("+15551234567"));
        ch.apply_allowed_senders(&["+19998887777".to_string()]);
        assert!(ch.is_number_allowed("+19998887777"));
        assert!(!ch.is_number_allowed("+15551234567"));
    }

    /// `PairOptions` used to default `session_path` to a relative `wa.db`, so
    /// the account's key material landed wherever the process ran.
    #[test]
    fn pair_options_have_no_relative_default() {
        let opts = PairOptions::new(std::path::PathBuf::from("/tmp/rantaiclaw/wa.db"));
        assert!(opts.session_path.is_absolute());
        assert!(opts.timeout > std::time::Duration::ZERO);

        let src = include_str!("whatsapp_web.rs");
        let production = src.split("#[cfg(all(test").next().expect("source");
        assert!(
            !production.contains("impl Default for PairOptions"),
            "a Default impl reintroduces the relative session path"
        );
    }

    /// `opts.timeout` was declared and never read, so pairing never ended: the
    /// bot auto-reconnects, and the awaited handle only resolves when the event
    /// loop dies.
    #[test]
    fn pair_once_honours_its_timeout() {
        let src = include_str!("whatsapp_web.rs");
        let production = src.split("#[cfg(all(test").next().expect("source");
        let body = production
            .split("pub fn pair_once(")
            .nth(1)
            .expect("pair_once exists");
        assert!(
            body.contains("tokio::time::timeout(opts.timeout"),
            "the pairing wait must be bounded by the declared timeout"
        );
        assert!(
            body.contains("PairEvent::Timeout"),
            "the timeout must be reported to the caller"
        );
        assert!(
            !body.contains("expect(\"runtime\")"),
            "a panicking runtime start drops the sender and hides the cause"
        );
    }

    /// A corrupt or unreadable session DB used to look identical to "no
    /// session", so the wizard paired a fresh device over existing key
    /// material.
    #[test]
    fn pair_once_refuses_to_pair_over_an_unreadable_session() {
        let src = include_str!("whatsapp_web.rs");
        let production = src.split("#[cfg(all(test").next().expect("source");
        let body = production
            .split("pub fn pair_once(")
            .nth(1)
            .expect("pair_once exists");
        assert!(
            !body.contains("if let Ok(exists) = backend.exists()"),
            "both session-load results must be matched, not silently ignored"
        );
        assert!(
            body.contains("refusing to pair over"),
            "the refusal must say why"
        );
    }

    /// The table test above exercises `allow_recipient` directly, and a
    /// `send()` that re-adds the old JID bypass passes it anyway — that bypass
    /// IS the defect. `send()` needs a live client to drive, so the wiring is
    /// asserted by source.
    ///
    /// `is_jid` itself was deleted as dead code; the string check below stays as
    /// a guard against the pattern being reintroduced under the same name.
    #[test]
    fn send_gates_every_recipient_form() {
        let src = include_str!("whatsapp_web.rs");
        let production = src.split("#[cfg(all(test").next().expect("source");
        let send_body = production
            .split("async fn send(&self, message: &SendMessage)")
            .nth(1)
            .expect("send exists");
        let next_fn = send_body.find("\n    async fn ").unwrap_or(send_body.len());
        let send_body = &send_body[..next_fn];
        assert!(
            send_body.contains("Self::allow_recipient("),
            "send() must run the allowlist gate"
        );
        assert!(
            !send_body.contains("is_jid("),
            "the gate must not be conditioned on the recipient being a bare number"
        );
        assert!(
            send_body.contains("anyhow::bail!"),
            "a blocked send must be an error, not a silent Ok(())"
        );
    }

    /// Message bodies must never reach INFO — they used to, including `/claim`
    /// pairing codes, and BEFORE the pairing handler ran.
    #[test]
    fn no_message_body_is_logged_at_info() {
        let src = include_str!("whatsapp_web.rs");
        let production = src.split("#[cfg(all(test").next().expect("source");
        assert!(
            !production.contains("WhatsApp Web message from {} in {}: {}"),
            "the body-logging INFO line is back"
        );
        let handler = production
            .split("Event::Message(msg, info)")
            .nth(1)
            .expect("the message arm exists");
        let pairing_at = handler
            .find("try_reply_pairing")
            .expect("the pairing interception is in the message arm");
        let log_at = handler
            .find("tracing::debug!")
            .expect("the message arm logs at debug");
        assert!(
            log_at < pairing_at,
            "the surviving log line must not carry the body; it logs only a length"
        );
        let logged = &handler[log_at..pairing_at];
        assert!(
            logged.contains("chars()"),
            "the log must carry a length, not the text: {logged}"
        );
        assert!(
            !logged.contains(", text\n") && !logged.contains("text\n"),
            "the body must not be an argument: {logged}"
        );
    }

    fn make_channel(allowed: Vec<String>) -> WhatsAppWebChannel {
        WhatsAppWebChannel::new("/tmp/wa-test.db".into(), None, None, allowed)
    }

    #[test]
    fn whatsapp_web_render_target_is_lightmarkup_raw() {
        assert_eq!(
            make_channel(vec![]).render_target(),
            crate::channels::format::RenderTarget::LightMarkup {
                links: crate::channels::format::LinkStyle::Raw
            }
        );
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
            WhatsAppWebChannel::normalize_sender(Some("628123456789"), "200000000000001"),
            "+628123456789"
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
