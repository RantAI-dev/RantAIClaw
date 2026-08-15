#![allow(clippy::uninlined_format_args)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::trim_split_whitespace)]
#![allow(clippy::doc_link_with_quotes)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::unnecessary_map_or)]

use anyhow::{anyhow, Result};
use async_imap::extensions::idle::IdleResponse;
use async_imap::types::Fetch;
use async_imap::Session;
use async_trait::async_trait;
use futures_util::TryStreamExt;
use lettre::message::SinglePart;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use mail_parser::{MessageParser, MimeHeaders};
use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::DnsName;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, timeout};
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::traits::{Channel, ChannelMessage, SendMessage};

/// Email channel configuration
///
/// `Debug` is hand-written rather than derived: the derive rendered
/// `password` in full, and this struct reaches `{:?}` on any config-dump or
/// error path, which is how a mailbox password ends up in a retained log.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct EmailConfig {
    /// IMAP server hostname
    pub imap_host: String,
    /// IMAP server port (default: 993 for TLS)
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    /// IMAP folder to poll (default: INBOX)
    #[serde(default = "default_imap_folder")]
    pub imap_folder: String,
    /// SMTP server hostname
    pub smtp_host: String,
    /// SMTP server port (default: 465 for TLS)
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    /// Use TLS for SMTP (default: true)
    #[serde(default = "default_true")]
    pub smtp_tls: bool,
    /// Email username for authentication
    pub username: String,
    /// Email password for authentication
    pub password: String,
    /// From address for outgoing emails
    pub from_address: String,
    /// IDLE timeout in seconds before re-establishing connection (default: 1740 = 29 minutes)
    /// RFC 2177 recommends clients restart IDLE every 29 minutes
    #[serde(default = "default_idle_timeout", alias = "poll_interval_secs")]
    pub idle_timeout_secs: u64,
    /// Allowed sender addresses/domains (empty = deny all, ["*"] = allow all)
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    /// Refuse mail whose `From:` is not backed by SPF/DKIM/DMARC.
    ///
    /// Off by default because a relay that strips `Authentication-Results`
    /// would otherwise silence a working mailbox. It does **not** gate the
    /// owner path: an address in `approval_owners` is refused when
    /// unauthenticated regardless of this flag — see
    /// [`EmailChannel::sender_identity`].
    #[serde(default)]
    pub require_authenticated_sender: bool,
}

impl std::fmt::Debug for EmailConfig {
    /// Every field except `password`, which renders as a fixed marker.
    ///
    /// Destructuring rather than skipping a field is deliberate: a credential
    /// added later becomes a compile error here, so the next one cannot be
    /// introduced and silently printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            imap_host,
            imap_port,
            imap_folder,
            smtp_host,
            smtp_port,
            smtp_tls,
            username,
            password: _,
            from_address,
            idle_timeout_secs,
            allowed_senders,
            require_authenticated_sender,
        } = self;

        f.debug_struct("EmailConfig")
            .field("imap_host", imap_host)
            .field("imap_port", imap_port)
            .field("imap_folder", imap_folder)
            .field("smtp_host", smtp_host)
            .field("smtp_port", smtp_port)
            .field("smtp_tls", smtp_tls)
            .field("username", username)
            .field("password", &"[redacted]")
            .field("from_address", from_address)
            .field("idle_timeout_secs", idle_timeout_secs)
            .field("allowed_senders", allowed_senders)
            .field("require_authenticated_sender", require_authenticated_sender)
            .finish()
    }
}

fn default_imap_port() -> u16 {
    993
}
fn default_smtp_port() -> u16 {
    465
}
fn default_imap_folder() -> String {
    "INBOX".into()
}
fn default_idle_timeout() -> u64 {
    1740 // 29 minutes per RFC 2177
}
fn default_true() -> bool {
    true
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            imap_host: String::new(),
            imap_port: default_imap_port(),
            imap_folder: default_imap_folder(),
            smtp_host: String::new(),
            smtp_port: default_smtp_port(),
            smtp_tls: true,
            username: String::new(),
            password: String::new(),
            from_address: String::new(),
            idle_timeout_secs: default_idle_timeout(),
            allowed_senders: Vec::new(),
            require_authenticated_sender: false,
        }
    }
}

type ImapSession = Session<TlsStream<TcpStream>>;

/// Email channel — IMAP IDLE for instant push notifications, SMTP for outbound
pub struct EmailChannel {
    pub config: EmailConfig,
    /// Addresses that may approve gated tools. Injected rather than loaded so
    /// the owner check below needs no IO on the message path.
    approval_owners: Vec<String>,
    /// Runtime-mutable so a console or CLI allowlist edit reaches the running
    /// channel. `std` lock because `is_sender_allowed` is called from sync
    /// context. Seeded from `config.allowed_senders`, which stays the
    /// across-restart source of truth.
    allowed_senders: Arc<std::sync::RwLock<Vec<String>>>,
    /// Bounded: an unbounded `HashSet` grew for the lifetime of the process,
    /// one entry per message ever seen.
    seen_messages: Arc<Mutex<(std::collections::VecDeque<String>, HashSet<String>)>>,
    /// Size/type limits for inbound images. Defaults to the shipped
    /// `[multimodal]` defaults; the factory overrides it with the operator's.
    multimodal: crate::config::MultimodalConfig,
}

impl EmailChannel {
    pub fn new(config: EmailConfig) -> Self {
        Self {
            allowed_senders: Arc::new(std::sync::RwLock::new(config.allowed_senders.clone())),
            config,
            approval_owners: Vec::new(),
            seen_messages: Arc::new(Mutex::new((
                std::collections::VecDeque::new(),
                HashSet::new(),
            ))),
            multimodal: crate::config::MultimodalConfig::default(),
        }
    }

    /// Apply the operator's `[multimodal]` limits to inbound images.
    #[must_use]
    pub fn with_multimodal(mut self, multimodal: crate::config::MultimodalConfig) -> Self {
        self.multimodal = multimodal;
        self
    }

    /// Remember a message id, evicting the oldest once the window is full.
    async fn remember_seen(&self, id: String) {
        const MAX_SEEN: usize = 2_000;
        let mut guard = self.seen_messages.lock().await;
        let (order, set) = &mut *guard;
        if !set.insert(id.clone()) {
            return;
        }
        order.push_back(id);
        while order.len() > MAX_SEEN {
            if let Some(old) = order.pop_front() {
                set.remove(&old);
            }
        }
    }

    /// Tell the channel which addresses carry owner authority, so it can refuse
    /// to hand that authority to an unauthenticated `From:`.
    #[must_use]
    pub fn with_approval_owners(mut self, owners: Vec<String>) -> Self {
        self.approval_owners = owners;
        self
    }

    /// Check if a sender email is in the allowlist
    pub fn is_sender_allowed(&self, email: &str) -> bool {
        let Ok(senders) = self.allowed_senders.read() else {
            return false; // A poisoned lock denies rather than admits.
        };
        if senders.is_empty() {
            return false; // Empty = deny all
        }
        if senders.iter().any(|a| a == "*") {
            return true; // Wildcard = allow all
        }
        let email_lower = email.to_lowercase();
        senders.iter().any(|allowed| {
            if allowed.starts_with('@') {
                // Domain match with @ prefix: "@example.com"
                email_lower.ends_with(&allowed.to_lowercase())
            } else if allowed.contains('@') {
                // Full email address match
                allowed.eq_ignore_ascii_case(email)
            } else {
                // Domain match without @ prefix: "example.com"
                email_lower.ends_with(&format!("@{}", allowed.to_lowercase()))
            }
        })
    }

