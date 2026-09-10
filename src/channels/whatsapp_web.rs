//! WhatsApp Web channel using wa-rs (native Rust implementation)
//!
//! This channel provides direct WhatsApp Web integration with:
//! - QR code and pair code linking
//! - End-to-end encryption via Signal Protocol
//! - Groups, presence, reactions, editing and deletion
//! - Inbound images, downloaded through the shared `media` budget gate
//!
//! The `wa-rs` library it wraps advertises full Baileys parity. This module
//! used to repeat that claim as its own, which was wrong: until 2026-09-10 it
//! had no media path at all, and outbound media is still missing.
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
    /// `[multimodal]` defaults; the factory overrides it with the operator's.
    multimodal: crate::config::MultimodalConfig,
    /// Bot handle for shutdown
    bot_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Client handle for sending messages and typing indicators
    client: Arc<Mutex<Option<Arc<wa_rs::Client>>>>,
    /// Message sender channel
    tx: Arc<Mutex<Option<tokio::sync::mpsc::Sender<ChannelMessage>>>>,
}

/// How long before the same chat may be told again that a message was dropped.
///
/// The queue saturates in bursts, so without this a user who sent five messages
/// during a busy turn would get five identical apologies. They need to know
/// once that they were not heard, not once per message.
#[cfg(feature = "whatsapp-web")]
const DROP_NOTICE_COOLDOWN: std::time::Duration = std::time::Duration::from_mins(1);

/// What the sender is told when their message could not be queued.
#[cfg(feature = "whatsapp-web")]
const DROP_NOTICE: &str =
    "I could not take that message just now — I am still working through the previous one. \
     Please send it again in a moment.";

impl WhatsAppWebChannel {
    /// Whether `chat` may be told about a drop now, recording the notice if so.
    ///
    /// Split out so the cooldown is testable without a live client: the whole
    /// decision is here, and the event loop only performs the send.
    #[cfg(feature = "whatsapp-web")]
    fn claim_drop_notice(
        seen: &Mutex<std::collections::HashMap<String, std::time::Instant>>,
        chat: &str,
        now: std::time::Instant,
    ) -> bool {
        let mut seen = seen.lock();
        // Bound the map: without this it grows one entry per chat, forever.
        seen.retain(|_, last| now.duration_since(*last) < DROP_NOTICE_COOLDOWN);
        match seen.get(chat) {
            Some(last) if now.duration_since(*last) < DROP_NOTICE_COOLDOWN => false,
            _ => {
                seen.insert(chat.to_string(), now);
                true
            }
        }
    }

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
            multimodal: crate::config::MultimodalConfig::default(),
            bot_handle: Arc::new(Mutex::new(None)),
            client: Arc::new(Mutex::new(None)),
            tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Apply the operator's `[multimodal]` limits to inbound images.
    #[must_use]
    pub fn with_multimodal(mut self, multimodal: crate::config::MultimodalConfig) -> Self {
        self.multimodal = multimodal;
        self
    }

