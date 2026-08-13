use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};

// Use tokio_rustls's re-export of rustls types
use tokio_rustls::rustls;

/// Read timeout for IRC — if no data arrives within this duration, the
/// connection is considered dead. IRC servers typically PING every 60-120s.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(5);

/// Monotonic counter to ensure unique message IDs under burst traffic.
static MSG_SEQ: AtomicU64 = AtomicU64::new(0);

/// Lines written back-to-back before pacing starts.
const BURST_LINES: usize = 2;

/// Delay between paced lines — roughly two per second, the rate most networks
/// tolerate indefinitely.
const CHUNK_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// How many times to accept a `433` (nickname in use) before giving up.
/// Without a cap a server that rejects every candidate produced an unbounded
/// NICK flood, which is itself a disconnect.
const MAX_NICK_RETRIES: u8 = 3;

/// IRC over TLS channel.
///
/// Connects to an IRC server using TLS, joins configured channels,
/// and forwards PRIVMSG messages to the `RantaiClaw` message bus.
/// Supports both channel messages and private messages (DMs).
pub struct IrcChannel {
    server: String,
    port: u16,
    nickname: String,
    username: String,
    channels: Vec<String>,
    /// `Arc<RwLock<..>>` so a successful `/bind`/`/claim` can append the sender's
    /// nick at runtime (immediate access without a channel restart).
    allowed_users: Arc<std::sync::RwLock<Vec<String>>>,
    /// Nicks/accounts that may approve gated tools. Held here so the channel
    /// can refuse to hand owner authority to an unauthenticated nick, which an
    /// IRC nick always is until services vouch for it.
    approval_owners: Vec<String>,
    server_password: Option<String>,
    nickserv_password: Option<String>,
    sasl_password: Option<String>,
    verify_tls: bool,
    /// Second, explicit opt-in for the one genuinely dangerous combination:
    /// `verify_tls = false` together with a configured password.
    allow_insecure_tls_with_password: bool,
    /// Shared write half of the TLS stream for sending messages.
    writer: Arc<Mutex<Option<WriteHalf>>>,
}

/// The write half of the live session.
///
/// Boxed rather than the concrete `WriteHalf<TlsStream<TcpStream>>` so the
/// session teardown and the outbound pacing can be driven in a test against an
/// in-memory pipe. IRC writes a handful of lines per reply, so the dynamic
/// dispatch is not measurable.
type WriteHalf = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;

/// Style instruction prepended to every IRC message before it reaches the LLM.
/// IRC clients render plain text only — no markdown, no HTML, no XML.
const IRC_STYLE_PREFIX: &str = "\
[context: you are responding over IRC. \
Plain text only. No markdown, no tables, no XML/HTML tags. \
Never use triple backtick code fences. Use a single blank line to separate blocks instead. \
Be terse and concise. \
Use short lines. Avoid walls of text.]\n";

/// Reserved bytes for the server-prepended sender prefix (`:nick!user@host `).
const SENDER_PREFIX_RESERVE: usize = 64;

/// A parsed IRC message.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IrcMessage {
    /// IRCv3 message tags (`@account=alice;time=… `). Empty on a server that
    /// does not send them, or when the capability was never negotiated.
    tags: Vec<(String, String)>,
    prefix: Option<String>,
    command: String,
    params: Vec<String>,
}

/// Decode an IRCv3 tag value: `\:` is `;`, `\s` is a space, `\\` is a
/// backslash, `\r`/`\n` are the line terminators. An unknown escape yields the
/// escaped character itself, and a trailing lone `\` is dropped — both per the
/// IRCv3 message-tags spec.
fn unescape_tag_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some(':') => out.push(';'),
            Some('s') => out.push(' '),
            Some('\\') => out.push('\\'),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn parse_tags(raw: &str) -> Vec<(String, String)> {
    raw.split(';')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (key, value) = kv.split_once('=').unwrap_or((kv, ""));
            (key.to_string(), unescape_tag_value(value))
        })
        .collect()
}

impl IrcMessage {
    /// Parse a raw IRC line into an `IrcMessage`.
    ///
    /// IRC format: `[@<tags>] [:<prefix>] <command> [<params>] [:<trailing>]`
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return None;
        }

        // Tags come first and are the only part introduced by `@`. Without
        // this branch the whole tag blob was read as the command, so every
        // tagged message — which is every message once a capability is
        // negotiated — parsed as garbage.
        let (tags, line) = match line.strip_prefix('@') {
            Some(stripped) => {
                let space = stripped.find(' ')?;
                (parse_tags(&stripped[..space]), &stripped[space + 1..])
            }
            None => (Vec::new(), line),
        };

        let (prefix, rest) = if let Some(stripped) = line.strip_prefix(':') {
            let space = stripped.find(' ')?;
            (Some(stripped[..space].to_string()), &stripped[space + 1..])
        } else {
            (None, line)
        };

        // Split at trailing (first `:` after command/params)
        let (params_part, trailing) = if let Some(colon_pos) = rest.find(" :") {
            (&rest[..colon_pos], Some(&rest[colon_pos + 2..]))
        } else {
            (rest, None)
        };

        let mut parts: Vec<&str> = params_part.split_whitespace().collect();
        if parts.is_empty() {
            return None;
        }

        let command = parts.remove(0).to_uppercase();
        let mut params: Vec<String> = parts.iter().map(std::string::ToString::to_string).collect();
        if let Some(t) = trailing {
            params.push(t.to_string());
        }

        Some(IrcMessage {
            tags,
            prefix,
            command,
            params,
        })
    }

    /// The value of one message tag, if the server sent it.
    fn tag(&self, name: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The authenticated services account behind this message, if any.
    ///
    /// `*` is the spec's "not logged in" marker and must not survive: it is
    /// also the wildcard the owner gate reads as "anybody", so letting it
    /// through would turn a logged-out user into every owner at once.
    fn account(&self) -> Option<&str> {
        self.tag("account")
            .map(str::trim)
            .filter(|a| !a.is_empty() && *a != "*")
    }

    /// Extract the nickname from the prefix (nick!user@host → nick).
    fn nick(&self) -> Option<&str> {
        self.prefix.as_ref().and_then(|p| {
            let end = p.find('!').unwrap_or(p.len());
            let nick = &p[..end];
            if nick.is_empty() {
                None
            } else {
                Some(nick)
            }
        })
    }
}