    /// Whether the receiving MTA vouched for the `From:` domain.
    ///
    /// `From:` is attacker-controlled — it is a header, not a credential — so
    /// on its own it identifies nobody. `Authentication-Results` is written by
    /// *our* MTA after it checked SPF/DKIM/DMARC, which is why it is the only
    /// part of the message worth trusting here.
    ///
    /// Read through `mail_parser`'s header API rather than a hand-rolled
    /// scanner: a bespoke parser for a security decision is how subtle
    /// bypasses get in, and the plan forbids it.
    ///
    /// Accepts `dmarc=pass`, or `spf=pass`/`dkim=pass` whose stated domain
    /// aligns with the `From:` domain. An unaligned pass proves someone
    /// authenticated — just not the person the `From:` claims.
    fn from_domain_is_authenticated(parsed: &mail_parser::Message, from_addr: &str) -> bool {
        let Some(from_domain) = from_addr.rsplit('@').next().map(str::to_lowercase) else {
            return false;
        };
        if from_domain.is_empty() {
            return false;
        }

        let Some(header) = parsed.header("Authentication-Results") else {
            return false;
        };
        let raw = match header.as_text() {
            Some(t) => t.to_lowercase(),
            None => return false,
        };

        if raw.contains("dmarc=pass") {
            return true;
        }

        // An spf/dkim pass only counts when it names the From: domain.
        for method in ["spf=pass", "dkim=pass"] {
            let mut rest = raw.as_str();
            while let Some(pos) = rest.find(method) {
                let tail = &rest[pos + method.len()..];
                // The domain appears in the same clause, before the next `;`.
                let clause = tail.split(';').next().unwrap_or("");
                if clause.contains(&from_domain) {
                    return true;
                }
                rest = tail;
            }
        }
        false
    }

    /// The sender to attribute a message to, or `None` when it must be dropped.
    ///
    /// Two independent refusals:
    ///
    /// 1. `require_authenticated_sender` is on and the mail is unauthenticated.
    /// 2. The address carries owner authority and the mail is unauthenticated —
    ///    **regardless of that flag**. Handing owner rights to a forgeable
    ///    header is the one case with no acceptable default, so it is not
    ///    configurable.
    ///
    /// Never falls back to `"unknown"`. That string is a shared identity: every
    /// unattributable sender collapses into one principal, and anything keyed
    /// on the sender then treats strangers as the same person.
    fn sender_identity(&self, parsed: &mail_parser::Message) -> Option<String> {
        let from = Self::extract_sender(parsed);
        if from == "unknown" {
            warn!("Email: dropping a message with no parseable From: address");
            return None;
        }

        let authenticated = Self::from_domain_is_authenticated(parsed, &from);
        if authenticated {
            return Some(from);
        }

        let claims_owner = self
            .approval_owners
            .iter()
            .any(|o| o.eq_ignore_ascii_case(&from));
        if claims_owner {
            warn!(
                "Email: dropping mail claiming to be from approval owner {from} — \
                 no SPF/DKIM/DMARC pass for that domain. From: is forgeable, so it \
                 cannot grant owner authority on its own."
            );
            return None;
        }

        if self.config.require_authenticated_sender {
            warn!(
                "Email: dropping unauthenticated mail from {from} — \
                 require_authenticated_sender is on and the message carries no \
                 SPF/DKIM/DMARC pass for that domain."
            );
            return None;
        }

        Some(from)
    }

    /// Turn an HTML body into something worth putting in a prompt.
    ///
    /// Kept hand-rolled rather than pulling in an HTML-to-text crate: this is
    /// a lossy best-effort for prompt text, not rendering, and CLAUDE.md treats
    /// dependency weight as a product goal. The two things it got wrong are
    /// fixed here instead.
    ///
    /// `<script>` and `<style>` bodies are skipped. Removing only the *tags*
    /// left their contents behind, so a marketing email put its whole
    /// stylesheet and tracking JavaScript into the prompt — tokens spent on
    /// text no human would ever have seen, and attacker-influenced text at
    /// that.
    ///
    /// The handful of entities that survive tag-stripping are decoded, since
    /// `&amp;nbsp;` and `&amp;amp;` otherwise reach the model verbatim.
    pub fn strip_html(html: &str) -> String {
        let mut result = String::new();
        let mut in_tag = false;
        let mut rest = html;

        while let Some(pos) = rest.find('<') {
            let lower_tail = rest[pos..].to_lowercase();
            let skipped = ["script", "style"].iter().find_map(|el| {
                if lower_tail.starts_with(&format!("<{el}")) {
                    lower_tail
                        .find(&format!("</{el}"))
                        .and_then(|end| rest[pos + end..].find('>').map(|c| pos + end + c + 1))
                } else {
                    None
                }
            });
            match skipped {
                Some(after) => {
                    result.push_str(&rest[..pos]);
                    result.push(' ');
                    rest = &rest[after..];
                }
                None => {
                    // Not a skipped element — hand this chunk to the tag
                    // stripper below by advancing one character.
                    let take = pos + rest[pos..].chars().next().map_or(1, char::len_utf8);
                    let (head, tail) = rest.split_at(take);
                    result.push_str(head);
                    rest = tail;
                }
            }
        }
        result.push_str(rest);

        let stripped = result;
        let mut result = String::new();
        for ch in stripped.chars() {
            match ch {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => result.push(ch),
                _ => {}
            }
        }
        let result = result
            .replace("&nbsp;", " ")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&apos;", "'")
            // `&amp;` last: decoding it first would turn `&amp;lt;` into `<`.
            .replace("&amp;", "&");
        let mut normalized = String::with_capacity(result.len());
        for word in result.split_whitespace() {
            if !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.push_str(word);
        }
        normalized
    }

    /// Extract the sender address from a parsed email
    fn extract_sender(parsed: &mail_parser::Message) -> String {
        parsed
            .from()
            .and_then(|addr| addr.first())
            .and_then(|a| a.address())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".into())
    }

    /// Extract readable text from a parsed email
    fn extract_text(parsed: &mail_parser::Message) -> String {
        if let Some(text) = parsed.body_text(0) {
            return text.to_string();
        }
        if let Some(html) = parsed.body_html(0) {
            return Self::strip_html(html.as_ref());
        }
        for part in parsed.attachments() {
            let part: &mail_parser::MessagePart = part;
            if let Some(ct) = MimeHeaders::content_type(part) {
                if ct.ctype() == "text" {
                    if let Ok(text) = std::str::from_utf8(part.contents()) {
                        let name = MimeHeaders::attachment_name(part).unwrap_or("file");
                        return format!("[Attachment: {}]\n{}", name, text);
                    }
                }
            }
        }
        "(no readable content)".to_string()
    }

    /// The text one email contributes to the agent's context: subject, body,
    /// then a marker per image attachment.
    ///
    /// Split out of `fetch_unseen` so it is reachable without an IMAP session.
    /// Assembling it inline there meant no test could see what the agent
    /// actually receives.
    fn message_content(&self, parsed: &mail_parser::Message) -> String {
        let subject = parsed.subject().unwrap_or("(no subject)");
        let mut content = format!("Subject: {}\n\n{}", subject, Self::extract_text(parsed));
        for marker in self.attachment_markers(parsed) {
            content.push('\n');
            content.push_str(&marker);
        }
        content
    }