    /// Upload one attachment and send it as a media message.
    ///
    /// A remote URL is sent as text: WhatsApp previews links, and re-uploading
    /// someone else's public file would spend bandwidth to gain nothing. A
    /// local path is read, uploaded through `wa-rs`, and only from inside the
    /// workspace — a reply is influenced by whoever is chatting, so a prompt
    /// injection naming the config would otherwise post the session and
    /// provider keys into the chat. Same rule and same function as Telegram and
    /// Discord.
    #[cfg(feature = "whatsapp-web")]
    async fn send_attachment(
        &self,
        client: &wa_rs::Client,
        to: wa_rs_binary::jid::Jid,
        attachment: &crate::channels::media::OutboundAttachment,
    ) -> Result<()> {
        let target = attachment.target.trim();

        if crate::channels::media::is_http_url(target) {
            // Boxed: `send_message` builds a ~34 KB future and clippy's
            // `large_futures` is denied on changed lines. Same treatment the
            // pairing call in `listen` already gets.
            Box::pin(client.send_message(
                to,
                wa_rs_proto::whatsapp::Message {
                    conversation: Some(target.to_string()),
                    ..Default::default()
                },
            ))
            .await?;
            return Ok(());
        }

        let path = std::path::Path::new(target);
        if !path.exists() {
            anyhow::bail!("WhatsApp attachment path not found: {target}");
        }
        let (_config_path, workspace_dir) = {
            use anyhow::Context as _;
            crate::config::Config::resolve_active_paths()
                .await
                .context("cannot resolve workspace to validate attachment path")?
        };
        if !crate::channels::media::path_within_workspace(path, &workspace_dir) {
            anyhow::bail!(
                "WhatsApp attachment path is outside the workspace and was blocked: {target}"
            );
        }

        let bytes = {
            use anyhow::Context as _;
            tokio::fs::read(path)
                .await
                .with_context(|| format!("cannot read attachment: {target}"))?
        };
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();
        // Sniffed from the bytes, not guessed from the extension: the recipient
        // renders on what we declare here.
        let mimetype = crate::channels::media::sniff_image_mime(&bytes)
            .unwrap_or("application/octet-stream")
            .to_string();

        let upload = {
            use anyhow::Context as _;
            client
                .upload(bytes, Self::media_type_for(attachment.kind))
                .await
                .with_context(|| format!("WhatsApp upload failed for {file_name}"))?
        };

        let outgoing =
            Self::outgoing_media_message(attachment.kind, &upload, &mimetype, &file_name);
        Box::pin(client.send_message(to, outgoing)).await?;
        Ok(())
    }

    /// Which wa-rs media bucket a marker kind uploads into.
    ///
    /// Pure so the mapping is testable: an upload encrypted under the wrong
    /// `MediaType` produces a file the recipient's client cannot open, and that
    /// failure is invisible from this side.
    #[cfg(feature = "whatsapp-web")]
    pub(crate) fn media_type_for(
        kind: crate::channels::media::AttachmentKind,
    ) -> wa_rs_core::download::MediaType {
        use crate::channels::media::AttachmentKind;
        use wa_rs_core::download::MediaType;
        match kind {
            AttachmentKind::Image => MediaType::Image,
            AttachmentKind::Video => MediaType::Video,
            // Voice notes ride the audio bucket; `ptt` on the message is what
            // makes WhatsApp render one as a voice note rather than a file.
            AttachmentKind::Audio | AttachmentKind::Voice => MediaType::Audio,
            AttachmentKind::Document => MediaType::Document,
        }
    }

    /// Build the outgoing message for an upload that already succeeded.
    ///
    /// Separated from the upload so the part that is easy to get wrong — which
    /// of the six fields each message type carries, and whether `ptt` is set —
    /// is reachable from a test. The client call around it is two lines.
    #[cfg(feature = "whatsapp-web")]
    pub(crate) fn outgoing_media_message(
        kind: crate::channels::media::AttachmentKind,
        upload: &wa_rs::upload::UploadResponse,
        mimetype: &str,
        file_name: &str,
    ) -> wa_rs_proto::whatsapp::Message {
        use crate::channels::media::AttachmentKind;
        use wa_rs_proto::whatsapp::{message, Message};

        // Every media message carries the same six values the upload returned.
        macro_rules! common {
            ($t:expr) => {{
                let mut m = $t;
                m.url = Some(upload.url.clone());
                m.direct_path = Some(upload.direct_path.clone());
                m.media_key = Some(upload.media_key.clone());
                m.file_enc_sha256 = Some(upload.file_enc_sha256.clone());
                m.file_sha256 = Some(upload.file_sha256.clone());
                m.file_length = Some(upload.file_length);
                m.mimetype = Some(mimetype.to_string());
                m
            }};
        }

        match kind {
            AttachmentKind::Image => Message {
                image_message: Some(Box::new(common!(message::ImageMessage::default()))),
                ..Default::default()
            },
            AttachmentKind::Video => Message {
                video_message: Some(Box::new(common!(message::VideoMessage::default()))),
                ..Default::default()
            },
            AttachmentKind::Audio | AttachmentKind::Voice => {
                let mut audio = common!(message::AudioMessage::default());
                audio.ptt = Some(kind == AttachmentKind::Voice);
                Message {
                    audio_message: Some(Box::new(audio)),
                    ..Default::default()
                }
            }
            AttachmentKind::Document => {
                let mut doc = common!(message::DocumentMessage::default());
                // Without a name the recipient sees an untitled blob.
                doc.file_name = Some(file_name.to_string());
                Message {
                    document_message: Some(Box::new(doc)),
                    ..Default::default()
                }
            }
        }
    }