/// Encode SASL PLAIN credentials: base64(\0nick\0password).
fn encode_sasl_plain(nick: &str, password: &str) -> String {
    // Simple base64 encoder — avoids adding a base64 crate dependency.
    // The project's Discord channel uses a similar inline approach.
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let input = format!("\0{nick}\0{password}");
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(CHARS[(triple >> 18 & 0x3F) as usize] as char);
        out.push(CHARS[(triple >> 12 & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            out.push(CHARS[(triple >> 6 & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }

        if chunk.len() > 2 {
            out.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }

    out
}

/// Split a message into lines safe for IRC transmission.
///
/// IRC is a line-based protocol — `\r\n` terminates each command, so any
/// newline inside a PRIVMSG payload would truncate the message and turn the
/// remainder into garbled/invalid IRC commands.
///
/// This function:
/// 1. Splits on `\n` (and strips `\r`) so each logical line becomes its own PRIVMSG.
/// 2. Splits any line that exceeds `max_bytes` at a safe UTF-8 boundary.
/// 3. Skips empty lines to avoid sending blank PRIVMSGs.
fn split_message(message: &str, max_bytes: usize) -> Vec<String> {
    let mut chunks = Vec::new();

    // Guard against max_bytes == 0 to prevent infinite loop
    if max_bytes == 0 {
        let mut full = String::new();
        for l in message
            .lines()
            .map(|l| l.trim_end_matches('\r'))
            .filter(|l| !l.is_empty())
        {
            if !full.is_empty() {
                full.push(' ');
            }
            full.push_str(l);
        }
        if full.is_empty() {
            chunks.push(String::new());
        } else {
            chunks.push(full);
        }
        return chunks;
    }

    for line in message.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }

        if line.len() <= max_bytes {
            chunks.push(line.to_string());
            continue;
        }

        // Line exceeds max_bytes — split at safe UTF-8 boundaries
        let mut remaining = line;
        while !remaining.is_empty() {
            if remaining.len() <= max_bytes {
                chunks.push(remaining.to_string());
                break;
            }

            let mut split_at = max_bytes;
            while split_at > 0 && !remaining.is_char_boundary(split_at) {
                split_at -= 1;
            }
            if split_at == 0 {
                // No valid boundary found going backward — advance forward instead
                split_at = max_bytes;
                while split_at < remaining.len() && !remaining.is_char_boundary(split_at) {
                    split_at += 1;
                }
            }

            chunks.push(remaining[..split_at].to_string());
            remaining = &remaining[split_at..];
        }
    }

    if chunks.is_empty() {
        chunks.push(String::new());
    }

    chunks
}

/// Configuration for constructing an `IrcChannel`.
pub struct IrcChannelConfig {
    pub server: String,
    pub port: u16,
    pub nickname: String,
    pub username: Option<String>,
    pub channels: Vec<String>,
    pub allowed_users: Vec<String>,
    pub server_password: Option<String>,
    pub nickserv_password: Option<String>,
    pub sasl_password: Option<String>,
    pub verify_tls: bool,
    pub allow_insecure_tls_with_password: bool,
    pub approval_owners: Vec<String>,
}

impl IrcChannel {
    pub fn new(cfg: IrcChannelConfig) -> Self {
        let username = cfg.username.unwrap_or_else(|| cfg.nickname.clone());
        Self {
            server: cfg.server,
            port: cfg.port,
            nickname: cfg.nickname,
            username,
            channels: cfg.channels,
            allowed_users: Arc::new(std::sync::RwLock::new(cfg.allowed_users)),
            approval_owners: cfg.approval_owners,
            server_password: cfg.server_password,
            nickserv_password: cfg.nickserv_password,
            sasl_password: cfg.sasl_password,
            verify_tls: cfg.verify_tls,
            allow_insecure_tls_with_password: cfg.allow_insecure_tls_with_password,
            writer: Arc::new(Mutex::new(None)),
        }
    }

    /// Refuse the one combination that discloses a credential: no peer
    /// authentication plus a password on the wire. SASL PLAIN is reversible
    /// base64, and NickServ IDENTIFY is plaintext, so an unauthenticated link
    /// hands both to whoever answered the connection.
    ///
    /// Returns the error the caller should fail to start with.
    fn insecure_credential_refusal(&self) -> Option<String> {
        if self.verify_tls || self.allow_insecure_tls_with_password {
            return None;
        }
        let configured: Vec<&str> = [
            ("sasl_password", self.sasl_password.as_ref()),
            ("nickserv_password", self.nickserv_password.as_ref()),
            ("server_password", self.server_password.as_ref()),
        ]
        .into_iter()
        .filter(|(_, v)| v.is_some_and(|s| !s.trim().is_empty()))
        .map(|(name, _)| name)
        .collect();
        if configured.is_empty() {
            return None;
        }
        Some(format!(
            "IRC refuses to start: verify_tls = false accepts any certificate for {}, and {} would be sent over that link. \
             Set verify_tls = true, or — only if you understand that the credential is disclosed to whoever answers — \
             set allow_insecure_tls_with_password = true. Rotate any credential already used over such a link.",
            self.server,
            configured.join(", ")
        ))
    }

    /// The identity forms for an inbound message, or `None` when the message
    /// must be dropped.
    ///
    /// An IRC nick is a first-come lease, not an identity: anyone who connects
    /// while the owner is offline — or forces them off with a ghost or a
    /// netsplit — holds it. So the services account, when the network supplies
    /// one, is the primary identity and the nick is demoted to an alias. With
    /// no account tag the nick still serves the chat allowlist, but a nick
    /// listed in `approval_owners` is refused outright rather than granted that
    /// authority on the strength of a name.
    ///
    /// A literal `*` in `approval_owners` is the operator switching the owner
    /// gate off; honour that rather than dropping every message on the network.
    fn resolve_identity(&self, account: Option<&str>, nick: &str) -> Option<String> {
        if let Some(account) = account {
            // The nick is deliberately NOT carried as an alias. Aliases are
            // matched by the owner gate, so passing the nick along would let
            // whoever holds it borrow an owner entry recorded under that name
            // while authenticated as somebody else entirely.
            return Some(account.to_string());
        }
        if self.approval_owners.iter().any(|o| o.trim() == "*") {
            return Some(nick.to_string());
        }
        let claims_owner = self
            .approval_owners
            .iter()
            .any(|o| o.trim().trim_start_matches('@').eq_ignore_ascii_case(nick));
        if claims_owner {
            tracing::warn!(
                "IRC: dropped a message from {nick}, which is listed in approval_owners, \
                 because the server sent no account tag. An IRC nick is a lease, not an \
                 identity. Identify to services on a network that offers the account-tag \
                 capability to use owner authority."
            );
            return None;
        }
        Some(nick.to_string())
    }

    fn is_user_allowed(&self, nick: &str) -> bool {
        let Ok(users) = self.allowed_users.read() else {
            return false;
        };
        if users.iter().any(|u| u == "*") {
            return true;
        }
        users.iter().any(|u| u.eq_ignore_ascii_case(nick))
    }

    /// Append a freshly-paired nick to the runtime allowlist (case-insensitive
    /// dedupe) so access is effective immediately. The persisted config (saved
    /// by the pairing core) is the source of truth across restarts.
    fn add_allowed_identity_runtime(&self, nick: &str) {
        let nick = nick.trim();
        if nick.is_empty() {
            return;
        }
        if let Ok(mut users) = self.allowed_users.write() {
            if !users.iter().any(|u| u.eq_ignore_ascii_case(nick)) {
                users.push(nick.to_string());
            }
        }
    }

    /// Create a TLS connection to the IRC server.
    async fn connect(
        &self,
    ) -> anyhow::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
        let addr = format!("{}:{}", self.server, self.port);
        let tcp = tokio::net::TcpStream::connect(&addr).await?;

        let tls_config = if self.verify_tls {
            let root_store: rustls::RootCertStore =
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect();
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        } else {
            tracing::warn!(
                "IRC: TLS certificate verification is DISABLED for {}:{} — any host that answers \
                 this connection is trusted, including one that intercepted it.",
                self.server,
                self.port
            );
            rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth()
        };

        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
        let domain = rustls::pki_types::ServerName::try_from(self.server.clone())?;
        let tls = connector.connect(domain, tcp).await?;

        Ok(tls)
    }

    /// Capability names from a `CAP LS`/`ACK` list, dropping any `=value`
    /// suffix (`sasl=PLAIN` is the capability `sasl`).
    fn parse_cap_list(listed: &str) -> Vec<String> {
        listed
            .split_whitespace()
            .map(|cap| cap.split('=').next().unwrap_or(cap).to_string())
            .collect()
    }

    /// The capabilities to request: what this channel needs, intersected with
    /// what the server offered.
    ///
    /// `account-tag` is the whole point — it is the only per-message evidence
    /// that a nick belongs to the account it claims. `extended-join` carries
    /// the same account on JOIN.
    fn caps_to_request(offered: &[String], want_sasl: bool) -> Vec<&'static str> {
        let mut wanted = vec!["account-tag", "extended-join"];
        if want_sasl {
            wanted.push("sasl");
        }
        wanted
            .into_iter()
            .filter(|cap| offered.iter().any(|o| o == cap))
            .collect()
    }

    /// The nick to try after a `433`, or `None` once the cap is reached.
    ///
    /// Uncapped, a server that rejects every candidate produced an unbounded
    /// NICK flood — which is itself a disconnect, so the loop could never
    /// succeed and never stopped trying.
    fn next_nick_candidate(current: &str, retries: u8) -> Option<String> {
        (retries < MAX_NICK_RETRIES).then(|| format!("{current}_"))
    }

    /// How long to wait before writing chunk `index`.
    ///
    /// Chunks used to go out back-to-back, which most networks disconnect as
    /// excess flood — so a long reply failed more reliably than a short one.
    /// A short burst is allowed (networks tolerate a few lines), then roughly
    /// two lines per second.
    fn pace_before(index: usize) -> Option<std::time::Duration> {
        (index >= BURST_LINES).then_some(CHUNK_DELAY)
    }

    /// Send a raw IRC line (appends \r\n).
    async fn send_raw(writer: &mut WriteHalf, line: &str) -> anyhow::Result<()> {
        let data = format!("{line}\r\n");
        writer.write_all(data.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }
}

/// Certificate verifier that accepts any certificate (for `verify_tls=false`).
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[async_trait]
#[allow(clippy::too_many_lines)]
impl Channel for IrcChannel {
    fn name(&self) -> &str {
        "irc"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let mut guard = self.writer.lock().await;
        let writer = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("IRC not connected"))?;

        // IRC renders no markup — strip to readable text, THEN feed the rendered
        // string into IRC's own per-line PRIVMSG splitter (keep it: it enforces
        // the 512-byte line limit, which format::split does not know about).
        // Plain's four-space code indent (no fences) matches IRC style.
        let rendered = crate::channels::format::render_to_string(
            &message.content,
            &crate::channels::format::RenderTarget::Plain,
        );

        // Calculate safe payload size:
        // 512 - sender prefix (~64 bytes for :nick!user@host) - "PRIVMSG " - target - " :" - "\r\n"
        let overhead = SENDER_PREFIX_RESERVE + 10 + message.recipient.len() + 2;
        let max_payload = 512_usize.saturating_sub(overhead);
        let chunks = split_message(&rendered, max_payload);

        for (index, chunk) in chunks.iter().enumerate() {
            if let Some(delay) = Self::pace_before(index) {
                tokio::time::sleep(delay).await;
            }
            Self::send_raw(writer, &format!("PRIVMSG {} :{chunk}", message.recipient)).await?;
        }

        Ok(())
    }

    fn apply_allowed_senders(&self, allowed: &[String]) {
        if let Ok(mut users) = self.allowed_users.write() {
            *users = allowed.to_vec();
        }
    }

    async fn listen(
        &self,
        tx: mpsc::Sender<ChannelMessage>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        // Every exit from the session — the read timeout, the `n == 0` bail,
        // any `?` — used to leave the write half in place. `send()` then wrote
        // into a half-closed socket, returned `Ok(())`, and the reply vanished
        // with no error anywhere. Clearing it here routes into the existing
        // "IRC not connected" path.
        let result = self.run_session(tx, cancel).await;
        *self.writer.lock().await = None;
        result
    }

    async fn health_check(&self) -> bool {
        // Report on the live session. Dialling a fresh TCP+TLS connection
        // every heartbeat is the single most reliable way to get a bot
        // K-lined, and it says nothing about whether the *session* works.
        self.writer.lock().await.is_some()
    }
}

impl IrcChannel {
    async fn run_session(
        &self,
        tx: mpsc::Sender<ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        if let Some(refusal) = self.insecure_credential_refusal() {
            anyhow::bail!(refusal);
        }

        let mut current_nick = self.nickname.clone();
        tracing::info!(
            "IRC channel connecting to {}:{} as {}...",
            self.server,
            self.port,
            current_nick
        );

        let tls = self.connect().await?;
        let (reader, writer) = tokio::io::split(tls);
        let mut writer: WriteHalf = Box::new(writer);

        // --- Capability negotiation ---
        // `CAP LS` first so the REQ only asks for what the server offers: a
        // REQ is atomic, so bundling `sasl` with `account-tag` on a server
        // that lacks one would lose both. A server with no CAP support ignores
        // this line and registration proceeds unchanged.
        Self::send_raw(&mut writer, "CAP LS 302").await?;

        // --- Server password ---
        if let Some(ref pass) = self.server_password {
            Self::send_raw(&mut writer, &format!("PASS {pass}")).await?;
        }

        // --- Nick/User registration ---
        Self::send_raw(&mut writer, &format!("NICK {current_nick}")).await?;
        Self::send_raw(
            &mut writer,
            &format!("USER {} 0 * :RantaiClaw", self.username),
        )
        .await?;

        // Store writer for send()
        {
            let mut guard = self.writer.lock().await;
            *guard = Some(writer);
        }

        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        let mut registered = false;
        let mut sasl_pending = false;
        let mut offered_caps: Vec<String> = Vec::new();
        let mut nick_retries = 0_u8;

        loop {
            line.clear();
            let n = tokio::time::timeout(READ_TIMEOUT, buf_reader.read_line(&mut line))
                .await
                .map_err(|_| {
                    anyhow::anyhow!("IRC read timed out (no data for {READ_TIMEOUT:?})")
                })??;
            if n == 0 {
                anyhow::bail!("IRC connection closed by server");
            }

            let Some(msg) = IrcMessage::parse(&line) else {
                continue;
            };

            match msg.command.as_str() {
                "PING" => {
                    let token = msg.params.first().map_or("", String::as_str);
                    let mut guard = self.writer.lock().await;
                    if let Some(ref mut w) = *guard {
                        Self::send_raw(w, &format!("PONG :{token}")).await?;
                    }
                }

                // Capability negotiation: `CAP <target> <sub> [*] :<caps>`.
                "CAP" => {
                    let sub = msg.params.get(1).map_or("", String::as_str);
                    // A `*` in the third position means the list continues in
                    // a further CAP line; only the final one may be answered.
                    let continues = msg.params.get(2).is_some_and(|p| p == "*");
                    let listed = msg.params.last().map_or("", String::as_str);

                    match sub {
                        "LS" => {
                            offered_caps.extend(Self::parse_cap_list(listed));
                            if !continues {
                                let request = Self::caps_to_request(
                                    &offered_caps,
                                    self.sasl_password.is_some(),
                                );
                                let mut guard = self.writer.lock().await;
                                if let Some(ref mut w) = *guard {
                                    if request.is_empty() {
                                        Self::send_raw(w, "CAP END").await?;
                                    } else {
                                        Self::send_raw(
                                            w,
                                            &format!("CAP REQ :{}", request.join(" ")),
                                        )
                                        .await?;
                                    }
                                }
                            }
                        }
                        "ACK" => {
                            let acked = Self::parse_cap_list(listed);
                            tracing::debug!("IRC capabilities acknowledged: {}", acked.join(" "));
                            let mut guard = self.writer.lock().await;
                            if let Some(ref mut w) = *guard {
                                if acked.iter().any(|c| c == "sasl") {
                                    sasl_pending = true;
                                    Self::send_raw(w, "AUTHENTICATE PLAIN").await?;
                                } else {
                                    Self::send_raw(w, "CAP END").await?;
                                }
                            }
                        }
                        "NAK" => {
                            tracing::warn!("IRC server refused capabilities: {listed}");
                            let mut guard = self.writer.lock().await;
                            if let Some(ref mut w) = *guard {
                                Self::send_raw(w, "CAP END").await?;
                            }
                        }
                        _ => {}
                    }
                }

                "AUTHENTICATE"
                    // Server sends "AUTHENTICATE +" to request credentials
                    if sasl_pending && msg.params.first().is_some_and(|p| p == "+") => {
                        // sasl_password is loaded from runtime config, not hard-coded
                        if let Some(password) = self.sasl_password.as_deref() {
                            let encoded = encode_sasl_plain(&current_nick, password);
                            let mut guard = self.writer.lock().await;
                            if let Some(ref mut w) = *guard {
                                Self::send_raw(w, &format!("AUTHENTICATE {encoded}")).await?;
                            }
                        } else {
                            // SASL was requested but no password is configured; abort SASL
                            tracing::warn!(
                                "SASL authentication requested but no SASL password is configured; aborting SASL"
                            );
                            sasl_pending = false;
                            let mut guard = self.writer.lock().await;
                            if let Some(ref mut w) = *guard {
                                Self::send_raw(w, "CAP END").await?;
                            }
                        }
                    }

                // RPL_SASLSUCCESS (903) — SASL done, end CAP
                "903" => {
                    sasl_pending = false;
                    let mut guard = self.writer.lock().await;
                    if let Some(ref mut w) = *guard {
                        Self::send_raw(w, "CAP END").await?;
                    }
                }

                // SASL failure (904, 905, 906, 907)
                "904" | "905" | "906" | "907" => {
                    tracing::warn!("IRC SASL authentication failed ({})", msg.command);
                    sasl_pending = false;
                    let mut guard = self.writer.lock().await;
                    if let Some(ref mut w) = *guard {
                        Self::send_raw(w, "CAP END").await?;
                    }
                }

                // RPL_WELCOME — registration complete
                "001" => {
                    registered = true;
                    tracing::info!("IRC registered as {}", current_nick);

                    // NickServ authentication
                    if let Some(ref pass) = self.nickserv_password {
                        let mut guard = self.writer.lock().await;
                        if let Some(ref mut w) = *guard {
                            Self::send_raw(w, &format!("PRIVMSG NickServ :IDENTIFY {pass}"))
                                .await?;
                        }
                    }

                    // Join channels
                    for chan in &self.channels {
                        let mut guard = self.writer.lock().await;
                        if let Some(ref mut w) = *guard {
                            Self::send_raw(w, &format!("JOIN {chan}")).await?;
                        }
                    }
                }

                // ERR_NICKNAMEINUSE (433)
                "433" => {
                    let Some(alt) = Self::next_nick_candidate(&current_nick, nick_retries) else {
                        anyhow::bail!(
                            "IRC: {MAX_NICK_RETRIES} nickname candidates were all in use \
                             (last: {current_nick}); giving up rather than flooding NICK"
                        );
                    };
                    nick_retries += 1;
                    tracing::warn!("IRC nickname {current_nick} is in use, trying {alt}");
                    let mut guard = self.writer.lock().await;
                    if let Some(ref mut w) = *guard {
                        Self::send_raw(w, &format!("NICK {alt}")).await?;
                    }
                    current_nick = alt;
                }

                "PRIVMSG" => {
                    if !registered {
                        continue;
                    }

                    let target = msg.params.first().map_or("", String::as_str);
                    let text = msg.params.get(1).map_or("", String::as_str);
                    let sender_nick = msg.nick().unwrap_or("unknown");
                    let account = msg.account();

                    // Skip messages from NickServ/ChanServ
                    if sender_nick.eq_ignore_ascii_case("NickServ")
                        || sender_nick.eq_ignore_ascii_case("ChanServ")
                    {
                        continue;
                    }

                    // Determine reply target: if sent to a channel, reply to channel;
                    // if DM (target == our nick), reply to sender
                    let is_channel = target.starts_with('#') || target.starts_with('&');

                    if !self.is_user_allowed(sender_nick) {
                        // Before rejecting, let a not-yet-allowed nick self-onboard
                        // with a `/bind`/`/claim <code>` minted via
                        // `rantaiclaw channels pair`. On success the nick lands in
                        // `allowed_users` (and, for an owner `/claim`, `approval_owners`).
                        if let Some(root) = crate::channels::pairing::profile_root("irc") {
                            // Pair the account when the network vouches for
                            // one — `approval_owners` on IRC holds services
                            // account names, not nicks.
                            if account.is_none() {
                                tracing::warn!(
                                    "IRC pairing from {sender_nick} with no account tag: \
                                     the nick will be added to the chat allowlist, but owner \
                                     authority cannot be granted to a nick."
                                );
                            }
                            let identities = vec![account.unwrap_or(sender_nick).to_string()];
                            if let Some(reply) = crate::channels::pairing::try_handle_pairing(
                                text,
                                "irc",
                                crate::channels::pairing::AllowlistField::AllowedUsers,
                                &identities,
                                &root,
                            )
                            .await
                            {
                                self.add_allowed_identity_runtime(sender_nick);
                                let pair_reply_target = if is_channel {
                                    target.to_string()
                                } else {
                                    sender_nick.to_string()
                                };
                                let _ =
                                    self.send(&SendMessage::new(reply, pair_reply_target)).await;
                                continue;
                            }
                        }
                        continue;
                    }

                    let reply_target = if is_channel {
                        target.to_string()
                    } else {
                        sender_nick.to_string()
                    };
                    let content = if is_channel {
                        format!("{IRC_STYLE_PREFIX}<{sender_nick}> {text}")
                    } else {
                        format!("{IRC_STYLE_PREFIX}{text}")
                    };

                    let Some(sender) = self.resolve_identity(account, sender_nick) else {
                        // Refused above, with the reason logged.
                        continue;
                    };

                    let seq = MSG_SEQ.fetch_add(1, Ordering::Relaxed);
                    let channel_msg = ChannelMessage { sender_aliases: Vec::new(),
                        id: format!("irc_{}_{seq}", chrono::Utc::now().timestamp_millis()),
                        sender,
                        reply_target,
                        content,
                        channel: "irc".to_string(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        thread_ts: None,
                    };

                    if tx.send(channel_msg).await.is_err() {
                        return Ok(());
                    }
                }

                // ERR_PASSWDMISMATCH (464) or other fatal errors
                "464" => {
                    anyhow::bail!("IRC password mismatch");
                }

                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── IRC message parsing ──────────────────────────────────

    #[test]
    fn parse_privmsg_with_prefix() {
        let msg = IrcMessage::parse(":nick!user@host PRIVMSG #channel :Hello world").unwrap();
        assert_eq!(msg.prefix.as_deref(), Some("nick!user@host"));
        assert_eq!(msg.command, "PRIVMSG");
        assert_eq!(msg.params, vec!["#channel", "Hello world"]);
    }

    #[test]
    fn parse_privmsg_dm() {
        let msg = IrcMessage::parse(":alice!a@host PRIVMSG botname :hi there").unwrap();
        assert_eq!(msg.command, "PRIVMSG");
        assert_eq!(msg.params, vec!["botname", "hi there"]);
        assert_eq!(msg.nick(), Some("alice"));
    }

    #[test]
    fn parse_ping() {
        let msg = IrcMessage::parse("PING :server.example.com").unwrap();
        assert!(msg.prefix.is_none());
        assert_eq!(msg.command, "PING");
        assert_eq!(msg.params, vec!["server.example.com"]);
    }

    #[test]
    fn parse_numeric_reply() {
        let msg = IrcMessage::parse(":server 001 botname :Welcome to the IRC network").unwrap();
        assert_eq!(msg.prefix.as_deref(), Some("server"));
        assert_eq!(msg.command, "001");
        assert_eq!(msg.params, vec!["botname", "Welcome to the IRC network"]);
    }

    #[test]
    fn parse_no_trailing() {
        let msg = IrcMessage::parse(":server 433 * botname").unwrap();
        assert_eq!(msg.command, "433");
        assert_eq!(msg.params, vec!["*", "botname"]);
    }

    #[test]
    fn parse_cap_ack() {
        let msg = IrcMessage::parse(":server CAP * ACK :sasl").unwrap();
        assert_eq!(msg.command, "CAP");
        assert_eq!(msg.params, vec!["*", "ACK", "sasl"]);
    }

    #[test]
    fn parse_empty_line_returns_none() {
        assert!(IrcMessage::parse("").is_none());
        assert!(IrcMessage::parse("\r\n").is_none());
    }

    #[test]
    fn parse_strips_crlf() {
        let msg = IrcMessage::parse("PING :test\r\n").unwrap();
        assert_eq!(msg.params, vec!["test"]);
    }

    #[test]
    fn parse_command_uppercase() {
        let msg = IrcMessage::parse("ping :test").unwrap();
        assert_eq!(msg.command, "PING");
    }

    #[test]
    fn nick_extraction_full_prefix() {
        let msg = IrcMessage::parse(":nick!user@host PRIVMSG #ch :msg").unwrap();
        assert_eq!(msg.nick(), Some("nick"));
    }

    #[test]
    fn nick_extraction_nick_only() {
        let msg = IrcMessage::parse(":server 001 bot :Welcome").unwrap();
        assert_eq!(msg.nick(), Some("server"));
    }

    #[test]
    fn nick_extraction_no_prefix() {
        let msg = IrcMessage::parse("PING :token").unwrap();
        assert_eq!(msg.nick(), None);
    }

    #[test]
    fn parse_authenticate_plus() {
        let msg = IrcMessage::parse("AUTHENTICATE +").unwrap();
        assert_eq!(msg.command, "AUTHENTICATE");
        assert_eq!(msg.params, vec!["+"]);
    }

    // ── SASL PLAIN encoding ─────────────────────────────────

    #[test]
    fn sasl_plain_encode() {
        let encoded = encode_sasl_plain("jilles", "sesame");
        // \0jilles\0sesame → base64
        assert_eq!(encoded, "AGppbGxlcwBzZXNhbWU=");
    }

    #[test]
    fn sasl_plain_empty_password() {
        let encoded = encode_sasl_plain("nick", "");
        // \0nick\0 → base64
        assert_eq!(encoded, "AG5pY2sA");
    }

    // ── Message splitting ───────────────────────────────────

    #[test]
    fn split_short_message() {
        let chunks = split_message("hello", 400);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_long_message() {
        let msg = "a".repeat(800);
        let chunks = split_message(&msg, 400);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 400);
        assert_eq!(chunks[1].len(), 400);
    }

    #[test]
    fn split_exact_boundary() {
        let msg = "a".repeat(400);
        let chunks = split_message(&msg, 400);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn split_unicode_safe() {
        // 'é' is 2 bytes in UTF-8; splitting at byte 3 would split mid-char
        let msg = "ééé"; // 6 bytes
        let chunks = split_message(msg, 3);
        // Should split at char boundary (2 bytes), not mid-char
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], "é");
        assert_eq!(chunks[1], "é");
        assert_eq!(chunks[2], "é");
    }

    #[test]
    fn split_empty_message() {
        let chunks = split_message("", 400);
        assert_eq!(chunks, vec![""]);
    }

    #[test]
    fn split_newlines_into_separate_lines() {
        let chunks = split_message("line one\nline two\nline three", 400);
        assert_eq!(chunks, vec!["line one", "line two", "line three"]);
    }

    #[test]
    fn split_crlf_newlines() {
        let chunks = split_message("hello\r\nworld", 400);
        assert_eq!(chunks, vec!["hello", "world"]);
    }

    #[test]
    fn split_skips_empty_lines() {
        let chunks = split_message("hello\n\n\nworld", 400);
        assert_eq!(chunks, vec!["hello", "world"]);
    }

    #[test]
    fn split_trailing_newline() {
        let chunks = split_message("hello\n", 400);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_multiline_with_long_line() {
        let long = "a".repeat(800);
        let msg = format!("short\n{long}\nend");
        let chunks = split_message(&msg, 400);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0], "short");
        assert_eq!(chunks[1].len(), 400);
        assert_eq!(chunks[2].len(), 400);
        assert_eq!(chunks[3], "end");
    }

    #[test]
    fn split_only_newlines() {
        let chunks = split_message("\n\n\n", 400);
        assert_eq!(chunks, vec![""]);
    }

    // ── Allowlist ───────────────────────────────────────────

    #[test]
    fn wildcard_allows_anyone() {
        let ch = make_channel();
        // Default make_channel has wildcard
        assert!(ch.is_user_allowed("anyone"));
        assert!(ch.is_user_allowed("stranger"));
    }

    #[test]
    fn specific_user_allowed() {
        let ch = IrcChannel::new(IrcChannelConfig {
            server: "irc.test".into(),
            port: 6697,
            nickname: "bot".into(),
            username: None,
            channels: vec![],
            allowed_users: vec!["alice".into(), "bob".into()],
            server_password: None,
            nickserv_password: None,
            sasl_password: None,
            verify_tls: true,
            allow_insecure_tls_with_password: false,
            approval_owners: Vec::new(),
        });
        assert!(ch.is_user_allowed("alice"));
        assert!(ch.is_user_allowed("bob"));
        assert!(!ch.is_user_allowed("eve"));
    }

    #[test]
    fn allowlist_case_insensitive() {
        let ch = IrcChannel::new(IrcChannelConfig {
            server: "irc.test".into(),
            port: 6697,
            nickname: "bot".into(),
            username: None,
            channels: vec![],
            allowed_users: vec!["Alice".into()],
            server_password: None,
            nickserv_password: None,
            sasl_password: None,
            verify_tls: true,
            allow_insecure_tls_with_password: false,
            approval_owners: Vec::new(),
        });
        assert!(ch.is_user_allowed("alice"));
        assert!(ch.is_user_allowed("ALICE"));
        assert!(ch.is_user_allowed("Alice"));
    }

    #[test]
    fn empty_allowlist_denies_all() {
        let ch = IrcChannel::new(IrcChannelConfig {
            server: "irc.test".into(),
            port: 6697,
            nickname: "bot".into(),
            username: None,
            channels: vec![],
            allowed_users: vec![],
            server_password: None,
            nickserv_password: None,
            sasl_password: None,
            verify_tls: true,
            allow_insecure_tls_with_password: false,
            approval_owners: Vec::new(),
        });
        assert!(!ch.is_user_allowed("anyone"));
    }

    // ── Constructor ─────────────────────────────────────────

    #[test]
    fn new_defaults_username_to_nickname() {
        let ch = IrcChannel::new(IrcChannelConfig {
            server: "irc.test".into(),
            port: 6697,
            nickname: "mybot".into(),
            username: None,
            channels: vec![],
            allowed_users: vec![],
            server_password: None,
            nickserv_password: None,
            sasl_password: None,
            verify_tls: true,
            allow_insecure_tls_with_password: false,
            approval_owners: Vec::new(),
        });
        assert_eq!(ch.username, "mybot");
    }

    #[test]
    fn new_uses_explicit_username() {
        let ch = IrcChannel::new(IrcChannelConfig {
            server: "irc.test".into(),
            port: 6697,
            nickname: "mybot".into(),
            username: Some("customuser".into()),
            channels: vec![],
            allowed_users: vec![],
            server_password: None,
            nickserv_password: None,
            sasl_password: None,
            verify_tls: true,
            allow_insecure_tls_with_password: false,
            approval_owners: Vec::new(),
        });
        assert_eq!(ch.username, "customuser");
        assert_eq!(ch.nickname, "mybot");
    }

    #[test]
    fn name_returns_irc() {
        let ch = make_channel();
        assert_eq!(ch.name(), "irc");
    }

    #[test]
    fn new_stores_all_fields() {
        let ch = IrcChannel::new(IrcChannelConfig {
            server: "irc.example.com".into(),
            port: 6697,
            nickname: "zcbot".into(),
            username: Some("rantaiclaw".into()),
            channels: vec!["#test".into()],
            allowed_users: vec!["alice".into()],
            server_password: Some("serverpass".into()),
            nickserv_password: Some("nspass".into()),
            sasl_password: Some("saslpass".into()),
            verify_tls: false,
            allow_insecure_tls_with_password: true,
            approval_owners: Vec::new(),
        });
        assert_eq!(ch.server, "irc.example.com");
        assert_eq!(ch.port, 6697);
        assert_eq!(ch.nickname, "zcbot");
        assert_eq!(ch.username, "rantaiclaw");
        assert_eq!(ch.channels, vec!["#test"]);
        assert_eq!(*ch.allowed_users.read().unwrap(), vec!["alice".to_string()]);
        assert_eq!(ch.server_password.as_deref(), Some("serverpass"));
        assert_eq!(ch.nickserv_password.as_deref(), Some("nspass"));
        assert_eq!(ch.sasl_password.as_deref(), Some("saslpass"));
        assert!(!ch.verify_tls);
    }

    // ── Config serde ────────────────────────────────────────

    #[test]
    fn irc_config_serde_roundtrip() {
        use crate::config::schema::IrcConfig;

        let config = IrcConfig {
            server: "irc.example.com".into(),
            port: 6697,
            nickname: "zcbot".into(),
            username: Some("rantaiclaw".into()),
            channels: vec!["#test".into(), "#dev".into()],
            allowed_users: vec!["alice".into()],
            server_password: None,
            nickserv_password: Some("secret".into()),
            sasl_password: None,
            verify_tls: Some(true),
            allow_insecure_tls_with_password: false,
        };

        let toml_str = toml::to_string(&config).unwrap();
        let parsed: IrcConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.server, "irc.example.com");
        assert_eq!(parsed.port, 6697);
        assert_eq!(parsed.nickname, "zcbot");
        assert_eq!(parsed.username.as_deref(), Some("rantaiclaw"));
        assert_eq!(parsed.channels, vec!["#test", "#dev"]);
        assert_eq!(parsed.allowed_users, vec!["alice"]);
        assert!(parsed.server_password.is_none());
        assert_eq!(parsed.nickserv_password.as_deref(), Some("secret"));
        assert!(parsed.sasl_password.is_none());
        assert_eq!(parsed.verify_tls, Some(true));
    }

    #[test]
    fn irc_config_minimal_toml() {
        use crate::config::schema::IrcConfig;

        let toml_str = r#"
server = "irc.example.com"
nickname = "bot"
"#;
        let parsed: IrcConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.server, "irc.example.com");
        assert_eq!(parsed.port, 6697); // default
        assert_eq!(parsed.nickname, "bot");
        assert!(parsed.username.is_none());
        assert!(parsed.channels.is_empty());
        assert!(parsed.allowed_users.is_empty());
        assert!(parsed.server_password.is_none());
        assert!(parsed.nickserv_password.is_none());
        assert!(parsed.sasl_password.is_none());
        assert!(parsed.verify_tls.is_none());
    }

    #[test]
    fn irc_config_default_port() {
        use crate::config::schema::IrcConfig;

        let json = r#"{"server":"irc.test","nickname":"bot"}"#;
        let parsed: IrcConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.port, 6697);
    }

    // ── pairing (/bind, /claim) ──────────────────────────────

    fn empty_allowlist_channel() -> IrcChannel {
        IrcChannel::new(IrcChannelConfig {
            server: "irc.test".into(),
            port: 6697,
            nickname: "bot".into(),
            username: None,
            channels: vec![],
            allowed_users: vec![],
            server_password: None,
            nickserv_password: None,
            sasl_password: None,
            verify_tls: true,
            allow_insecure_tls_with_password: false,
            approval_owners: Vec::new(),
        })
    }

    #[test]
    fn add_allowed_identity_runtime_grants_immediate_access() {
        let ch = empty_allowlist_channel();
        assert!(!ch.is_user_allowed("alice"));
        ch.add_allowed_identity_runtime("alice");
        assert!(ch.is_user_allowed("alice"));
        // Case-insensitive dedupe (matches the allowlist semantics).
        ch.add_allowed_identity_runtime("ALICE");
        assert_eq!(ch.allowed_users.read().unwrap().len(), 1);
    }

    /// A store-minted "irc" code (the kind `rantaiclaw channels pair` issues) is
    /// accepted on `/claim`: the shared core lands the sender's nick in
    /// `allowed_users` AND `approval_owners`. Drives the same code path the
    /// PRIVMSG handler invokes.
    #[tokio::test]
    async fn store_minted_irc_code_claims_owner() {
        use crate::channels::pairing::{try_handle_pairing, AllowlistField};
        use crate::config::schema::IrcConfig;
        use crate::security::pairing_store;

        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");

        // Seed a config with an irc section so apply_pairing has a target.
        {
            let mut seed = crate::config::Config::load_or_init().await.unwrap();
            seed.channels_config.irc = Some(IrcConfig {
                server: "irc.test".into(),
                port: 6697,
                nickname: "bot".into(),
                username: None,
                channels: vec![],
                allowed_users: vec![],
                server_password: None,
                nickserv_password: None,
                sasl_password: None,
                verify_tls: Some(true),
                allow_insecure_tls_with_password: false,
            });
            seed.save().await.unwrap();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let code = pairing_store::mint(root, "irc", 3_600, None, true, now).unwrap();

        let reply = try_handle_pairing(
            &format!("/claim {code}"),
            "irc",
            AllowlistField::AllowedUsers,
            &["alice".to_string()],
            root,
        )
        .await
        .expect("a /claim must be handled");
        assert!(reply.contains("owner"), "reply was: {reply}");

        let config = crate::config::Config::load_or_init().await.unwrap();
        let users = &config.channels_config.irc.as_ref().unwrap().allowed_users;
        assert!(users.contains(&"alice".to_string()), "users: {users:?}");
        let owners = &config.channels_config.approval_owners;
        assert!(owners.contains(&"alice".to_string()), "owners: {owners:?}");

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }

    // ── Helpers ─────────────────────────────────────────────

    fn make_channel() -> IrcChannel {
        IrcChannel::new(IrcChannelConfig {
            server: "irc.example.com".into(),
            port: 6697,
            nickname: "zcbot".into(),
            username: None,
            channels: vec!["#rantaiclaw".into()],
            allowed_users: vec!["*".into()],
            server_password: None,
            nickserv_password: None,
            sasl_password: None,
            verify_tls: true,
            allow_insecure_tls_with_password: false,
            approval_owners: Vec::new(),
        })
    }

    // ── Plan 126: identity, teardown, pacing, TLS ────────────────

    fn channel_with_owners(owners: &[&str]) -> IrcChannel {
        IrcChannel::new(IrcChannelConfig {
            server: "irc.test".into(),
            port: 6697,
            nickname: "bot".into(),
            username: None,
            channels: vec![],
            allowed_users: vec!["*".into()],
            server_password: None,
            nickserv_password: None,
            sasl_password: None,
            verify_tls: true,
            allow_insecure_tls_with_password: false,
            approval_owners: owners.iter().map(|o| (*o).to_string()).collect(),
        })
    }

    /// THE plan's primary test. An IRC nick is a first-come lease: anyone who
    /// connects while the owner is offline holds it. Resolving that nick as the
    /// owner hands over the full toolset plus authority to approve shell
    /// commands.
    #[test]
    fn nick_without_an_account_tag_is_not_an_owner() {
        let ch = channel_with_owners(&["alice"]);
        assert!(
            ch.resolve_identity(None, "alice").is_none(),
            "an unauthenticated nick that matches an owner must be dropped"
        );
        // Case-insensitively, too — IRC nicks are.
        assert!(ch.resolve_identity(None, "ALICE").is_none());
        // A stranger is unaffected: the chat allowlist still governs them.
        assert_eq!(
            ch.resolve_identity(None, "bob"),
            Some("bob".to_string()),
            "only the owner path fails closed; ordinary chat still works"
        );
    }

    /// The account is the identity; the nick is not carried as an alias,
    /// because aliases are matched by the owner gate and would let whoever
    /// holds the nick borrow an owner entry recorded under that name.
    #[test]
    fn account_tag_is_the_primary_sender_and_the_nick_is_not_an_alias() {
        let ch = channel_with_owners(&["alice"]);
        assert_eq!(
            ch.resolve_identity(Some("alice"), "alice_"),
            Some("alice".to_string())
        );
        assert_eq!(
            ch.resolve_identity(Some("mallory"), "alice"),
            Some("mallory".to_string()),
            "holding the owner's nick must not import the owner's authority"
        );
    }

    /// `*` is the account-tag spec's "not logged in" marker — and the owner
    /// gate's wildcard. Letting it through would make every logged-out user an
    /// owner at once.
    #[test]
    fn a_star_account_tag_is_not_an_identity() {
        let msg = IrcMessage::parse("@account=* :eve!e@host PRIVMSG #ch :hi").unwrap();
        assert_eq!(msg.account(), None);
        let msg = IrcMessage::parse("@account=alice :alice!a@host PRIVMSG #ch :hi").unwrap();
        assert_eq!(msg.account(), Some("alice"));
    }

    /// A literal `*` in `approval_owners` is the operator switching the owner
    /// gate off. Dropping every message on that network would be a worse
    /// failure than the one being prevented.
    #[test]
    fn a_wildcard_owner_list_does_not_drop_messages() {
        let ch = channel_with_owners(&["*"]);
        assert_eq!(
            ch.resolve_identity(None, "anyone"),
            Some("anyone".to_string())
        );
    }

    #[test]
    fn tagged_lines_parse_and_untagged_lines_are_unaffected() {
        let msg = IrcMessage::parse(
            "@account=alice;time=2026-08-13T00:00:00Z :alice!a@h PRIVMSG #ch :yo",
        )
        .unwrap();
        assert_eq!(msg.command, "PRIVMSG");
        assert_eq!(msg.nick(), Some("alice"));
        assert_eq!(msg.params, vec!["#ch", "yo"]);
        assert_eq!(msg.tag("time"), Some("2026-08-13T00:00:00Z"));

        let plain = IrcMessage::parse(":bob!b@h PRIVMSG #ch :yo").unwrap();
        assert!(plain.tags.is_empty());
        assert_eq!(plain.command, "PRIVMSG");
    }

    #[test]
    fn tag_values_are_unescaped() {
        let msg = IrcMessage::parse(r"@k=a\sb\:c\\d\r\n :n!u@h PING :x").unwrap();
        assert_eq!(msg.tag("k"), Some("a b;c\\d\r\n"));
        // A valueless tag is present with an empty value.
        let msg = IrcMessage::parse("@bare :n!u@h PING :x").unwrap();
        assert_eq!(msg.tag("bare"), Some(""));
    }

    #[test]
    fn caps_requested_are_the_intersection_with_what_the_server_offers() {
        let offered = IrcChannel::parse_cap_list("multi-prefix sasl=PLAIN account-tag");
        assert_eq!(
            IrcChannel::caps_to_request(&offered, true),
            vec!["account-tag", "sasl"],
            "extended-join was not offered, so it must not be requested"
        );
        assert_eq!(
            IrcChannel::caps_to_request(&offered, false),
            vec!["account-tag"],
            "sasl must not be requested when no SASL password is configured"
        );
        assert!(
            IrcChannel::caps_to_request(&[], true).is_empty(),
            "a server that offers nothing gets no REQ at all"
        );
    }

    /// The write half used to survive the listener, so `send()` wrote into a
    /// half-closed socket, returned `Ok(())`, and the reply was lost silently.
    #[tokio::test]
    async fn send_after_listener_exit_is_an_error() {
        let ch = channel_with_owners(&[]);
        // Seed a live sink so the slot is occupied exactly as it would be
        // mid-session; `listen()` then fails at connect and must clear it.
        let (sink, _peer) = tokio::io::duplex(1024);
        *ch.writer.lock().await = Some(Box::new(sink));
        assert!(
            ch.send(&SendMessage::new("probe".to_string(), "#ch".to_string()))
                .await
                .is_ok(),
            "precondition: an occupied slot accepts a write"
        );

        let (tx, _rx) = mpsc::channel(1);
        let result = ch
            .listen(tx, tokio_util::sync::CancellationToken::new())
            .await;
        assert!(result.is_err(), "connecting to irc.test must fail");

        assert!(
            ch.writer.lock().await.is_none(),
            "the listener must clear the write half on the way out"
        );
        let err = ch
            .send(&SendMessage::new("hello".to_string(), "#ch".to_string()))
            .await
            .expect_err("send must report the dead session, not swallow it");
        assert!(err.to_string().contains("not connected"), "got: {err}");
        assert!(!ch.health_check().await);
    }

    /// Chunks went out back-to-back, which most networks disconnect as excess
    /// flood — so a long reply failed more reliably than a short one.
    #[test]
    fn chunks_are_paced_after_a_short_burst() {
        assert_eq!(IrcChannel::pace_before(0), None);
        assert_eq!(IrcChannel::pace_before(BURST_LINES - 1), None);
        assert_eq!(IrcChannel::pace_before(BURST_LINES), Some(CHUNK_DELAY));
        assert_eq!(IrcChannel::pace_before(50), Some(CHUNK_DELAY));
    }

    /// The pacing is actually applied — on a paused clock, so the test does
    /// not sleep. Every chunk still arrives, in order.
    #[tokio::test(start_paused = true)]
    async fn paced_send_still_delivers_every_line_in_order() {
        let ch = channel_with_owners(&[]);
        let (sink, mut peer) = tokio::io::duplex(64 * 1024);
        *ch.writer.lock().await = Some(Box::new(sink));

        // Long enough that IRC's own 512-byte splitter produces several
        // PRIVMSGs; the renderer folds a multi-line body back into one line,
        // so length is what makes chunks here.
        let body = "a".repeat(2_000);
        let started = tokio::time::Instant::now();
        ch.send(&SendMessage::new(body, "#ch".to_string()))
            .await
            .expect("send");
        let elapsed = started.elapsed();

        // Close the write half so the read below terminates.
        ch.writer.lock().await.take();
        let mut written = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut written)
            .await
            .expect("read");
        let written = String::from_utf8(written).expect("utf-8");
        let lines = written.matches("PRIVMSG #ch :").count();
        assert!(lines > BURST_LINES, "expected several chunks, got {lines}");
        assert_eq!(
            written.matches('a').count(),
            2_000,
            "every byte of the payload must be delivered"
        );
        assert_eq!(
            elapsed,
            CHUNK_DELAY * u32::try_from(lines - BURST_LINES).unwrap(),
            "every line past the burst must be paced"
        );
    }

    #[test]
    fn nick_retry_is_capped() {
        // Walk the retry sequence the 433 arm walks: it must terminate rather
        // than keep minting candidates for a server that rejects every one.
        let mut nick = "bot".to_string();
        let mut retries = 0_u8;
        while let Some(next) = IrcChannel::next_nick_candidate(&nick, retries) {
            nick = next;
            retries += 1;
            assert!(retries <= 10, "the retry loop did not terminate");
        }
        assert_eq!(retries, MAX_NICK_RETRIES);
        assert_eq!(nick, "bot___");
        assert!(IrcChannel::next_nick_candidate(&nick, retries).is_none());
    }

    #[tokio::test]
    async fn verify_tls_false_with_a_password_is_refused_without_the_opt_in() {
        let insecure = |allow: bool, sasl: Option<&str>| {
            IrcChannel::new(IrcChannelConfig {
                server: "irc.test".into(),
                port: 6697,
                nickname: "bot".into(),
                username: None,
                channels: vec![],
                allowed_users: vec![],
                server_password: None,
                nickserv_password: None,
                sasl_password: sasl.map(str::to_string),
                verify_tls: false,
                allow_insecure_tls_with_password: allow,
                approval_owners: Vec::new(),
            })
        };

        let refusal = insecure(false, Some("hunter2"))
            .insecure_credential_refusal()
            .expect("a credential over an unauthenticated link must be refused");
        assert!(refusal.contains("sasl_password"), "{refusal}");
        assert!(
            !refusal.contains("hunter2"),
            "the refusal must not quote the credential: {refusal}"
        );

        assert!(
            insecure(true, Some("hunter2"))
                .insecure_credential_refusal()
                .is_none(),
            "the explicit opt-in must be honoured"
        );
        assert!(
            insecure(false, None)
                .insecure_credential_refusal()
                .is_none(),
            "no credential, no disclosure — an unverified link alone still starts"
        );
        assert!(
            channel_with_owners(&[])
                .insecure_credential_refusal()
                .is_none(),
            "verify_tls = true is unaffected"
        );

        // And the refusal reaches the caller, rather than only being logged.
        let (tx, _rx) = mpsc::channel(1);
        let err = insecure(false, Some("hunter2"))
            .listen(tx, tokio_util::sync::CancellationToken::new())
            .await
            .expect_err("the channel must refuse to start");
        assert!(err.to_string().contains("verify_tls"), "got: {err}");
    }

    #[test]
    fn allowlist_edit_reaches_the_channel() {
        let ch = channel_with_owners(&[]);
        assert!(ch.is_user_allowed("anyone"), "seeded with *");
        ch.apply_allowed_senders(&["alice".to_string()]);
        assert!(ch.is_user_allowed("alice"));
        assert!(
            !ch.is_user_allowed("anyone"),
            "the edit must replace the list, not append to it"
        );
    }
}