    /// Turn an email's image attachments into markers, per
    /// `docs/security/inbound-media-policy.md`: images become `[IMAGE:data:…]`,
    /// a rejection becomes a visible note.
    ///
    /// Email is the one channel that needs no fetch — `mail_parser` has already
    /// decoded the bytes — so no credential and no attacker-chosen host are
    /// involved, and only the size/type half of the policy applies.
    ///
    /// Runs on the whole message, not on `extract_text`'s attachment loop:
    /// that loop is a *fallback* reached only when the mail has neither a text
    /// nor an HTML body, so a screenshot attached to an ordinary email never
    /// reached it at all.
    ///
    /// Parts that neither claim to be an image nor look like one are left
    /// alone. Every channel's attachments are things a human deliberately
    /// attached; an email's also include protocol furniture — vCards, calendar
    /// invites, delivery reports — and a "not an image" note on each of those
    /// would be noise on ordinary mail. Anything that *is* or *claims to be* an
    /// image still always produces a marker.
    fn attachment_markers(&self, parsed: &mail_parser::Message) -> Vec<String> {
        use crate::channels::media;

        let (max_images, _) = self.multimodal.effective_limits();
        let cap = media::max_bytes(&self.multimodal);

        parsed
            .attachments()
            .filter_map(|part| {
                let bytes = part.contents();
                let claimed = MimeHeaders::content_type(part)
                    .map(|ct| match ct.subtype() {
                        Some(sub) => format!("{}/{}", ct.ctype(), sub),
                        None => ct.ctype().to_string(),
                    })
                    .unwrap_or_default();
                let claimed = (!claimed.is_empty()).then_some(claimed);
                // Either half is enough: the claim catches an oversized JPEG
                // whose bytes we would otherwise skip, and the sniff catches a
                // real PNG mislabelled `application/pdf`.
                if !media::claimed_type_is_image(claimed.as_deref())
                    && media::sniff_image_mime(bytes).is_none()
                {
                    return None;
                }
                Some(media::accept_bytes(bytes, claimed.as_deref(), cap).to_marker())
            })
            .take(max_images)
            .collect()
    }

    /// Connect to IMAP server with TLS and authenticate
    async fn connect_imap(&self) -> Result<ImapSession> {
        let addr = format!("{}:{}", self.config.imap_host, self.config.imap_port);
        debug!("Connecting to IMAP server at {}", addr);

        // Connect TCP
        let tcp = TcpStream::connect(&addr).await?;

        // Establish TLS using rustls
        let certs = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        let config = ClientConfig::builder()
            .with_root_certificates(certs)
            .with_no_client_auth();
        let tls_stream: TlsConnector = Arc::new(config).into();
        let sni: DnsName = self.config.imap_host.clone().try_into()?;
        let stream = tls_stream.connect(sni.into(), tcp).await?;

        // Create IMAP client
        let client = async_imap::Client::new(stream);

        // Login
        let session = client
            .login(&self.config.username, &self.config.password)
            .await
            .map_err(|(e, _)| anyhow!("IMAP login failed: {}", e))?;

        debug!("IMAP login successful");
        Ok(session)
    }