    /// Is there anything here to answer?
    ///
    /// Extracted from the `listen()` closure so a test can reach it — no test
    /// enters that closure, which is the same structural hole
    /// `every_channel_listen_path_calls_its_allowlist_gate` exists for. Keying
    /// this on text alone dropped an image sent with no caption.
    pub(crate) fn has_deliverable_content(text: &str, has_image: bool) -> bool {
        !text.trim().is_empty() || has_image
    }

    /// Turn downloaded image bytes into the marker the agent sees.
    ///
    /// Separated from the download so the part that carries the risk — the
    /// budget, the size cap and the byte sniffing — is reachable from a test
    /// without the `whatsapp-web` feature, a linked session or a live socket.
    /// The library call around it is a single `client.download(..)`.
    ///
    /// `charge` is explicit here because the bytes arrive from a library call
    /// rather than a URL, so `fetch_image_bytes` (which charges internally) is
    /// not on this path. Charged *before* the bytes are examined, matching
    /// every other channel.
    pub(crate) fn image_marker_from_bytes(
        bytes: &[u8],
        claimed: Option<&str>,
        multimodal: &crate::config::MultimodalConfig,
        sender: &str,
    ) -> String {
        use crate::channels::media::{ImageBytes, MediaOutcome};

        let sender_key = format!("whatsapp:{sender}");
        if let Err(note) = crate::channels::media::charge(&sender_key) {
            return MediaOutcome::Rejected(note).to_marker();
        }
        let cap = crate::channels::media::max_bytes(multimodal);
        match crate::channels::media::accept_image_bytes(bytes, claimed, cap) {
            ImageBytes::Ok { mime, bytes } => {
                use base64::Engine as _;
                let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                MediaOutcome::Image(format!("data:{mime};base64,{encoded}")).to_marker()
            }
            ImageBytes::Rejected(note) => MediaOutcome::Rejected(note).to_marker(),
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

    /// WhatsApp Web can deliver attachments, so the model is told the syntax.
    /// Telling a channel that cannot leaks `[IMAGE:…]` to the reader as literal
    /// text, which is why this is per-channel and not a default.
    fn delivery_instructions(&self) -> Option<&'static str> {
        static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        Some(TEXT.get_or_init(|| crate::channels::media::delivery_instructions_for("WhatsApp")))
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

        // Attachments come out of the text before rendering, or the markers
        // reach the reader as literal `[IMAGE:…]`.
        let (text, attachments) =
            crate::channels::media::parse_attachment_markers(&message.content);
        if !attachments.is_empty() {
            if !text.is_empty() {
                let rendered =
                    crate::channels::format::render_to_string(&text, &self.render_target());
                Box::pin(client.send_message(
                    to.clone(),
                    wa_rs_proto::whatsapp::Message {
                        conversation: Some(rendered),
                        ..Default::default()
                    },
                ))
                .await?;
            }
            for attachment in &attachments {
                Box::pin(self.send_attachment(&client, to.clone(), attachment)).await?;
            }
            return Ok(());
        }

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

        use super::whatsapp_http::ReqwestHttpClient;
        use wa_rs::bot::Bot;
        use wa_rs::pair_code::PairCodeOptions;
        use wa_rs::store::{Device, DeviceStore};
        use wa_rs_binary::jid::JidExt as _;
        use wa_rs_core::proto_helpers::MessageExt;
        use wa_rs_core::types::events::Event;
        use wa_rs_tokio_transport::TokioWebSocketTransportFactory;

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
        let http_client = ReqwestHttpClient::new();

        // Build the bot
        let tx_clone = tx.clone();
        let allowed_numbers = self.allowed_numbers.clone();
        let multimodal = self.multimodal.clone();
        // Last time each chat was told a message was dropped, so a saturated
        // queue produces one apology per chat rather than one per message.
        let drop_notices: Arc<Mutex<std::collections::HashMap<String, std::time::Instant>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));

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
                let drop_notices = Arc::clone(&drop_notices);
                let multimodal = multimodal.clone();
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
                                // An image sent with no caption has empty text
                                // and is still a message. Keying "is there
                                // anything here" on text alone dropped it.
                                let image = msg.image_message.as_deref();
                                if !Self::has_deliverable_content(trimmed, image.is_some()) {
                                    tracing::debug!(
                                        "WhatsApp Web: ignoring empty or non-text message from {}",
                                        normalized
                                    );
                                    return;
                                }