    /// Fetch and process unseen messages from the selected mailbox
    async fn fetch_unseen(&self, session: &mut ImapSession) -> Result<Vec<ParsedEmail>> {
        // Search for unseen messages
        let uids = session.uid_search("UNSEEN").await?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        debug!("Found {} unseen messages", uids.len());

        let mut results = Vec::new();
        let uid_set = Self::uid_list(&uids.iter().copied().collect::<Vec<_>>());

        // Fetch message bodies
        let messages = session.uid_fetch(&uid_set, "RFC822").await?;
        let messages: Vec<Fetch> = messages.try_collect().await?;

        let mut parsed_uids: Vec<u32> = Vec::new();
        let mut unparseable_uids: Vec<u32> = Vec::new();

        for msg in messages {
            let uid = msg.uid.unwrap_or(0);
            let Some(parsed) = msg.body().and_then(|b| MessageParser::default().parse(b)) else {
                unparseable_uids.push(uid);
                continue;
            };
            // Parsed — so it is accounted for either way below, including when
            // the sender is refused.
            parsed_uids.push(uid);

            let Some(sender) = self.sender_identity(&parsed) else {
                // Refused above, with the reason logged. Marked seen so
                // the same forgery is not re-evaluated every poll.
                self.remember_seen(uid.to_string()).await;
                continue;
            };
            // Reached only once `sender_identity` above has accepted the sender.
            let content = self.message_content(&parsed);
            let msg_id = parsed
                .message_id()
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("gen-{}", Uuid::new_v4()));

            #[allow(clippy::cast_sign_loss)]
            // `to_timestamp()` applies the header's UTC offset. The
            // previous code rebuilt a `NaiveDate` from the parts and
            // called `and_utc()`, which reads a local wall-clock time
            // as if it were UTC — every message from a non-UTC sender
            // was stamped hours off.
            let ts = parsed
                .date()
                .map(|d| u64::try_from(d.to_timestamp()).unwrap_or(0))
                .unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                });

            results.push(ParsedEmail {
                _uid: uid,
                msg_id,
                sender,
                content,
                timestamp: ts,
            });
        }

        if !unparseable_uids.is_empty() {
            warn!(
                "IMAP: {} message(s) could not be parsed; flagging them for review: {:?}",
                unparseable_uids.len(),
                unparseable_uids
            );
        }

        for (set, flags) in Self::flag_stores(&parsed_uids, &unparseable_uids) {
            let _ = session
                .uid_store(&set, flags)
                .await?
                .try_collect::<Vec<_>>()
                .await;
        }

        Ok(results)
    }

    fn uid_list(uids: &[u32]) -> String {
        uids.iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The `\Seen` stores a fetched batch needs, split by parse outcome.
    ///
    /// Previously one store covered the whole fetched set, and only when at
    /// least one message parsed. That lost unparseable mail into `\Seen` with
    /// no trace, and a batch where *every* message failed to parse flagged
    /// nothing at all — so the next poll refetched the same UIDs, forever.
    ///
    /// Unparseable UIDs get `\Flagged` alongside `\Seen`: they leave UNSEEN so
    /// the loop ends, and stay visible in the mailbox so they have not
    /// silently vanished. A custom keyword would say more, but servers may
    /// refuse one that is not in `PERMANENTFLAGS`; `\Flagged` is universal.
    fn flag_stores(parsed: &[u32], unparseable: &[u32]) -> Vec<(String, &'static str)> {
        let mut stores = Vec::new();
        if !parsed.is_empty() {
            stores.push((Self::uid_list(parsed), "+FLAGS (\\Seen)"));
        }
        if !unparseable.is_empty() {
            stores.push((Self::uid_list(unparseable), "+FLAGS (\\Seen \\Flagged)"));
        }
        stores
    }

    /// Run the IDLE loop, returning when a new message arrives or timeout
    /// Note: IDLE consumes the session and returns it via done()
    async fn wait_for_changes(
        &self,
        session: ImapSession,
    ) -> Result<(IdleWaitResult, ImapSession)> {
        let idle_timeout = Duration::from_secs(self.config.idle_timeout_secs);

        // Start IDLE mode - this consumes the session
        let mut idle = session.idle();
        idle.init().await?;

        debug!("Entering IMAP IDLE mode");

        // wait() returns (future, stop_source) - we only need the future
        let (wait_future, _stop_source) = idle.wait();

        // Wait for server notification or timeout
        let result = timeout(idle_timeout, wait_future).await;

        match result {
            Ok(Ok(response)) => {
                debug!("IDLE response: {:?}", response);
                // Done with IDLE, return session to normal mode
                let session = idle.done().await?;
                let wait_result = match response {
                    IdleResponse::NewData(_) => IdleWaitResult::NewMail,
                    IdleResponse::Timeout => IdleWaitResult::Timeout,
                    IdleResponse::ManualInterrupt => IdleWaitResult::Interrupted,
                };
                Ok((wait_result, session))
            }
            Ok(Err(e)) => {
                // Try to clean up IDLE state
                let _ = idle.done().await;
                Err(anyhow!("IDLE error: {}", e))
            }
            Err(_) => {
                // Timeout - RFC 2177 recommends restarting IDLE every 29 minutes
                debug!("IDLE timeout reached, will re-establish");
                let session = idle.done().await?;
                Ok((IdleWaitResult::Timeout, session))
            }
        }
    }

    /// Main IDLE-based listen loop with automatic reconnection
    async fn listen_with_idle(&self, tx: mpsc::Sender<ChannelMessage>) -> Result<()> {
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_mins(1);

        loop {
            match self.run_idle_session(&tx).await {
                Ok(()) => {
                    // Clean exit (channel closed)
                    return Ok(());
                }
                Err(e) => {
                    error!(
                        "IMAP session error: {}. Reconnecting in {:?}...",
                        e, backoff
                    );
                    sleep(backoff).await;
                    // Exponential backoff with cap
                    backoff = std::cmp::min(backoff * 2, max_backoff);
                }
            }
        }
    }

    /// Run a single IDLE session until error or clean shutdown
    async fn run_idle_session(&self, tx: &mpsc::Sender<ChannelMessage>) -> Result<()> {
        // Connect and authenticate
        let mut session = self.connect_imap().await?;

        // Select the mailbox
        session.select(&self.config.imap_folder).await?;
        info!(
            "Email IDLE listening on {} (instant push enabled)",
            self.config.imap_folder
        );

        // Check for existing unseen messages first
        self.process_unseen(&mut session, tx).await?;

        loop {
            // Enter IDLE and wait for changes (consumes session, returns it via result)
            match self.wait_for_changes(session).await {
                Ok((IdleWaitResult::NewMail, returned_session)) => {
                    debug!("New mail notification received");
                    session = returned_session;
                    self.process_unseen(&mut session, tx).await?;
                }
                Ok((IdleWaitResult::Timeout, returned_session)) => {
                    // Re-check for mail after IDLE timeout (defensive)
                    session = returned_session;
                    self.process_unseen(&mut session, tx).await?;
                }
                Ok((IdleWaitResult::Interrupted, _)) => {
                    info!("IDLE interrupted, exiting");
                    return Ok(());
                }
                Err(e) => {
                    // Connection likely broken, need to reconnect
                    return Err(e);
                }
            }
        }
    }

    /// Fetch unseen messages and send to channel
    async fn process_unseen(
        &self,
        session: &mut ImapSession,
        tx: &mpsc::Sender<ChannelMessage>,
    ) -> Result<()> {
        let messages = self.fetch_unseen(session).await?;

        for email in messages {
            // Check allowlist
            if !self.is_sender_allowed(&email.sender) {
                warn!("Blocked email from {}", email.sender);
                continue;
            }

            let is_new = {
                let already = self.seen_messages.lock().await.1.contains(&email.msg_id);
                if !already {
                    self.remember_seen(email.msg_id.clone()).await;
                }
                !already
            };
            if !is_new {
                continue;
            }

            let msg = ChannelMessage {
                sender_aliases: Vec::new(),
                id: email.msg_id,
                reply_target: email.sender.clone(),
                sender: email.sender,
                content: email.content,
                channel: "email".to_string(),
                timestamp: email.timestamp,
                thread_ts: None,
            };

            if tx.send(msg).await.is_err() {
                // Channel closed, exit cleanly
                return Ok(());
            }
        }

        Ok(())
    }

    /// Build the SMTP transport, refusing to put credentials on the wire in
    /// the clear.
    ///
    /// `smtp_tls = false` used to mean `builder_dangerous` **with credentials
    /// attached** — the mailbox username and password sent over an unencrypted
    /// connection, to whoever is listening. Three branches now:
    ///
    /// - implicit TLS (`relay`, typically port 465),
    /// - STARTTLS (`starttls_relay`, typically 587),
    /// - plaintext, permitted only when there is no credential to leak.
    ///
    /// A credential-less local relay on port 25 is a legitimate setup, so
    /// plaintext stays reachable — it just cannot carry a secret.
    /// Replace the live allowlist so a console or CLI edit reaches this
    /// channel without a restart. The trait method plan 115 added.
    ///
    /// Replaces rather than merges: a *removal* has to take effect too, or
    /// revoking someone's access would need the daemon bounced.
    ///
    /// Note for plan 144: `is_sender_allowed`'s matching rules — bare domain,
    /// `@domain`, full address, `*`, all case-insensitive — are undocumented
    /// in the channel reference.
    pub fn set_allowed_senders(&self, allowed: &[String]) {
        if let Ok(mut senders) = self.allowed_senders.write() {
            if senders.as_slice() != allowed {
                info!(
                    target: "channels",
                    channel = "email",
                    count = allowed.len(),
                    "applied updated allowlist from config"
                );
                *senders = allowed.to_vec();
            }
        }
    }

    fn create_smtp_transport(&self) -> Result<SmtpTransport> {
        let has_credentials = !self.config.username.is_empty() || !self.config.password.is_empty();

        if !self.config.smtp_tls && has_credentials {
            return Err(anyhow!(
                "Refusing to send SMTP credentials over a plaintext connection to {}:{}.\n\
                 `smtp_tls = false` disables encryption, and the username and password would \
                 travel in the clear.\n\
                 Fix: set [channels_config.email] smtp_tls = true (port 465 for implicit TLS, \
                 587 for STARTTLS), or clear `username` and `password` if this really is an \
                 unauthenticated local relay.",
                self.config.smtp_host,
                self.config.smtp_port
            ));
        }

        let creds = Credentials::new(self.config.username.clone(), self.config.password.clone());

        let transport = if !self.config.smtp_tls {
            // No credentials to protect — see the guard above.
            SmtpTransport::builder_dangerous(&self.config.smtp_host)
                .port(self.config.smtp_port)
                .build()
        } else if self.config.smtp_port == 587 {
            // 587 is the submission port: the session starts in the clear and
            // upgrades. `relay()` would try to negotiate TLS immediately and
            // fail against a STARTTLS-only server.
            SmtpTransport::starttls_relay(&self.config.smtp_host)?
                .port(self.config.smtp_port)
                .credentials(creds)
                .build()
        } else {
            SmtpTransport::relay(&self.config.smtp_host)?
                .port(self.config.smtp_port)
                .credentials(creds)
                .build()
        };
        Ok(transport)
    }
}

/// Internal struct for parsed email data
struct ParsedEmail {
    _uid: u32,
    msg_id: String,
    sender: String,
    content: String,
    timestamp: u64,
}

/// Result from waiting on IDLE
enum IdleWaitResult {
    NewMail,
    Timeout,
    Interrupted,
}

#[async_trait]
impl Channel for EmailChannel {
    fn name(&self) -> &str {
        "email"
    }

    fn apply_allowed_senders(&self, allowed: &[String]) {
        self.set_allowed_senders(allowed);
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        // Use explicit subject if provided, otherwise fall back to legacy parsing or default
        let (subject, body) = if let Some(ref subj) = message.subject {
            (subj.as_str(), message.content.as_str())
        } else if message.content.starts_with("Subject: ") {
            if let Some(pos) = message.content.find('\n') {
                (&message.content[9..pos], message.content[pos + 1..].trim())
            } else {
                ("RantaiClaw Message", message.content.as_str())
            }
        } else {
            ("RantaiClaw Message", message.content.as_str())
        };

        // Render only the reply body (a text/plain part) — the Subject: and
        // quote handling above operate on the raw content and stay untouched.
        let rendered_body = crate::channels::format::render_to_string(
            body,
            &crate::channels::format::RenderTarget::Plain,
        );

        let email = Message::builder()
            .from(self.config.from_address.parse()?)
            .to(message.recipient.parse()?)
            .subject(subject)
            .singlepart(SinglePart::plain(rendered_body))?;

        let transport = self.create_smtp_transport()?;
        transport.send(&email)?;
        info!("Email sent to {}", message.recipient);
        Ok(())
    }

    async fn listen(
        &self,
        tx: mpsc::Sender<ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        info!(
            "Starting email channel with IDLE support on {}",
            self.config.imap_folder
        );
        self.listen_with_idle(tx).await
    }