                                // Download through the library, then apply the
                                // shared policy. `client.download` is the whole
                                // of the wa-rs side: `ImageMessage` already
                                // implements `Downloadable`.
                                let mut content = trimmed.to_string();
                                if let Some(img) = image {
                                    let claimed = img.mimetype.clone();
                                    let marker = match client.download(img).await {
                                        Ok(bytes) => Self::image_marker_from_bytes(
                                            &bytes,
                                            claimed.as_deref(),
                                            &multimodal,
                                            &normalized,
                                        ),
                                        Err(e) => {
                                            tracing::warn!(
                                                "WhatsApp Web: an inbound image could not be \
                                                 downloaded: {e}"
                                            );
                                            crate::channels::media::MediaOutcome::Rejected(
                                                "Attachment unavailable: the image could not be \
                                                 downloaded"
                                                    .into(),
                                            )
                                            .to_marker()
                                        }
                                    };
                                    if !content.is_empty() {
                                        content.push('\n');
                                    }
                                    content.push_str(&marker);
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
                                    content: content.clone(),
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
                                    // Tell the sender. A dropped message was
                                    // silent to them: no reply, no reason, and
                                    // nothing to distinguish a busy agent from
                                    // a broken bot. The notice goes out through
                                    // wa-rs, not the agent queue, so it cannot
                                    // re-enter this path.
                                    //
                                    // Only on `Full`. `Closed` means the runtime
                                    // is shutting down — there is nothing to try
                                    // again with, and the send would race the
                                    // teardown.
                                    let chat = chat_jid.to_string();
                                    if matches!(e, tokio::sync::mpsc::error::TrySendError::Full(_))
                                        && Self::claim_drop_notice(
                                            &drop_notices,
                                            &chat,
                                            std::time::Instant::now(),
                                        )
                                    {
                                        let notice = wa_rs_proto::whatsapp::Message {
                                            conversation: Some(DROP_NOTICE.to_string()),
                                            ..Default::default()
                                        };
                                        if let Err(e) =
                                            Box::pin(client.send_message(chat_jid.clone(), notice))
                                                .await
                                        {
                                            tracing::warn!(
                                                "WhatsApp Web: could not tell {chat} its message \
                                                 was dropped: {e}"
                                            );
                                        }
                                    }
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

    async fn start_typing(&self, recipient: &str, _thread_ts: Option<&str>) -> Result<()> {
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

    async fn start_typing(&self, _recipient: &str, _thread_ts: Option<&str>) -> Result<()> {
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
    use super::whatsapp_http::ReqwestHttpClient;
    use async_stream::stream;
    use tokio::sync::mpsc;
    use wa_rs::bot::Bot;
    use wa_rs::pair_code::PairCodeOptions;
    use wa_rs::store::{Device, DeviceStore};
    use wa_rs_core::types::events::Event;
    use wa_rs_tokio_transport::TokioWebSocketTransportFactory;

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
                .with_http_client(ReqwestHttpClient::new())
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

/// Inbound-image policy, reachable without a linked session or a live socket.
///
/// Gated on the feature like the module's other tests, which is fine because
/// `whatsapp-web` is a default feature: these run in an ordinary
/// `cargo test --lib`.
#[cfg(all(test, feature = "whatsapp-web"))]
mod media_tests {
    use super::*;

    /// A 1x1 PNG. Small, real bytes, so the sniffer has something to read.
    fn png() -> Vec<u8> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
            .expect("fixture decodes")
    }

    fn caps(max_mb: usize) -> crate::config::MultimodalConfig {
        let mut m = crate::config::MultimodalConfig::default();
        m.max_image_size_mb = max_mb;
        m
    }

    /// The happy path. Until 2026-09-10 this channel had no media path at all,
    /// so an image sent to it simply vanished.
    #[test]
    fn an_inbound_image_becomes_an_image_marker() {
        let marker = WhatsAppWebChannel::image_marker_from_bytes(
            &png(),
            Some("image/png"),
            &caps(8),
            "15550000001",
        );
        assert!(
            marker.contains("IMAGE:") && marker.contains("data:image/png;base64,"),
            "an accepted image must reach the agent as a marker: {marker}"
        );
    }

    /// Over the operator's cap becomes a visible note, not a silent drop and
    /// not a truncated image.
    #[test]
    fn an_image_over_the_operators_cap_is_refused_with_a_note() {
        let big = vec![0x89u8; 2 * 1024 * 1024];
        let marker = WhatsAppWebChannel::image_marker_from_bytes(
            &big,
            Some("image/png"),
            &caps(1),
            "15550000002",
        );
        assert!(
            marker.contains("too large"),
            "the refusal must say why: {marker}"
        );
        assert!(
            !marker.contains("base64"),
            "nothing may be forwarded when the cap is exceeded: {marker}"
        );
    }

    /// The claimed type is checked against the bytes rather than trusted.
    #[test]
    fn a_non_image_file_is_refused_with_a_reason() {
        let marker = WhatsAppWebChannel::image_marker_from_bytes(
            b"%PDF-1.7 not an image at all",
            Some("application/pdf"),
            &caps(8),
            "15550000003",
        );
        assert!(
            marker.contains("rejected") || marker.contains("unsupported"),
            "a non-image must be refused with a reason: {marker}"
        );
        assert!(!marker.contains("base64"), "{marker}");
    }

    /// Empty bytes are a failed download, not an empty image.
    #[test]
    fn an_empty_download_is_reported_rather_than_forwarded() {
        let marker = WhatsAppWebChannel::image_marker_from_bytes(
            &[],
            Some("image/png"),
            &caps(8),
            "15550000004",
        );
        assert!(!marker.contains("base64"), "{marker}");
        assert!(
            marker.contains("unavailable") || marker.contains("rejected"),
            "{marker}"
        );
    }

    /// An image with no caption is still a message. This is what dropped it.
    #[test]
    fn an_image_with_no_caption_is_still_something_to_answer() {
        assert!(
            WhatsAppWebChannel::has_deliverable_content("", true),
            "an image with no caption must be delivered"
        );
        assert!(WhatsAppWebChannel::has_deliverable_content(
            "look at this",
            true
        ));
        assert!(WhatsAppWebChannel::has_deliverable_content(
            "just words",
            false
        ));
        assert!(
            !WhatsAppWebChannel::has_deliverable_content("   ", false),
            "whitespace with no image is still nothing"
        );
    }

    fn upload(len: u64) -> wa_rs::upload::UploadResponse {
        wa_rs::upload::UploadResponse {
            url: "https://mmg.whatsapp.net/x".into(),
            direct_path: "/v/x".into(),
            media_key: vec![1, 2, 3],
            file_enc_sha256: vec![4, 5, 6],
            file_sha256: vec![7, 8, 9],
            file_length: len,
        }
    }

    /// Every media message must carry all six values the upload returned. Drop
    /// one and the recipient's client cannot decrypt the file, which looks like
    /// a broken attachment rather than a bug on this side.
    #[test]
    fn every_media_message_carries_the_whole_upload() {
        use crate::channels::media::AttachmentKind;
        let up = upload(4096);

        for kind in [
            AttachmentKind::Image,
            AttachmentKind::Video,
            AttachmentKind::Audio,
            AttachmentKind::Voice,
            AttachmentKind::Document,
        ] {
            let m = WhatsAppWebChannel::outgoing_media_message(kind, &up, "image/png", "f.png");
            // Read the six back out of whichever variant was built.
            let got = m
                .image_message
                .as_ref()
                .map(|i| (&i.url, &i.direct_path, &i.media_key, &i.file_length))
                .or_else(|| {
                    m.video_message
                        .as_ref()
                        .map(|v| (&v.url, &v.direct_path, &v.media_key, &v.file_length))
                })
                .or_else(|| {
                    m.audio_message
                        .as_ref()
                        .map(|a| (&a.url, &a.direct_path, &a.media_key, &a.file_length))
                })
                .or_else(|| {
                    m.document_message
                        .as_ref()
                        .map(|d| (&d.url, &d.direct_path, &d.media_key, &d.file_length))
                })
                .unwrap_or_else(|| panic!("{kind:?} built no media message"));

            assert_eq!(
                got.0.as_deref(),
                Some("https://mmg.whatsapp.net/x"),
                "{kind:?}"
            );
            assert_eq!(got.1.as_deref(), Some("/v/x"), "{kind:?}");
            assert_eq!(got.2.as_deref(), Some(&[1u8, 2, 3][..]), "{kind:?}");
            assert_eq!(*got.3, Some(4096), "{kind:?}");
        }
    }

    /// Each marker kind builds its own message type. Sending a video as an
    /// image is not a cosmetic difference: the recipient's client renders on
    /// this.
    #[test]
    fn each_kind_builds_its_own_message_type() {
        use crate::channels::media::AttachmentKind;
        let up = upload(1);

        let img = WhatsAppWebChannel::outgoing_media_message(
            AttachmentKind::Image,
            &up,
            "image/png",
            "f.png",
        );
        assert!(img.image_message.is_some() && img.document_message.is_none());

        let vid = WhatsAppWebChannel::outgoing_media_message(
            AttachmentKind::Video,
            &up,
            "video/mp4",
            "f.mp4",
        );
        assert!(vid.video_message.is_some() && vid.image_message.is_none());

        let doc = WhatsAppWebChannel::outgoing_media_message(
            AttachmentKind::Document,
            &up,
            "application/pdf",
            "report.pdf",
        );
        let doc_msg = doc.document_message.as_ref().expect("document");
        assert_eq!(
            doc_msg.file_name.as_deref(),
            Some("report.pdf"),
            "a document with no name shows as an untitled blob"
        );
    }

    /// `ptt` is the only thing separating a voice note from an audio file, and
    /// both ride the same upload bucket.
    #[test]
    fn a_voice_note_is_flagged_ptt_and_an_audio_file_is_not() {
        use crate::channels::media::AttachmentKind;
        let up = upload(1);

        let voice = WhatsAppWebChannel::outgoing_media_message(
            AttachmentKind::Voice,
            &up,
            "audio/ogg",
            "v.ogg",
        );
        assert_eq!(
            voice.audio_message.as_ref().expect("audio").ptt,
            Some(true),
            "a voice marker must render as a voice note"
        );

        let audio = WhatsAppWebChannel::outgoing_media_message(
            AttachmentKind::Audio,
            &up,
            "audio/mpeg",
            "a.mp3",
        );
        assert_eq!(
            audio.audio_message.as_ref().expect("audio").ptt,
            Some(false)
        );
    }

    /// The upload bucket decides the encryption keys. Encrypt under the wrong
    /// one and the file is undecryptable, invisibly from this side.
    #[test]
    fn each_kind_uploads_into_its_own_media_bucket() {
        use crate::channels::media::AttachmentKind;
        use wa_rs_core::download::MediaType;
        assert_eq!(
            WhatsAppWebChannel::media_type_for(AttachmentKind::Image),
            MediaType::Image
        );
        assert_eq!(
            WhatsAppWebChannel::media_type_for(AttachmentKind::Video),
            MediaType::Video
        );
        assert_eq!(
            WhatsAppWebChannel::media_type_for(AttachmentKind::Document),
            MediaType::Document
        );
        // Both voice forms share the audio bucket; `ptt` differentiates them.
        assert_eq!(
            WhatsAppWebChannel::media_type_for(AttachmentKind::Audio),
            MediaType::Audio
        );
        assert_eq!(
            WhatsAppWebChannel::media_type_for(AttachmentKind::Voice),
            MediaType::Audio
        );
    }

    /// WhatsApp Web can deliver attachments now, so the model is told how to
    /// ask, in the same vocabulary every other delivering channel uses.
    #[test]
    fn whatsapp_web_tells_the_model_the_marker_syntax() {
        let ch = WhatsAppWebChannel::new("/tmp/wa.db".into(), None, None, vec!["*".into()]);
        let text = ch
            .delivery_instructions()
            .expect("whatsapp web can deliver attachments");
        assert!(text.contains("WhatsApp"), "{text}");
        for marker in ["[IMAGE:", "[DOCUMENT:", "[VIDEO:", "[AUDIO:", "[VOICE:"] {
            assert!(text.contains(marker), "missing {marker}: {text}");
        }
    }

    /// The budget is charged per channel-qualified sender, and charged before
    /// the bytes are examined, so a stream of images cannot outspend the
    /// allowance by failing a later check.
    #[test]
    fn a_sender_over_their_budget_does_not_get_the_image_forwarded() {
        let sender = format!("15559{}", std::process::id() % 100_000);
        let key = format!("whatsapp:{sender}");
        while crate::channels::media::charge(&key).is_ok() {}
        let marker = WhatsAppWebChannel::image_marker_from_bytes(
            &png(),
            Some("image/png"),
            &caps(8),
            &sender,
        );
        assert!(
            !marker.contains("base64"),
            "a sender over budget must not get their image forwarded: {marker}"
        );
    }
}

#[cfg(all(test, feature = "whatsapp-web"))]
mod tests {
    use super::*;

    /// A dropped inbound message used to be silent to the sender: the log said
    /// so, they got no reply and no reason, and a busy agent looked exactly like
    /// a broken bot. The notice closes that — but a saturated queue drops in
    /// bursts, so it must not turn one busy turn into five apologies.
    #[test]
    fn a_chat_is_told_about_a_drop_once_per_cooldown() {
        use std::collections::HashMap;
        use std::time::Instant;

        let seen = Mutex::new(HashMap::new());
        let t0 = Instant::now();

        assert!(
            WhatsAppWebChannel::claim_drop_notice(&seen, "chat-a", t0),
            "the first drop in a chat must be reported"
        );
        assert!(
            !WhatsAppWebChannel::claim_drop_notice(&seen, "chat-a", t0),
            "a second drop in the same burst must not repeat the apology"
        );

        // A different chat is a different person waiting on a reply.
        assert!(
            WhatsAppWebChannel::claim_drop_notice(&seen, "chat-b", t0),
            "another chat must still be told"
        );

        // Once the window has passed, they are told again — the alternative is
        // going quiet on someone whose second attempt also failed.
        let later = t0 + DROP_NOTICE_COOLDOWN + std::time::Duration::from_secs(1);
        assert!(WhatsAppWebChannel::claim_drop_notice(
            &seen, "chat-a", later
        ));
    }

    /// The map is keyed per chat and would otherwise grow for the lifetime of
    /// the process — the same unbounded-set shape `seen_messages` was fixed for.
    #[test]
    fn the_drop_notice_map_does_not_grow_without_bound() {
        use std::collections::HashMap;
        use std::time::Instant;

        let seen = Mutex::new(HashMap::new());
        let t0 = Instant::now();
        for i in 0..50 {
            WhatsAppWebChannel::claim_drop_notice(&seen, &format!("chat-{i}"), t0);
        }
        assert_eq!(seen.lock().len(), 50);

        // One call after the window drops every stale entry.
        let later = t0 + DROP_NOTICE_COOLDOWN + std::time::Duration::from_secs(1);
        WhatsAppWebChannel::claim_drop_notice(&seen, "chat-fresh", later);
        assert_eq!(
            seen.lock().len(),
            1,
            "entries older than the cooldown must be evicted"
        );
    }

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
            // Its own table since schema v32. This is also the test that proves
            // a `/claim` from WhatsApp Web still reaches an allowlist: the
            // channel reports `name() == "whatsapp"`, so the pairing arm has to
            // fall through to the Web table when there is no Cloud one.
            seed.channels_config.whatsapp_web = Some(crate::config::schema::WhatsAppWebConfig {
                session_path: "/tmp/wa.db".into(),
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
        // The Web table, not the Cloud one. `Channel::name()` is still
        // `"whatsapp"` for both transports, so `apply_pairing` matches the
        // `"whatsapp"` arm and has to fall through to `whatsapp_web` when no
        // Cloud table exists. If that fallback is dropped this unwrap panics.
        let numbers = &config
            .channels_config
            .whatsapp_web
            .as_ref()
            .expect("the Web table must still exist after a /claim")
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