    async fn health_check(&self) -> bool {
        // Fully async health check - attempt IMAP connection
        match timeout(Duration::from_secs(10), self.connect_imap()).await {
            Ok(Ok(mut session)) => {
                // Try to logout cleanly
                let _ = session.logout().await;
                true
            }
            Ok(Err(e)) => {
                debug!("Health check failed: {}", e);
                false
            }
            Err(_) => {
                debug!("Health check timed out");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_smtp_port_uses_tls_port() {
        assert_eq!(default_smtp_port(), 465);
    }

    #[test]
    fn email_config_default_uses_tls_smtp_defaults() {
        let config = EmailConfig::default();
        assert_eq!(config.smtp_port, 465);
        assert!(config.smtp_tls);
    }

    #[test]
    fn default_idle_timeout_is_29_minutes() {
        assert_eq!(default_idle_timeout(), 1740);
    }

    #[tokio::test]
    async fn seen_messages_starts_empty() {
        let channel = EmailChannel::new(EmailConfig::default());
        let seen = channel.seen_messages.lock().await;
        assert!(seen.1.is_empty());
    }

    #[tokio::test]
    async fn seen_messages_tracks_unique_ids() {
        let channel = EmailChannel::new(EmailConfig::default());
        channel.remember_seen("first-id".to_string()).await;
        channel.remember_seen("first-id".to_string()).await;
        channel.remember_seen("second-id".to_string()).await;

        let seen = channel.seen_messages.lock().await;
        assert_eq!(seen.1.len(), 2, "a repeat id must not be counted twice");
        assert_eq!(seen.0.len(), 2, "the eviction queue must stay in step");
    }

    /// The set used to grow for the lifetime of the process, one entry per
    /// message ever seen.
    #[tokio::test]
    async fn seen_messages_is_bounded() {
        let channel = EmailChannel::new(EmailConfig::default());
        for i in 0..2_500 {
            channel.remember_seen(format!("id-{i}")).await;
        }
        let seen = channel.seen_messages.lock().await;
        assert_eq!(seen.1.len(), 2_000, "the window must cap");
        assert_eq!(seen.0.len(), 2_000);
        assert!(!seen.1.contains("id-0"), "the oldest must be evicted");
        assert!(seen.1.contains("id-2499"), "the newest must be kept");
    }

    /// The `\Seen` store used to cover the whole fetched set, so a message the
    /// parser rejected was filed as read and never seen by anyone.
    #[test]
    fn unparseable_message_is_not_marked_seen() {
        let stores = EmailChannel::flag_stores(&[11], &[12]);
        let plain = stores
            .iter()
            .find(|(_, flags)| *flags == "+FLAGS (\\Seen)")
            .expect("parsed UIDs must still be marked seen");
        assert_eq!(
            plain.0, "11",
            "only the parsed UID belongs in the plain \\Seen store"
        );
        let flagged = stores
            .iter()
            .find(|(_, flags)| flags.contains("\\Flagged"))
            .expect("the unparseable UID must be flagged, not silently filed");
        assert_eq!(flagged.0, "12");
    }

    /// A batch where every message failed to parse flagged nothing at all, so
    /// the next `UNSEEN` search returned the same UIDs — forever.
    #[test]
    fn all_unparseable_batch_does_not_loop() {
        let stores = EmailChannel::flag_stores(&[], &[7, 8]);
        assert_eq!(
            stores.len(),
            1,
            "an all-unparseable batch must still issue a store: {stores:?}"
        );
        assert_eq!(stores[0].0, "7,8");
        assert!(
            stores[0].1.contains("\\Seen"),
            "the UIDs must leave UNSEEN or the poll refetches them"
        );
    }

    #[test]
    fn flag_stores_is_empty_for_an_empty_batch() {
        assert!(EmailChannel::flag_stores(&[], &[]).is_empty());
    }

    // EmailConfig tests

    #[test]
    fn email_config_default() {
        let config = EmailConfig::default();
        assert_eq!(config.imap_host, "");
        assert_eq!(config.imap_port, 993);
        assert_eq!(config.imap_folder, "INBOX");
        assert_eq!(config.smtp_host, "");
        assert_eq!(config.smtp_port, 465);
        assert!(config.smtp_tls);
        assert_eq!(config.username, "");
        assert_eq!(config.password, "");
        assert_eq!(config.from_address, "");
        assert_eq!(config.idle_timeout_secs, 1740);
        assert!(config.allowed_senders.is_empty());
    }

    #[test]
    fn email_config_custom() {
        let config = EmailConfig {
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            imap_folder: "Archive".to_string(),
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 465,
            smtp_tls: true,
            username: "user@example.com".to_string(),
            password: "pass123".to_string(),
            from_address: "bot@example.com".to_string(),
            idle_timeout_secs: 1200,
            allowed_senders: vec!["allowed@example.com".to_string()],
            require_authenticated_sender: false,
        };
        assert_eq!(config.imap_host, "imap.example.com");
        assert_eq!(config.imap_folder, "Archive");
        assert_eq!(config.idle_timeout_secs, 1200);
    }

    #[test]
    fn email_config_clone() {
        let config = EmailConfig {
            imap_host: "imap.test.com".to_string(),
            imap_port: 993,
            imap_folder: "INBOX".to_string(),
            smtp_host: "smtp.test.com".to_string(),
            smtp_port: 587,
            smtp_tls: true,
            username: "user@test.com".to_string(),
            password: "secret".to_string(),
            from_address: "bot@test.com".to_string(),
            idle_timeout_secs: 1740,
            allowed_senders: vec!["*".to_string()],
            require_authenticated_sender: false,
        };
        let cloned = config.clone();
        assert_eq!(cloned.imap_host, config.imap_host);
        assert_eq!(cloned.smtp_port, config.smtp_port);
        assert_eq!(cloned.allowed_senders, config.allowed_senders);
    }

    // HTML to prompt text

    /// The defect: only the tags were removed, so the stylesheet and the
    /// tracking script landed in the prompt as text.
    #[test]
    fn script_and_style_bodies_do_not_reach_the_prompt() {
        let html =
            "<p>Hello</p><style>.x{color:red}</style><script>track('abc')</script><p>Bye</p>";
        let out = EmailChannel::strip_html(html);
        assert!(!out.contains("color:red"), "{out}");
        assert!(!out.contains("track"), "{out}");
        assert_eq!(out, "Hello Bye");
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(
            EmailChannel::strip_html("<p>a&nbsp;b &amp; c &lt;d&gt;</p>"),
            "a b & c <d>"
        );
    }

    /// `&amp;` must decode last, or `&amp;lt;` collapses to `<` and a literal
    /// that was escaped on purpose turns back into markup.
    #[test]
    fn double_escaped_entities_decode_once() {
        assert_eq!(EmailChannel::strip_html("<p>&amp;lt;</p>"), "&lt;");
    }

    #[test]
    fn plain_html_still_works() {
        assert_eq!(
            EmailChannel::strip_html("<div><b>bold</b> and <i>italic</i></div>"),
            "bold and italic"
        );
    }

    // Runtime allowlist

    /// A config edit must reach the running channel — including a removal,
    /// which is the half that matters when revoking access.
    #[test]
    fn apply_allowed_senders_replaces_the_live_list() {
        let ch = EmailChannel::new(EmailConfig {
            allowed_senders: vec!["old@example.com".to_string()],
            ..Default::default()
        });
        assert!(ch.is_sender_allowed("old@example.com"));

        ch.apply_allowed_senders(&["new@example.com".to_string()]);
        assert!(ch.is_sender_allowed("new@example.com"));
        assert!(
            !ch.is_sender_allowed("old@example.com"),
            "a removal must take effect, or revoking access needs a restart"
        );
    }

    /// The existing domain semantics must survive the move behind a lock.
    #[test]
    fn apply_allowed_senders_keeps_domain_matching() {
        let ch = EmailChannel::new(EmailConfig::default());
        ch.apply_allowed_senders(&["example.com".to_string()]);
        assert!(ch.is_sender_allowed("anyone@example.com"));
        assert!(!ch.is_sender_allowed("anyone@other.com"));
    }

    // SMTP transport safety

    fn smtp_channel(tls: bool, port: u16, user: &str, pass: &str) -> EmailChannel {
        EmailChannel::new(EmailConfig {
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: port,
            smtp_tls: tls,
            username: user.to_string(),
            password: pass.to_string(),
            ..Default::default()
        })
    }

    /// The defect: `smtp_tls = false` attached the mailbox credentials to an
    /// unencrypted connection and sent them anyway.
    #[test]
    fn plaintext_with_credentials_is_refused() {
        let ch = smtp_channel(false, 25, "bot@example.com", "placeholder-mailbox-password");
        let err = ch.create_smtp_transport().unwrap_err().to_string();
        assert!(err.contains("plaintext"), "{err}");
        assert!(
            !err.contains("placeholder-mailbox-password"),
            "the error must not repeat the secret: {err}"
        );
    }

    /// A password with no username is still a password.
    #[test]
    fn plaintext_with_only_a_password_is_still_refused() {
        let ch = smtp_channel(false, 25, "", "placeholder-mailbox-password");
        assert!(ch.create_smtp_transport().is_err());
    }

    /// An unauthenticated local relay is a legitimate setup and must keep working.
    #[test]
    fn plaintext_without_credentials_is_allowed() {
        let ch = smtp_channel(false, 25, "", "");
        assert!(ch.create_smtp_transport().is_ok());
    }

    #[test]
    fn tls_ports_build_with_credentials() {
        for port in [465u16, 587] {
            let ch = smtp_channel(true, port, "bot@example.com", "placeholder-pass");
            assert!(
                ch.create_smtp_transport().is_ok(),
                "port {port} should build"
            );
        }
    }

    // Inbound attachments

    /// A multipart mail with a plain-text body and one attachment part.
    ///
    /// The body matters: `extract_text` returns at the first text or HTML body
    /// and only falls through to the attachment loop when there is neither, so
    /// a screenshot on an ordinary email is exactly the case that used to be
    /// dropped.
    fn mail_with_attachment(part_mime: &str, bytes: &[u8]) -> String {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        format!(
            "From: user_a@example.com\r\n\
             Subject: look at this\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n\
             \r\n\
             --BOUND\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             here is the shot\r\n\
             --BOUND\r\n\
             Content-Type: {part_mime}\r\n\
             Content-Disposition: attachment; filename=\"part.bin\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             {encoded}\r\n\
             --BOUND--\r\n"
        )
    }

    fn content_for(ch: &EmailChannel, raw: &str) -> String {
        let parsed = MessageParser::default()
            .parse(raw.as_bytes())
            .expect("parse mail");
        ch.message_content(&parsed)
    }

    fn png_bytes(padding: usize) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend(std::iter::repeat_n(0u8, padding));
        bytes
    }

    fn media_channel(max_image_size_mb: usize) -> EmailChannel {
        EmailChannel::new(EmailConfig {
            allowed_senders: vec!["*".to_string()],
            ..Default::default()
        })
        .with_multimodal(crate::config::MultimodalConfig {
            max_image_size_mb,
            ..Default::default()
        })
    }

    /// The defect: an attached screenshot never reached the agent, because the
    /// only attachment handling sat in `extract_text`'s no-body fallback.
    #[test]
    fn an_attached_image_reaches_the_agent_alongside_the_body() {
        let out = content_for(
            &media_channel(5),
            &mail_with_attachment("image/png", &png_bytes(32)),
        );
        assert!(
            out.contains("[IMAGE:data:image/png;base64,"),
            "no image marker: {out}"
        );
        // The body is still there — the marker is appended, not substituted.
        assert!(out.contains("here is the shot"), "{out}");
        assert!(out.starts_with("Subject: look at this"), "{out}");
    }

    /// The bytes decide, not the sender's `Content-Type`.
    #[test]
    fn bytes_that_are_not_an_image_are_rejected_even_when_claimed_as_one() {
        let out = content_for(
            &media_channel(5),
            &mail_with_attachment("image/png", b"%PDF-1.7 not a png"),
        );
        assert!(out.contains("unsupported type"), "{out}");
        assert!(!out.contains("[IMAGE:"), "{out}");
    }

    /// An honest PNG mislabelled `application/pdf` must still be caught and
    /// reported, not skipped as "not media".
    #[test]
    fn an_image_mislabelled_as_a_document_is_reported_not_dropped() {
        let out = content_for(
            &media_channel(5),
            &mail_with_attachment("application/pdf", &png_bytes(32)),
        );
        assert!(out.contains("type mismatch"), "{out}");
        assert!(!out.contains("[IMAGE:"), "{out}");
    }

    /// The cap comes from `[multimodal].max_image_size_mb`, and going over it
    /// is a visible note rather than a truncated image.
    #[test]
    fn an_oversized_image_becomes_a_visible_note() {
        // `effective_limits` clamps to 1 MiB minimum, so the payload must beat that.
        let out = content_for(
            &media_channel(1),
            &mail_with_attachment("image/png", &png_bytes(1024 * 1024 + 1)),
        );
        // Truncated on purpose: the failing value here is a megabyte of base64,
        // and dumping it turns one CI failure into an unreadable log.
        let head: String = out.chars().take(200).collect();
        assert!(out.contains("too large"), "{head}");
        assert!(
            !out.contains("[IMAGE:"),
            "accepted an oversized image: {head}"
        );
    }

    /// Protocol furniture — a calendar invite, a vCard, a delivery report — is
    /// not media and must not produce a "not an image" note on ordinary mail.
    #[test]
    fn a_non_media_attachment_produces_no_marker() {
        let out = content_for(
            &media_channel(5),
            &mail_with_attachment("text/calendar", b"BEGIN:VCALENDAR\r\nEND:VCALENDAR"),
        );
        assert!(!out.contains("[IMAGE:"), "{out}");
        assert!(!out.contains("Attachment rejected"), "{out}");
        assert!(out.contains("here is the shot"), "{out}");
    }

    // Sender authentication

    /// Build a raw message with an optional `Authentication-Results` header.
    fn raw_mail(from: &str, auth: Option<&str>) -> String {
        let auth_line = auth.map_or(String::new(), |a| {
            format!("Authentication-Results: {a}\r\n")
        });
        format!("From: {from}\r\n{auth_line}Subject: hi\r\n\r\nbody\r\n")
    }

    fn identity(channel: &EmailChannel, from: &str, auth: Option<&str>) -> Option<String> {
        let raw = raw_mail(from, auth);
        let parsed = MessageParser::default().parse(raw.as_bytes()).unwrap();
        channel.sender_identity(&parsed)
    }

    fn owner_channel(require_auth: bool) -> EmailChannel {
        let config = EmailConfig {
            allowed_senders: vec!["*".to_string()],
            require_authenticated_sender: require_auth,
            ..Default::default()
        };
        EmailChannel::new(config).with_approval_owners(vec!["owner@example.com".to_string()])
    }

    /// The core of the defect: `From:` is a header, not a credential.
    #[test]
    fn unauthenticated_mail_cannot_claim_to_be_an_approval_owner() {
        let ch = owner_channel(false);
        assert_eq!(
            identity(&ch, "owner@example.com", None),
            None,
            "owner authority must never come from an unverified From:"
        );
        assert_eq!(
            identity(&ch, "owner@example.com", Some("mx.example.com; spf=fail")),
            None
        );
    }

    #[test]
    fn an_authenticated_owner_is_accepted() {
        let ch = owner_channel(false);
        assert_eq!(
            identity(&ch, "owner@example.com", Some("mx.example.com; dmarc=pass")),
            Some("owner@example.com".to_string())
        );
    }

    /// A pass that names a different domain proves someone authenticated —
    /// just not the person the `From:` claims to be.
    #[test]
    fn an_unaligned_pass_does_not_authenticate_the_from_domain() {
        let ch = owner_channel(false);
        assert_eq!(
            identity(
                &ch,
                "owner@example.com",
                Some("mx.example.com; spf=pass smtp.mailfrom=attacker.test")
            ),
            None
        );
        assert_eq!(
            identity(
                &ch,
                "owner@example.com",
                Some("mx.example.com; spf=pass smtp.mailfrom=example.com")
            ),
            Some("owner@example.com".to_string())
        );
    }

    /// Non-owner mail still flows by default: a relay that strips the header
    /// must not silence an otherwise working mailbox.
    #[test]
    fn ordinary_unauthenticated_mail_passes_when_the_flag_is_off() {
        let ch = owner_channel(false);
        assert_eq!(
            identity(&ch, "someone@example.com", None),
            Some("someone@example.com".to_string())
        );
    }

    #[test]
    fn the_flag_turns_ordinary_unauthenticated_mail_away() {
        let ch = owner_channel(true);
        assert_eq!(identity(&ch, "someone@example.com", None), None);
        assert_eq!(
            identity(
                &ch,
                "someone@example.com",
                Some("mx; dkim=pass header.d=example.com")
            ),
            Some("someone@example.com".to_string())
        );
    }

    /// `"unknown"` is a shared identity — every unattributable sender collapses
    /// into one principal — so an unparseable `From:` is dropped, not renamed.
    #[test]
    fn an_unparseable_from_is_dropped_rather_than_called_unknown() {
        let ch = owner_channel(false);
        let parsed = MessageParser::default()
            .parse(b"Subject: no from\r\n\r\nbody\r\n".as_slice())
            .unwrap();
        assert_eq!(ch.sender_identity(&parsed), None);
    }

    // EmailChannel tests

    #[tokio::test]
    async fn email_channel_new() {
        let config = EmailConfig::default();
        let channel = EmailChannel::new(config.clone());
        assert_eq!(channel.config.imap_host, config.imap_host);

        let seen_guard = channel.seen_messages.lock().await;
        assert_eq!(seen_guard.1.len(), 0);
    }

    #[test]
    fn email_channel_name() {
        let channel = EmailChannel::new(EmailConfig::default());
        assert_eq!(channel.name(), "email");
    }

    // is_sender_allowed tests

    #[test]
    fn is_sender_allowed_empty_list_denies_all() {
        let config = EmailConfig {
            allowed_senders: vec![],
            ..Default::default()
        };
        let channel = EmailChannel::new(config);
        assert!(!channel.is_sender_allowed("anyone@example.com"));
        assert!(!channel.is_sender_allowed("user@test.com"));
    }

    #[test]
    fn is_sender_allowed_wildcard_allows_all() {
        let config = EmailConfig {
            allowed_senders: vec!["*".to_string()],
            ..Default::default()
        };
        let channel = EmailChannel::new(config);
        assert!(channel.is_sender_allowed("anyone@example.com"));
        assert!(channel.is_sender_allowed("user@test.com"));
        assert!(channel.is_sender_allowed("random@domain.org"));
    }

    #[test]
    fn is_sender_allowed_specific_email() {
        let config = EmailConfig {
            allowed_senders: vec!["allowed@example.com".to_string()],
            ..Default::default()
        };
        let channel = EmailChannel::new(config);
        assert!(channel.is_sender_allowed("allowed@example.com"));
        assert!(!channel.is_sender_allowed("other@example.com"));
        assert!(!channel.is_sender_allowed("allowed@other.com"));
    }

    #[test]
    fn is_sender_allowed_domain_with_at_prefix() {
        let config = EmailConfig {
            allowed_senders: vec!["@example.com".to_string()],
            ..Default::default()
        };
        let channel = EmailChannel::new(config);
        assert!(channel.is_sender_allowed("user@example.com"));
        assert!(channel.is_sender_allowed("admin@example.com"));
        assert!(!channel.is_sender_allowed("user@other.com"));
    }

    #[test]
    fn is_sender_allowed_domain_without_at_prefix() {
        let config = EmailConfig {
            allowed_senders: vec!["example.com".to_string()],
            ..Default::default()
        };
        let channel = EmailChannel::new(config);
        assert!(channel.is_sender_allowed("user@example.com"));
        assert!(channel.is_sender_allowed("admin@example.com"));
        assert!(!channel.is_sender_allowed("user@other.com"));
    }

    #[test]
    fn is_sender_allowed_case_insensitive() {
        let config = EmailConfig {
            allowed_senders: vec!["Allowed@Example.COM".to_string()],
            ..Default::default()
        };
        let channel = EmailChannel::new(config);
        assert!(channel.is_sender_allowed("allowed@example.com"));
        assert!(channel.is_sender_allowed("ALLOWED@EXAMPLE.COM"));
        assert!(channel.is_sender_allowed("AlLoWeD@eXaMpLe.cOm"));
    }

    #[test]
    fn is_sender_allowed_multiple_senders() {
        let config = EmailConfig {
            allowed_senders: vec![
                "user1@example.com".to_string(),
                "user2@test.com".to_string(),
                "@allowed.com".to_string(),
            ],
            ..Default::default()
        };
        let channel = EmailChannel::new(config);
        assert!(channel.is_sender_allowed("user1@example.com"));
        assert!(channel.is_sender_allowed("user2@test.com"));
        assert!(channel.is_sender_allowed("anyone@allowed.com"));
        assert!(!channel.is_sender_allowed("user3@example.com"));
    }

    #[test]
    fn is_sender_allowed_wildcard_with_specific() {
        let config = EmailConfig {
            allowed_senders: vec!["*".to_string(), "specific@example.com".to_string()],
            ..Default::default()
        };
        let channel = EmailChannel::new(config);
        assert!(channel.is_sender_allowed("anyone@example.com"));
        assert!(channel.is_sender_allowed("specific@example.com"));
    }

    #[test]
    fn is_sender_allowed_empty_sender() {
        let config = EmailConfig {
            allowed_senders: vec!["@example.com".to_string()],
            ..Default::default()
        };
        let channel = EmailChannel::new(config);
        assert!(!channel.is_sender_allowed(""));
        // "@example.com" ends with "@example.com" so it's allowed
        assert!(channel.is_sender_allowed("@example.com"));
    }

    // strip_html tests

    #[test]
    fn strip_html_basic() {
        assert_eq!(EmailChannel::strip_html("<p>Hello</p>"), "Hello");
        assert_eq!(EmailChannel::strip_html("<div>World</div>"), "World");
    }

    #[test]
    fn strip_html_nested_tags() {
        assert_eq!(
            EmailChannel::strip_html("<div><p>Hello <strong>World</strong></p></div>"),
            "Hello World"
        );
    }

    #[test]
    fn strip_html_multiple_lines() {
        let html = "<div>\n  <p>Line 1</p>\n  <p>Line 2</p>\n</div>";
        assert_eq!(EmailChannel::strip_html(html), "Line 1 Line 2");
    }

    #[test]
    fn strip_html_preserves_text() {
        assert_eq!(EmailChannel::strip_html("No tags here"), "No tags here");
        assert_eq!(EmailChannel::strip_html(""), "");
    }

    #[test]
    fn strip_html_handles_malformed() {
        assert_eq!(EmailChannel::strip_html("<p>Unclosed"), "Unclosed");
        // The function removes everything between < and >, so "Text>with>brackets" becomes "Textwithbrackets"
        assert_eq!(
            EmailChannel::strip_html("Text>with>brackets"),
            "Textwithbrackets"
        );
    }

    #[test]
    fn strip_html_self_closing_tags() {
        // Self-closing tags are removed but don't add spaces
        assert_eq!(EmailChannel::strip_html("Hello<br/>World"), "HelloWorld");
        assert_eq!(EmailChannel::strip_html("Text<hr/>More"), "TextMore");
    }

    #[test]
    fn strip_html_attributes_preserved() {
        assert_eq!(
            EmailChannel::strip_html("<a href=\"http://example.com\">Link</a>"),
            "Link"
        );
    }

    #[test]
    fn strip_html_multiple_spaces_collapsed() {
        assert_eq!(
            EmailChannel::strip_html("<p>Word</p>  <p>Word</p>"),
            "Word Word"
        );
    }

    #[test]
    /// Was an assertion that entities survive stripping — it encoded the old
    /// behaviour, where the model was handed raw `&lt;` sequences to interpret.
    fn strip_html_special_characters() {
        assert_eq!(
            EmailChannel::strip_html("<span>&lt;tag&gt;</span>"),
            "<tag>"
        );
    }

    // Default function tests

    #[test]
    fn default_imap_port_returns_993() {
        assert_eq!(default_imap_port(), 993);
    }

    #[test]
    fn default_smtp_port_returns_465() {
        assert_eq!(default_smtp_port(), 465);
    }

    #[test]
    fn default_imap_folder_returns_inbox() {
        assert_eq!(default_imap_folder(), "INBOX");
    }

    #[test]
    fn default_true_returns_true() {
        assert!(default_true());
    }

    // EmailConfig serialization tests

    #[test]
    fn email_config_serialize_deserialize() {
        let config = EmailConfig {
            imap_host: "imap.example.com".to_string(),
            imap_port: 993,
            imap_folder: "INBOX".to_string(),
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            smtp_tls: true,
            username: "user@example.com".to_string(),
            password: "password123".to_string(),
            from_address: "bot@example.com".to_string(),
            idle_timeout_secs: 1740,
            allowed_senders: vec!["allowed@example.com".to_string()],
            require_authenticated_sender: false,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: EmailConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.imap_host, config.imap_host);
        assert_eq!(deserialized.smtp_port, config.smtp_port);
        assert_eq!(deserialized.allowed_senders, config.allowed_senders);
    }

    #[test]
    fn email_config_deserialize_with_defaults() {
        let json = r#"{
            "imap_host": "imap.test.com",
            "smtp_host": "smtp.test.com",
            "username": "user",
            "password": "pass",
            "from_address": "bot@test.com"
        }"#;

        let config: EmailConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.imap_port, 993); // default
        assert_eq!(config.smtp_port, 465); // default
        assert!(config.smtp_tls); // default
        assert_eq!(config.idle_timeout_secs, 1740); // default
    }

    #[test]
    fn idle_timeout_deserializes_explicit_value() {
        let json = r#"{
            "imap_host": "imap.test.com",
            "smtp_host": "smtp.test.com",
            "username": "user",
            "password": "pass",
            "from_address": "bot@test.com",
            "idle_timeout_secs": 900
        }"#;
        let config: EmailConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.idle_timeout_secs, 900);
    }

    #[test]
    fn idle_timeout_deserializes_legacy_poll_interval_alias() {
        let json = r#"{
            "imap_host": "imap.test.com",
            "smtp_host": "smtp.test.com",
            "username": "user",
            "password": "pass",
            "from_address": "bot@test.com",
            "poll_interval_secs": 120
        }"#;
        let config: EmailConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.idle_timeout_secs, 120);
    }

    #[test]
    fn idle_timeout_propagates_to_channel() {
        let config = EmailConfig {
            idle_timeout_secs: 600,
            ..Default::default()
        };
        let channel = EmailChannel::new(config);
        assert_eq!(channel.config.idle_timeout_secs, 600);
    }

    #[test]
    fn email_config_debug_output() {
        // This asserted only that a hostname round-tripped, which the derive
        // gave for free — it could not fail. What matters is the field it did
        // not look at: `Debug` rendered the mailbox password in full, and this
        // struct reaches `{:?}` on config-dump and error paths.
        let config = EmailConfig {
            imap_host: "imap.example.com".to_string(),
            username: "bot@example.com".to_string(),
            password: "placeholder-mailbox-password".to_string(),
            ..Default::default()
        };
        let debug_str = format!("{config:?}");

        assert!(
            !debug_str.contains("placeholder-mailbox-password"),
            "the password must never reach a log: {debug_str}"
        );
        assert!(
            debug_str.contains("[redacted]"),
            "the field must still be visible as redacted, not silently dropped: {debug_str}"
        );
        // The non-secret fields still have to be there, or this "fix" would be
        // a Debug impl that prints nothing useful.
        assert!(debug_str.contains("imap.example.com"));
        assert!(debug_str.contains("bot@example.com"));
    }
}
