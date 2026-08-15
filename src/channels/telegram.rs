use super::traits::{Channel, ChannelMessage, SendMessage};
use crate::config::{Config, StreamMode};
use crate::security::pairing::PairingGuard;
use anyhow::Context;
use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::multipart::{Form, Part};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::fs;

/// Telegram's maximum message length for text messages
const TELEGRAM_MAX_MESSAGE_LENGTH: usize = 4096;
/// Reserve space for continuation markers added by send_text_chunks:
/// worst case is "(continued)\n\n" + chunk + "\n\n(continues...)" = 30 extra chars
const TELEGRAM_CONTINUATION_OVERHEAD: usize = 30;
const TELEGRAM_BIND_COMMAND: &str = "/bind";
/// `/claim <code>` — like `/bind`, but also registers the sender as an
/// approval **owner** (`channels_config.approval_owners`), i.e. someone whose
/// in-chat `/approve` of a gated tool is honored. Reuses the same one-time
/// pairing code as `/bind`.
const TELEGRAM_CLAIM_COMMAND: &str = "/claim";

/// Split a message into chunks that respect Telegram's 4096 character limit.
/// Tries to split at word boundaries when possible, and handles continuation.
/// The effective per-chunk limit is reduced to leave room for continuation markers.
/// Add the "(continued)" / "(continues...)" markers used for multi-chunk sends.
/// `TELEGRAM_CONTINUATION_OVERHEAD` (30) covers this helper's 29-char worst case,
/// so `format::split` budgets on `limit - overhead` and the decorated chunk never
/// exceeds Telegram's cap.
fn decorate_continuation(chunk: &str, index: usize, total: usize) -> String {
    if total <= 1 {
        chunk.to_string()
    } else if index == 0 {
        format!("{chunk}\n\n(continues...)")
    } else if index + 1 == total {
        format!("(continued)\n\n{chunk}")
    } else {
        format!("(continued)\n\n{chunk}\n\n(continues...)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TelegramAttachmentKind {
    Image,
    Document,
    Video,
    Audio,
    Voice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TelegramAttachment {
    kind: TelegramAttachmentKind,
    target: String,
}

impl TelegramAttachmentKind {
    fn from_marker(marker: &str) -> Option<Self> {
        match marker.trim().to_ascii_uppercase().as_str() {
            "IMAGE" | "PHOTO" => Some(Self::Image),
            "DOCUMENT" | "FILE" => Some(Self::Document),
            "VIDEO" => Some(Self::Video),
            "AUDIO" => Some(Self::Audio),
            "VOICE" => Some(Self::Voice),
            _ => None,
        }
    }
}

fn is_http_url(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

fn infer_attachment_kind_from_target(target: &str) -> Option<TelegramAttachmentKind> {
    let normalized = target
        .split('?')
        .next()
        .unwrap_or(target)
        .split('#')
        .next()
        .unwrap_or(target);

    let extension = Path::new(normalized)
        .extension()
        .and_then(|ext| ext.to_str())?
        .to_ascii_lowercase();

    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => Some(TelegramAttachmentKind::Image),
        "mp4" | "mov" | "mkv" | "avi" | "webm" => Some(TelegramAttachmentKind::Video),
        "mp3" | "m4a" | "wav" | "flac" => Some(TelegramAttachmentKind::Audio),
        "ogg" | "oga" | "opus" => Some(TelegramAttachmentKind::Voice),
        "pdf" | "txt" | "md" | "csv" | "json" | "zip" | "tar" | "gz" | "doc" | "docx" | "xls"
        | "xlsx" | "ppt" | "pptx" => Some(TelegramAttachmentKind::Document),
        _ => None,
    }
}

/// Confine an outbound attachment's local path to the agent workspace.
///
/// Returns `true` only if `target` — after canonicalization, which resolves
/// symlinks and `..` — lives under the canonical `workspace` root. A model
/// reply can be influenced (or prompt-injected) by a channel guest, so an
/// attachment marker like `[DOCUMENT:~/.rantaiclaw/config.toml]` must not be
/// able to read arbitrary host files (config with API keys + bot token, ssh
/// keys, `/etc/*`) and upload them to the chat. This mirrors the workspace
/// confinement the `file_*` tools already enforce (`is_resolved_path_allowed`);
/// the attachment path was a second, unsandboxed file read.
///
/// Fails closed: an unresolvable target (missing file, canonicalize error) is
/// not sendable.
fn attachment_path_within_workspace(target: &Path, workspace: &Path) -> bool {
    let Ok(canonical_target) = target.canonicalize() else {
        return false;
    };
    let workspace_root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    canonical_target.starts_with(&workspace_root)
}

fn parse_path_only_attachment(message: &str) -> Option<TelegramAttachment> {
    let trimmed = message.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }

    let candidate = trimmed.trim_matches(|c| matches!(c, '`' | '"' | '\''));
    if candidate.chars().any(char::is_whitespace) {
        return None;
    }

    let candidate = candidate.strip_prefix("file://").unwrap_or(candidate);
    let kind = infer_attachment_kind_from_target(candidate)?;

    if !is_http_url(candidate) && !Path::new(candidate).exists() {
        return None;
    }

    Some(TelegramAttachment {
        kind,
        target: candidate.to_string(),
    })
}

/// Strip tool_call XML-style tags from message text.
///
/// These tags are internal syntax and must not be shown to the user. The
/// original rationale — that Telegram's Markdown parser rejects them with a
/// 400 — no longer holds: this channel renders `RenderTarget::TelegramHtml`
/// and `escape_html` turns a literal `<` into `&lt;`, so an unstripped tag
/// cannot produce a parse error. Leaked tool-call XML is still ugly, which is
/// why the function stays.
fn strip_tool_call_tags(message: &str) -> String {
    const TOOL_CALL_OPEN_TAGS: [&str; 7] = [
        "<function_calls>",
        "<function_call>",
        "<tool_call>",
        "<toolcall>",
        "<tool-call>",
        "<tool>",
        "<invoke>",
    ];

    fn find_first_tag<'a>(haystack: &str, tags: &'a [&'a str]) -> Option<(usize, &'a str)> {
        tags.iter()
            .filter_map(|tag| haystack.find(tag).map(|idx| (idx, *tag)))
            .min_by_key(|(idx, _)| *idx)
    }

    fn matching_close_tag(open_tag: &str) -> Option<&'static str> {
        match open_tag {
            "<function_calls>" => Some("</function_calls>"),
            "<function_call>" => Some("</function_call>"),
            "<tool_call>" => Some("</tool_call>"),
            "<toolcall>" => Some("</toolcall>"),
            "<tool-call>" => Some("</tool-call>"),
            "<tool>" => Some("</tool>"),
            "<invoke>" => Some("</invoke>"),
            _ => None,
        }
    }

    fn extract_first_json_end(input: &str) -> Option<usize> {
        let trimmed = input.trim_start();
        let trim_offset = input.len().saturating_sub(trimmed.len());

        for (byte_idx, ch) in trimmed.char_indices() {
            if ch != '{' && ch != '[' {
                continue;
            }

            let slice = &trimmed[byte_idx..];
            let mut stream =
                serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
            if let Some(Ok(_value)) = stream.next() {
                let consumed = stream.byte_offset();
                if consumed > 0 {
                    return Some(trim_offset + byte_idx + consumed);
                }
            }
        }

        None
    }

    fn strip_leading_close_tags(mut input: &str) -> &str {
        loop {
            let trimmed = input.trim_start();
            if !trimmed.starts_with("</") {
                return trimmed;
            }

            let Some(close_end) = trimmed.find('>') else {
                return "";
            };
            input = &trimmed[close_end + 1..];
        }
    }

    let mut kept_segments = Vec::new();
    let mut remaining = message;

    while let Some((start, open_tag)) = find_first_tag(remaining, &TOOL_CALL_OPEN_TAGS) {
        let before = &remaining[..start];
        if !before.is_empty() {
            kept_segments.push(before.to_string());
        }

        let Some(close_tag) = matching_close_tag(open_tag) else {
            break;
        };
        let after_open = &remaining[start + open_tag.len()..];

        if let Some(close_idx) = after_open.find(close_tag) {
            remaining = &after_open[close_idx + close_tag.len()..];
            continue;
        }

        if let Some(consumed_end) = extract_first_json_end(after_open) {
            remaining = strip_leading_close_tags(&after_open[consumed_end..]);
            continue;
        }

        // Unterminated tag: drop it and everything after, rather than
        // re-emitting the raw tags this function exists to remove. Anything
        // following an unclosed tool-call opener is the tool payload, not
        // prose for the user.
        remaining = "";
        break;
    }

    if !remaining.is_empty() {
        kept_segments.push(remaining.to_string());
    }

    let mut result = kept_segments.concat();

    // Clean up any resulting blank lines (but preserve paragraphs)
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }

    result.trim().to_string()
}

fn parse_attachment_markers(message: &str) -> (String, Vec<TelegramAttachment>) {
    let mut cleaned = String::with_capacity(message.len());
    let mut attachments = Vec::new();
    let mut cursor = 0;

    while cursor < message.len() {
        let Some(open_rel) = message[cursor..].find('[') else {
            cleaned.push_str(&message[cursor..]);
            break;
        };

        let open = cursor + open_rel;
        cleaned.push_str(&message[cursor..open]);

        let Some(close_rel) = message[open..].find(']') else {
            cleaned.push_str(&message[open..]);
            break;
        };

        let close = open + close_rel;
        let marker = &message[open + 1..close];

        let parsed = marker.split_once(':').and_then(|(kind, target)| {
            let kind = TelegramAttachmentKind::from_marker(kind)?;
            let target = target.trim();
            if target.is_empty() {
                return None;
            }
            Some(TelegramAttachment {
                kind,
                target: target.to_string(),
            })
        });

        if let Some(attachment) = parsed {
            attachments.push(attachment);
        } else {
            cleaned.push_str(&message[open..=close]);
        }

        cursor = close + 1;
    }

    (cleaned.trim().to_string(), attachments)
}

/// Media-marker syntax, appended to the system prompt on this channel only.
/// Telegram is the one channel that can actually deliver an attachment; telling
/// the model otherwise elsewhere leaks markers as literal text.
pub(crate) const TELEGRAM_DELIVERY_INSTRUCTIONS: &str = "When responding on Telegram, include media markers for files or URLs that should be sent as attachments. Use one marker per attachment with this exact syntax: [IMAGE:<path-or-url>], [DOCUMENT:<path-or-url>], [VIDEO:<path-or-url>], [AUDIO:<path-or-url>], or [VOICE:<path-or-url>]. Keep normal user-facing text outside markers and never wrap markers in code fences.";

/// Telegram channel — long-polls the Bot API for updates
pub struct TelegramChannel {
    bot_token: String,
    allowed_users: Arc<RwLock<Vec<String>>>,
    pairing: Option<PairingGuard>,
    client: reqwest::Client,
    /// Keyed by recipient. One shared slot meant starting typing for chat B
    /// silently killed chat A's indicator, and with the runtime's parallel
    /// message path concurrent chats are the normal case.
    typing_handles: Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>,
    stream_mode: StreamMode,
    draft_update_interval_ms: u64,
    last_draft_edit: Mutex<std::collections::HashMap<String, std::time::Instant>>,
    mention_only: bool,
    bot_username: Mutex<Option<String>>,
    /// Size/type limits for inbound images. Defaults to the shipped
    /// `[multimodal]` defaults; the factory overrides it with the operator's.
    multimodal: crate::config::MultimodalConfig,
    /// Bot API root. Only the tests override it — nine of them used to send
    /// real requests to `api.telegram.org` with a fake token and assert
    /// `is_err()`, which passes whether or not the request was well formed.
    api_base: String,
}

/// The public Bot API. Not a config key: overriding it is a test seam.
const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

/// Validate a Telegram bot token by calling `getMe` directly, returning the
/// bot's username on success. Deliberately does NOT build a [`TelegramChannel`]
/// (which sets up pairing state and can print a one-time pairing code) — it is
/// a side-effect-free probe used by the webui "connect channel" flow to confirm
/// a token works before persisting it. The caller distinguishes "invalid token"
/// from "Telegram unreachable" only by the message; both are connect failures.
pub async fn validate_bot_token(bot_token: &str) -> anyhow::Result<String> {
    let url = format!("{TELEGRAM_API_BASE}/bot{bot_token}/getMe");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("could not reach Telegram")?;
    if !resp.status().is_success() {
        anyhow::bail!("Telegram getMe returned HTTP {}", resp.status());
    }
    let data: serde_json::Value = resp.json().await.context("parse getMe response")?;
    data.get("result")
        .and_then(|r| r.get("username"))
        .and_then(|u| u.as_str())
        .map(str::to_string)
        .context("bot username missing from getMe response")
}

impl TelegramChannel {
    pub fn new(bot_token: String, allowed_users: Vec<String>, mention_only: bool) -> Self {
        let normalized_allowed = Self::normalize_allowed_users(allowed_users);
        let pairing = if normalized_allowed.is_empty() {
            let guard = PairingGuard::new(true, &[]);
            if let Some(code) = guard.pairing_code() {
                println!("  🔐 Telegram pairing required. One-time code: {code}");
                println!("     DM the bot `{TELEGRAM_BIND_COMMAND} {code}` to let yourself chat,");
                println!(
                    "     or `{TELEGRAM_CLAIM_COMMAND} {code}` to also become an approval owner (can /approve tools)."
                );
            }
            Some(guard)
        } else {
            None
        };

        Self {
            bot_token,
            allowed_users: Arc::new(RwLock::new(normalized_allowed)),
            pairing,
            client: reqwest::Client::new(),
            stream_mode: StreamMode::Off,
            draft_update_interval_ms: 1000,
            last_draft_edit: Mutex::new(std::collections::HashMap::new()),
            typing_handles: Mutex::new(std::collections::HashMap::new()),
            mention_only,
            bot_username: Mutex::new(None),
            multimodal: crate::config::MultimodalConfig::default(),
            api_base: TELEGRAM_API_BASE.to_string(),
        }
    }

    /// Apply the operator's `[multimodal]` limits to inbound photos.
    #[must_use]
    pub fn with_multimodal(mut self, multimodal: crate::config::MultimodalConfig) -> Self {
        self.multimodal = multimodal;
        self
    }

    /// Point this channel at a local server so a test can assert what was
    /// actually sent.
    #[cfg(test)]
    fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    /// Configure streaming mode for progressive draft updates.
    pub fn with_streaming(
        mut self,
        stream_mode: StreamMode,
        draft_update_interval_ms: u64,
    ) -> Self {
        self.stream_mode = stream_mode;
        self.draft_update_interval_ms = draft_update_interval_ms;
        self
    }

    /// Parse reply_target into (chat_id, optional thread_id).
    fn parse_reply_target(reply_target: &str) -> (String, Option<String>) {
        if let Some((chat_id, thread_id)) = reply_target.split_once(':') {
            (chat_id.to_string(), Some(thread_id.to_string()))
        } else {
            (reply_target.to_string(), None)
        }
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client("channel.telegram")
    }

    fn normalize_identity(value: &str) -> String {
        value.trim().trim_start_matches('@').to_string()
    }

    fn normalize_allowed_users(allowed_users: Vec<String>) -> Vec<String> {
        allowed_users
            .into_iter()
            .map(|entry| Self::normalize_identity(&entry))
            .filter(|entry| !entry.is_empty())
            .collect()
    }

    async fn load_config_without_env() -> anyhow::Result<Config> {
        // Resolve the SAME config path the daemon reads (profile / env-dir /
        // active_workspace-marker aware) instead of a hardcoded
        // `~/.rantaiclaw/config.toml`. The legacy hardcoded path missed the
        // profile layout entirely, so `/claim`/`/bind` persisted owner +
        // allowlist to a file the daemon never reads (or, on migrated installs,
        // clobbered the compatibility symlink). We still skip env-VALUE overrides
        // by parsing the file directly rather than going through `load_or_init`.
        let (config_path, workspace_dir) = Config::resolve_active_paths().await?;

        let contents = fs::read_to_string(&config_path)
            .await
            .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
        let mut config: Config = toml::from_str(&contents).context(
            "Failed to parse config.toml — check [channels_config.telegram] section for syntax errors",
        )?;
        config.config_path = config_path;
        config.workspace_dir = workspace_dir;
        Ok(config)
    }

    async fn persist_allowed_identity(&self, identity: &str) -> anyhow::Result<()> {
        let mut config = Self::load_config_without_env().await?;
        let Some(telegram) = config.channels_config.telegram.as_mut() else {
            anyhow::bail!(
                "Missing [channels_config.telegram] section in config.toml. \
                Add bot_token and allowed_users under [channels_config.telegram], \
                or run `rantaiclaw onboard --channels-only` to configure interactively"
            );
        };

        let normalized = Self::normalize_identity(identity);
        if normalized.is_empty() {
            anyhow::bail!("Cannot persist empty Telegram identity");
        }

        if !telegram.allowed_users.iter().any(|u| u == &normalized) {
            telegram.allowed_users.push(normalized);
            config
                .save()
                .await
                .context("Failed to persist Telegram allowlist to config.toml")?;
        }

        Ok(())
    }

    /// Add `identities` to `channels_config.approval_owners` and persist.
    /// Owners are matched against the inbound sender id by [`crate::approval::can_approve`],
    /// which resolves to the username (if any) else the numeric id — so we
    /// persist BOTH forms (when distinct) to guarantee a match either way.
    /// Takes effect for in-chat approval when the channels runtime (re)starts.
    async fn persist_approval_owner(&self, identities: &[String]) -> anyhow::Result<()> {
        let mut config = Self::load_config_without_env().await?;
        let mut changed = false;
        for id in identities {
            let normalized = Self::normalize_identity(id);
            if normalized.is_empty() {
                continue;
            }
            if !config
                .channels_config
                .approval_owners
                .iter()
                .any(|o| o == &normalized)
            {
                config.channels_config.approval_owners.push(normalized);
                changed = true;
            }
        }
        if changed {
            config
                .save()
                .await
                .context("Failed to persist approval_owners to config.toml")?;
        }
        Ok(())
    }

    fn add_allowed_identity_runtime(&self, identity: &str) {
        let normalized = Self::normalize_identity(identity);
        if normalized.is_empty() {
            return;
        }
        if let Ok(mut users) = self.allowed_users.write() {
            if !users.iter().any(|u| u == &normalized) {
                users.push(normalized);
            }
        }
    }

    /// Parse `<command> <code>` (tolerating `<command>@botname`), returning the
    /// trimmed non-empty code. Shared by `/bind` and `/claim`.
    fn extract_command_code<'a>(text: &'a str, command: &str) -> Option<&'a str> {
        let mut parts = text.split_whitespace();
        let cmd = parts.next()?;
        let base_command = cmd.split('@').next().unwrap_or(cmd);
        if base_command != command {
            return None;
        }
        parts.next().map(str::trim).filter(|code| !code.is_empty())
    }

    fn extract_bind_code(text: &str) -> Option<&str> {
        Self::extract_command_code(text, TELEGRAM_BIND_COMMAND)
    }

    fn extract_claim_code(text: &str) -> Option<&str> {
        Self::extract_command_code(text, TELEGRAM_CLAIM_COMMAND)
    }

    fn pairing_code_active(&self) -> bool {
        self.pairing
            .as_ref()
            .and_then(PairingGuard::pairing_code)
            .is_some()
    }

    /// The draft body to send: the rendered text, cut to Telegram's limit.
    ///
    /// Gated and cut in `chars`, the unit the limit is expressed in — the same
    /// reasoning `finalize_draft` already carries. The gate used to read raw
    /// `len()` bytes, so a CJK or emoji-heavy reply was truncated at roughly a
    /// third of its intended length.
    fn draft_display_text(rendered: &str) -> &str {
        if rendered.chars().count() <= TELEGRAM_MAX_MESSAGE_LENGTH {
            return rendered;
        }
        let end = rendered
            .char_indices()
            .nth(TELEGRAM_MAX_MESSAGE_LENGTH)
            .map_or(rendered.len(), |(idx, _)| idx);
        &rendered[..end]
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.api_base, self.bot_token)
    }

    async fn fetch_bot_username(&self) -> anyhow::Result<String> {
        let resp = self.http_client().get(self.api_url("getMe")).send().await?;

        if !resp.status().is_success() {
            anyhow::bail!("Failed to fetch bot info: {}", resp.status());
        }

        let data: serde_json::Value = resp.json().await?;
        let username = data
            .get("result")
            .and_then(|r| r.get("username"))
            .and_then(|u| u.as_str())
            .context("Bot username not found in response")?;

        Ok(username.to_string())
    }

    async fn get_bot_username(&self) -> Option<String> {
        {
            let cache = self.bot_username.lock();
            if let Some(ref username) = *cache {
                return Some(username.clone());
            }
        }

        match self.fetch_bot_username().await {
            Ok(username) => {
                let mut cache = self.bot_username.lock();
                *cache = Some(username.clone());
                Some(username)
            }
            Err(e) => {
                tracing::warn!("Failed to fetch bot username: {e}");
                None
            }
        }
    }

    fn is_telegram_username_char(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || ch == '_'
    }

    fn find_bot_mention_spans(text: &str, bot_username: &str) -> Vec<(usize, usize)> {
        let bot_username = bot_username.trim_start_matches('@');
        if bot_username.is_empty() {
            return Vec::new();
        }

        let mut spans = Vec::new();

        for (at_idx, ch) in text.char_indices() {
            if ch != '@' {
                continue;
            }

            if at_idx > 0 {
                let prev = text[..at_idx].chars().next_back().unwrap_or(' ');
                if Self::is_telegram_username_char(prev) {
                    continue;
                }
            }

            let username_start = at_idx + 1;
            let mut username_end = username_start;

            for (rel_idx, candidate_ch) in text[username_start..].char_indices() {
                if Self::is_telegram_username_char(candidate_ch) {
                    username_end = username_start + rel_idx + candidate_ch.len_utf8();
                } else {
                    break;
                }
            }

            if username_end == username_start {
                continue;
            }

            let mention_username = &text[username_start..username_end];
            if mention_username.eq_ignore_ascii_case(bot_username) {
                spans.push((at_idx, username_end));
            }
        }

        spans
    }

    fn contains_bot_mention(text: &str, bot_username: &str) -> bool {
        !Self::find_bot_mention_spans(text, bot_username).is_empty()
    }

    fn normalize_incoming_content(text: &str, bot_username: &str) -> Option<String> {
        let spans = Self::find_bot_mention_spans(text, bot_username);
        if spans.is_empty() {
            let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
            return (!normalized.is_empty()).then_some(normalized);
        }

        let mut normalized = String::with_capacity(text.len());
        let mut cursor = 0;
        for (start, end) in spans {
            normalized.push_str(&text[cursor..start]);
            cursor = end;
        }
        normalized.push_str(&text[cursor..]);

        let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
        (!normalized.is_empty()).then_some(normalized)
    }

    fn is_group_message(message: &serde_json::Value) -> bool {
        message
            .get("chat")
            .and_then(|c| c.get("type"))
            .and_then(|t| t.as_str())
            .map(|t| t == "group" || t == "supergroup")
            .unwrap_or(false)
    }

    fn is_user_allowed(&self, username: &str) -> bool {
        let identity = Self::normalize_identity(username);
        self.allowed_users
            .read()
            .map(|users| users.iter().any(|u| u == "*" || u == &identity))
            .unwrap_or(false)
    }

    fn is_any_user_allowed<'a, I>(&self, identities: I) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        identities.into_iter().any(|id| self.is_user_allowed(id))
    }

    /// Extract `(text, chat_id, identities)` from an inbound update for the
    /// shared pairing path. `identities` is `[numeric_id, username]` (each
    /// included only when present and non-empty) — the same forms the legacy
    /// bind/claim path persists, so `can_approve` resolves either. Returns
    /// `None` when the update has no text message or no chat id.
    fn extract_pairing_context(
        update: &serde_json::Value,
    ) -> Option<(String, String, Vec<String>)> {
        let message = update.get("message")?;
        let text = message.get("text").and_then(serde_json::Value::as_str)?;
        let chat_id = message
            .get("chat")
            .and_then(|c| c.get("id"))
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string())?;

        let from = message.get("from");
        let mut identities: Vec<String> = Vec::new();
        if let Some(id) = from
            .and_then(|f| f.get("id"))
            .and_then(serde_json::Value::as_i64)
        {
            identities.push(id.to_string());
        }
        if let Some(username) = from
            .and_then(|f| f.get("username"))
            .and_then(serde_json::Value::as_str)
            .map(Self::normalize_identity)
            .filter(|u| !u.is_empty() && u != "unknown")
        {
            identities.push(username);
        }

        Some((text.to_string(), chat_id, identities))
    }

    /// Shared-store fallback for a `/bind`/`/claim` whose code the in-memory
    /// [`PairingGuard`] did not recognize.
    ///
    /// Operators mint on-demand codes (`rantaiclaw channels pair`) into the
    /// shared [`crate::security::pairing_store`] without restarting the daemon.
    /// This consults that store via the shared
    /// [`crate::channels::pairing::try_handle_pairing`] core (which appends the
    /// sender to `allowed_users` and, for an owner-capable `/claim`, to
    /// `approval_owners`, then persists `config.toml`). It only *consumes* a
    /// store code when one actually matches (probed first via
    /// [`crate::security::pairing_store::contains`]), so a non-matching code
    /// falls through to the legacy in-memory PairingGuard path and the startup
    /// code keeps working.
    ///
    /// `identities` must be the sender's persisted forms — `[numeric_id,
    /// username]` (both when present) — matching what the legacy path stores.
    /// Returns `true` if the store owned and handled the command (the caller
    /// must then NOT send its own reply or forward the message).
    async fn try_handle_store_pairing(
        &self,
        text: &str,
        chat_id: &str,
        identities: &[String],
    ) -> bool {
        use crate::channels::pairing::{parse_pairing_command, try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        let Some(cmd) = parse_pairing_command(text) else {
            return false;
        };
        let Some(root) = crate::channels::pairing::profile_root("telegram") else {
            return false;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Only take ownership of the command when the store actually has a live
        // matching code; otherwise let the legacy in-memory path handle it.
        match pairing_store::contains(&root, "telegram", &cmd.code, now) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(e) => {
                tracing::warn!("Telegram pairing store probe failed: {e:#}");
                return false;
            }
        }

        let Some(reply) = try_handle_pairing(
            text,
            "telegram",
            AllowlistField::AllowedUsers,
            identities,
            &root,
        )
        .await
        else {
            // Shouldn't happen (we only get here for a parsed command), but be safe.
            return false;
        };

        // Mirror the new identities into the runtime allowlist so the change
        // takes effect immediately without a restart (config is already saved).
        for id in identities {
            self.add_allowed_identity_runtime(id);
        }

        let _ = self.send(&SendMessage::new(reply, chat_id)).await;
        true
    }

    /// Handle a `/claim <code>` owner-claim. Returns `true` if the update was a
    /// claim attempt (handled here — must NOT be forwarded to the agent).
    ///
    /// Validates the one-time pairing code (same code as `/bind`), then registers
    /// the sender as an approval **owner** (`channels_config.approval_owners`) and
    /// an allowed user. Works whether or not the sender is already allowed, so it
    /// covers both the initial pairing bootstrap and an allowed user claiming
    /// ownership. The owner key is the sender's numeric id + username (both
    /// persisted, since `can_approve` resolves the sender to username-else-id).
    async fn try_handle_claim(&self, update: &serde_json::Value) -> bool {
        let Some(message) = update.get("message") else {
            return false;
        };
        let text = message
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let Some(code) = Self::extract_claim_code(text) else {
            return false;
        };
        let code = code.to_string();

        let Some(chat_id) = message
            .get("chat")
            .and_then(|c| c.get("id"))
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string())
        else {
            return true; // recognised as /claim but malformed — consume it
        };

        let from = message.get("from");
        let sender_id = from
            .and_then(|f| f.get("id"))
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string());
        let username = from
            .and_then(|f| f.get("username"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|u| !u.is_empty() && u != "unknown");

        // Persist both forms to approval_owners; primary (id-preferred) to the
        // chat allowlist so the owner can also talk to the bot.
        let mut owner_identities: Vec<String> = Vec::new();
        if let Some(ref id) = sender_id {
            owner_identities.push(id.clone());
        }
        if let Some(ref u) = username {
            owner_identities.push(u.clone());
        }
        let primary = sender_id.clone().or_else(|| username.clone());
        let Some(primary) = primary else {
            let _ = self
                .send(&SendMessage::new(
                    "Couldn't determine your Telegram id, so I can't make you an owner.",
                    &chat_id,
                ))
                .await;
            return true;
        };

        let Some(pairing) = self.pairing.as_ref() else {
            let _ = self
                .send(&SendMessage::new(
                    "Pairing isn't active, so `/claim` can't be verified. Ask the operator to (re)start the channel with an empty `allowed_users` (pairing mode) so a one-time code is issued.",
                    &chat_id,
                ))
                .await;
            return true;
        };

        match pairing.try_pair(&code, &chat_id).await {
            Ok(Some(_token)) => {
                self.add_allowed_identity_runtime(&primary);
                let _ = self.persist_allowed_identity(&primary).await;
                match self.persist_approval_owner(&owner_identities).await {
                    Ok(()) => {
                        let _ = self
                            .send(&SendMessage::new(
                                format!(
                                    "✅ You're now an approval owner ({primary}). You can `/approve` tool calls in chat. \
If approvals don't take effect right away, the operator may need to restart the channel runtime."
                                ),
                                &chat_id,
                            ))
                            .await;
                        tracing::info!(
                            "Telegram: /claim registered approval owner(s)={owner_identities:?}"
                        );
                    }
                    Err(e) => {
                        let _ = self
                            .send(&SendMessage::new(
                                format!("Added you as an allowed user, but failed to set you as an owner: {e}"),
                                &chat_id,
                            ))
                            .await;
                    }
                }
            }
            Ok(None) | Err(_) => {
                let _ = self
                    .send(&SendMessage::new(
                        "❌ Invalid or expired claim code. Ask the operator for a fresh one.",
                        &chat_id,
                    ))
                    .await;
            }
        }
        true
    }

    async fn handle_unauthorized_message(&self, update: &serde_json::Value) {
        let Some(message) = update.get("message") else {
            return;
        };

        let Some(text) = message.get("text").and_then(serde_json::Value::as_str) else {
            return;
        };

        let username_opt = message
            .get("from")
            .and_then(|from| from.get("username"))
            .and_then(serde_json::Value::as_str);
        let username = username_opt.unwrap_or("unknown");
        let normalized_username = Self::normalize_identity(username);

        let sender_id = message
            .get("from")
            .and_then(|from| from.get("id"))
            .and_then(serde_json::Value::as_i64);
        let sender_id_str = sender_id.map(|id| id.to_string());
        let normalized_sender_id = sender_id_str.as_deref().map(Self::normalize_identity);

        let chat_id = message
            .get("chat")
            .and_then(|chat| chat.get("id"))
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string());

        let Some(chat_id) = chat_id else {
            tracing::warn!("Telegram: missing chat_id in message, skipping");
            return;
        };

        let mut identities = vec![normalized_username.as_str()];
        if let Some(ref id) = normalized_sender_id {
            identities.push(id.as_str());
        }

        if self.is_any_user_allowed(identities.iter().copied()) {
            return;
        }

        if let Some(code) = Self::extract_bind_code(text) {
            if let Some(pairing) = self.pairing.as_ref() {
                match pairing.try_pair(code, &chat_id).await {
                    Ok(Some(_token)) => {
                        let bind_identity = normalized_sender_id.clone().or_else(|| {
                            if normalized_username.is_empty() || normalized_username == "unknown" {
                                None
                            } else {
                                Some(normalized_username.clone())
                            }
                        });

                        if let Some(identity) = bind_identity {
                            self.add_allowed_identity_runtime(&identity);
                            match self.persist_allowed_identity(&identity).await {
                                Ok(()) => {
                                    let _ = self
                                        .send(&SendMessage::new(
                                            "✅ Telegram account bound successfully. You can talk to RantaiClaw now.",
                                            &chat_id,
                                        ))
                                        .await;
                                    tracing::info!(
                                        "Telegram: paired and allowlisted identity={identity}"
                                    );
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "Telegram: failed to persist allowlist after bind: {e}"
                                    );
                                    let _ = self
                                        .send(&SendMessage::new(
                                            "⚠️ Bound for this runtime, but failed to persist config. Access may be lost after restart; check config file permissions.",
                                            &chat_id,
                                        ))
                                        .await;
                                }
                            }
                        } else {
                            let _ = self
                                .send(&SendMessage::new(
                                    "❌ Could not identify your Telegram account. Ensure your account has a username or stable user ID, then retry.",
                                    &chat_id,
                                ))
                                .await;
                        }
                    }
                    Ok(None) => {
                        let _ = self
                            .send(&SendMessage::new(
                                "❌ Invalid binding code. Ask operator for the latest code and retry.",
                                &chat_id,
                            ))
                            .await;
                    }
                    Err(lockout_secs) => {
                        let _ = self
                            .send(&SendMessage::new(
                                format!("⏳ Too many invalid attempts. Retry in {lockout_secs}s."),
                                &chat_id,
                            ))
                            .await;
                    }
                }
            } else {
                let _ = self
                    .send(&SendMessage::new(
                        "ℹ️ Telegram pairing is not active. Ask operator to add your user ID to channels_config.telegram.allowed_users in config.toml.",
                        &chat_id,
                    ))
                    .await;
            }
            return;
        }

        tracing::warn!(
            "Telegram: ignoring message from unauthorized user: username={username}, sender_id={}. \
Allowlist Telegram username (without '@') or numeric user ID.",
            sender_id_str.as_deref().unwrap_or("unknown")
        );

        let suggested_identity = normalized_sender_id
            .clone()
            .or_else(|| {
                if normalized_username.is_empty() || normalized_username == "unknown" {
                    None
                } else {
                    Some(normalized_username.clone())
                }
            })
            .unwrap_or_else(|| "YOUR_TELEGRAM_ID".to_string());

        let _ = self
            .send(&SendMessage::new(
                format!(
                    "🔐 This bot requires operator approval.\n\nCopy this command to operator terminal:\n`rantaiclaw channel bind-telegram {suggested_identity}`\n\nAfter operator runs it, send your message again."
                ),
                &chat_id,
            ))
            .await;

        if self.pairing_code_active() {
            let _ = self
                .send(&SendMessage::new(
                    "ℹ️ If operator provides a one-time pairing code, you can also run `/bind <code>`.",
                    &chat_id,
                ))
                .await;
        }
    }

    fn parse_update_message(
        &self,
        update: &serde_json::Value,
    ) -> Option<(ChannelMessage, Option<String>)> {
        let message = update.get("message")?;

        // Support both text messages and photo messages (with optional caption)
        let text_opt = message.get("text").and_then(serde_json::Value::as_str);
        let caption_opt = message.get("caption").and_then(serde_json::Value::as_str);

        // Extract file_id from photo (highest resolution = last element)
        let photo_file_id = message
            .get("photo")
            .and_then(serde_json::Value::as_array)
            .and_then(|photos| photos.last())
            .and_then(|p| p.get("file_id"))
            .and_then(serde_json::Value::as_str)
            .map(|s| s.to_string());

        // Require at least text, caption, or photo
        let text = match (text_opt, caption_opt, &photo_file_id) {
            (Some(t), _, _) => t.to_string(),
            (None, Some(c), _) => c.to_string(),
            (None, None, Some(_)) => String::new(), // will be filled with image marker later
            (None, None, None) => return None,
        };

        let username = message
            .get("from")
            .and_then(|from| from.get("username"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let sender_id = message
            .get("from")
            .and_then(|from| from.get("id"))
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string());

        // Prefer the numeric id: a Telegram `@username` can be released and
        // re-registered, so whoever takes the handle would inherit whatever
        // that handle was granted. The id cannot be transferred.
        let sender_identity = sender_id
            .clone()
            .unwrap_or_else(|| username.clone())
            .to_string();

        // The alternate identity form for `sender` (the username when `sender`
        // is the numeric id), so the owner gate can recognize an owner
        // recorded under either form — the same two-form logic the chat
        // allowlist already applies below.
        let sender_aliases: Vec<String> = if username != "unknown" && username != sender_identity {
            vec![username.clone()]
        } else {
            Vec::new()
        };

        let mut identities = vec![username.as_str()];
        if let Some(id) = sender_id.as_deref() {
            identities.push(id);
        }

        if !self.is_any_user_allowed(identities.iter().copied()) {
            return None;
        }

        let is_group = Self::is_group_message(message);
        if self.mention_only && is_group {
            let bot_username = self.bot_username.lock();
            if let Some(ref bot_username) = *bot_username {
                if !Self::contains_bot_mention(&text, bot_username) {
                    return None;
                }
            } else {
                return None;
            }
        }

        let chat_id = message
            .get("chat")
            .and_then(|chat| chat.get("id"))
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string())?;

        let message_id = message
            .get("message_id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);

        // Extract thread/topic ID for forum support
        let thread_id = message
            .get("message_thread_id")
            .and_then(serde_json::Value::as_i64)
            .map(|id| id.to_string());

        // reply_target: chat_id or chat_id:thread_id format
        let reply_target = if let Some(tid) = thread_id {
            format!("{}:{}", chat_id, tid)
        } else {
            chat_id.clone()
        };

        let content = if self.mention_only && is_group {
            let bot_username = self.bot_username.lock();
            let bot_username = bot_username.as_ref()?;
            Self::normalize_incoming_content(&text, bot_username)?
        } else {
            text.to_string()
        };

        Some((
            ChannelMessage {
                id: format!("telegram_{chat_id}_{message_id}"),
                sender: sender_identity,
                reply_target,
                content,
                channel: "telegram".to_string(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                // The reply ANCHOR, not the forum topic. The topic is a
                // destination and stays in `reply_target` (`chat_id:thread_id`)
                // — carrying it in both fields would let the two disagree.
                thread_ts: if message_id == 0 {
                    None
                } else {
                    Some(message_id.to_string())
                },
                sender_aliases,
            },
            photo_file_id,
        ))
    }

    /// Download a Telegram photo by file_id, resize to fit within 1024px, and return as base64 data URI.
    /// Resolve a photo `file_id` to an `[IMAGE:…]` marker, or to the note the
    /// user should see.
    ///
    /// The fetch goes through `channels::media`, so it obeys the operator's
    /// `[multimodal].max_image_size_mb` (this path used to carry its own 25 MiB
    /// constant), reads the body **bounded**, and decides the type from the
    /// bytes. The caller used to drop any failure with `if let Ok(..)`, which
    /// is the silent-drop the policy forbids.
    async fn resolve_photo_marker(
        &self,
        file_id: &str,
        sender: &str,
    ) -> crate::channels::media::MediaOutcome {
        use crate::channels::media::{ImageBytes, MediaOutcome};
        use base64::Engine as _;

        // Budget first: `getFile` is an authenticated round trip, and the
        // charge inside the fetch below happens a request too late to spare an
        // exhausted sender that one.
        if let Err(note) = crate::channels::media::peek(&format!("telegram:{sender}")) {
            return MediaOutcome::Rejected(note);
        }

        // Step 1: call getFile to get file_path
        let get_file_url = self.api_url(&format!("getFile?file_id={}", file_id));
        let Ok(resp) = self.http_client().get(&get_file_url).send().await else {
            return MediaOutcome::Rejected("Attachment unavailable: media fetch failed".into());
        };
        let Ok(json) = resp.json::<serde_json::Value>().await else {
            return MediaOutcome::Rejected(
                "Attachment unavailable: getFile returned no file path".into(),
            );
        };
        let Some(file_path) = json
            .get("result")
            .and_then(|r| r.get("file_path"))
            .and_then(|p| p.as_str())
            .map(str::to_string)
        else {
            return MediaOutcome::Rejected(
                "Attachment unavailable: getFile returned no file path".into(),
            );
        };

        // Step 2: download under the shared policy.
        let download_url = format!("{}/file/bot{}/{}", self.api_base, self.bot_token, file_path);
        let bytes = match crate::channels::media::fetch_image_bytes(
            &self.http_client(),
            &download_url,
            None,
            None,
            crate::channels::media::max_bytes(&self.multimodal),
            &format!("telegram:{sender}"),
        )
        .await
        {
            ImageBytes::Ok { bytes, .. } => bytes,
            ImageBytes::Rejected(note) => return MediaOutcome::Rejected(note),
        };

        // Step 3: resize to max 512px on longest side to fit within model context.
        let resize = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            // Bound the decode: a small, heavily-compressed image can declare huge
            // dimensions and force a multi-GB pixel allocation (decompression
            // bomb) before the thumbnail step. Cap dimensions + total allocation.
            let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes.as_slice()))
                .with_guessed_format()
                .map_err(|e| anyhow::anyhow!("failed to read image header: {e}"))?;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(16_384);
            limits.max_image_height = Some(16_384);
            limits.max_alloc = Some(256 * 1024 * 1024);
            reader.limits(limits);
            let img = reader.decode()?;
            let (w, h) = (img.width(), img.height());
            let max_dim = 512u32;
            let resized = if w > max_dim || h > max_dim {
                img.thumbnail(max_dim, max_dim)
            } else {
                img
            };
            let mut buf = Vec::new();
            resized.write_to(
                &mut std::io::Cursor::new(&mut buf),
                image::ImageFormat::Jpeg,
            )?;
            Ok(buf)
        })
        .await;

        let resized_bytes = match resize {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                tracing::warn!("Telegram: could not decode an inbound photo: {error}");
                return MediaOutcome::Rejected(
                    "Attachment rejected: the image could not be decoded".into(),
                );
            }
            Err(error) => {
                tracing::warn!("Telegram: photo resize task failed: {error}");
                return MediaOutcome::Rejected(
                    "Attachment unavailable: the image could not be processed".into(),
                );
            }
        };

        let b64 = base64::engine::general_purpose::STANDARD.encode(&resized_bytes);
        MediaOutcome::Image(format!("data:image/jpeg;base64,{b64}"))
    }

    async fn send_text_chunks(
        &self,
        message: &str,
        chat_id: &str,
        thread_id: Option<&str>,
        reply_anchor: Option<&str>,
    ) -> anyhow::Result<()> {
        use crate::channels::format::{render_pair, split_paired, RenderTarget};

        let limit = TELEGRAM_MAX_MESSAGE_LENGTH - TELEGRAM_CONTINUATION_OVERHEAD;
        // Primary target comes from the trait seam (one source of truth); the twin
        // is always Plain — the universal fallback every platform accepts.
        let (html_blocks, plain_blocks) =
            render_pair(message, &self.render_target(), &RenderTarget::Plain);
        let pairs = split_paired(&html_blocks, &plain_blocks, limit);
        let total = pairs.len();

        for (index, (html, plain)) in pairs.iter().enumerate() {
            let decorated = decorate_continuation(html, index, total);
            let mut html_body = serde_json::json!({
                "chat_id": chat_id,
                "text": decorated,
                "parse_mode": "HTML",
            });
            if let Some(tid) = thread_id {
                html_body["message_thread_id"] = serde_json::Value::String(tid.to_string());
            }
            // Only the first chunk replies to the prompt; anchoring each chunk
            // renders as N replies to one message.
            if index == 0 {
                if let Some(anchor) = reply_anchor {
                    html_body["reply_parameters"] = serde_json::json!({
                        "message_id": anchor.parse::<i64>().unwrap_or_default(),
                        // The user may have deleted the message between prompt
                        // and reply; without this the whole send fails.
                        "allow_sending_without_reply": true,
                    });
                }
            }

            let resp = self
                .http_client()
                .post(self.api_url("sendMessage"))
                .json(&html_body)
                .send()
                .await?;

            if resp.status().is_success() {
                if index + 1 < total {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                continue;
            }

            let html_status = resp.status();
            let html_err = resp.text().await.unwrap_or_default();
            tracing::warn!(status = ?html_status, "Telegram HTML send failed; retrying as plain");

            // An empty twin means split_paired had no fallback for this chunk —
            // never send the unrendered HTML.
            if plain.is_empty() {
                anyhow::bail!("Telegram sendMessage failed ({html_status}): {html_err}");
            }

            let mut plain_body = serde_json::json!({
                "chat_id": chat_id,
                "text": decorate_continuation(plain, index, total),
            });
            if let Some(tid) = thread_id {
                plain_body["message_thread_id"] = serde_json::Value::String(tid.to_string());
            }
            let plain_resp = self
                .http_client()
                .post(self.api_url("sendMessage"))
                .json(&plain_body)
                .send()
                .await?;
            if !plain_resp.status().is_success() {
                let status = plain_resp.status();
                let err = plain_resp.text().await.unwrap_or_default();
                anyhow::bail!("Telegram sendMessage failed ({status}): {err}");
            }
            if index + 1 < total {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Ok(())
    }

    async fn send_media_by_url(
        &self,
        method: &str,
        media_field: &str,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
        });
        body[media_field] = serde_json::Value::String(url.to_string());

        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::String(tid.to_string());
        }

        if let Some(cap) = caption {
            body["caption"] = serde_json::Value::String(cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url(method))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram {method} by URL failed: {err}");
        }

        tracing::info!("Telegram {method} sent to {chat_id}: {url}");
        Ok(())
    }

    async fn send_attachment(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        attachment: &TelegramAttachment,
    ) -> anyhow::Result<()> {
        let target = attachment.target.trim();

        if is_http_url(target) {
            return match attachment.kind {
                TelegramAttachmentKind::Image => {
                    self.send_photo_by_url(chat_id, thread_id, target, None)
                        .await
                }
                TelegramAttachmentKind::Document => {
                    self.send_document_by_url(chat_id, thread_id, target, None)
                        .await
                }
                TelegramAttachmentKind::Video => {
                    self.send_video_by_url(chat_id, thread_id, target, None)
                        .await
                }
                TelegramAttachmentKind::Audio => {
                    self.send_audio_by_url(chat_id, thread_id, target, None)
                        .await
                }
                TelegramAttachmentKind::Voice => {
                    self.send_voice_by_url(chat_id, thread_id, target, None)
                        .await
                }
            };
        }

        let path = Path::new(target);
        if !path.exists() {
            anyhow::bail!("Telegram attachment path not found: {target}");
        }

        // Confine local attachments to the workspace: a reply (which a channel
        // guest can influence or prompt-inject) must not exfiltrate arbitrary
        // host files — the config with API keys/bot token, ssh keys — to the
        // chat. Mirrors the file_* tool sandbox. Resolve the active workspace
        // lazily here (attachments are infrequent) and fail closed.
        let (_config_path, workspace_dir) = Config::resolve_active_paths()
            .await
            .context("cannot resolve workspace to validate attachment path")?;
        if !attachment_path_within_workspace(path, &workspace_dir) {
            anyhow::bail!(
                "Telegram attachment path is outside the workspace and was blocked: {target}"
            );
        }

        match attachment.kind {
            TelegramAttachmentKind::Image => self.send_photo(chat_id, thread_id, path, None).await,
            TelegramAttachmentKind::Document => {
                self.send_document(chat_id, thread_id, path, None).await
            }
            TelegramAttachmentKind::Video => self.send_video(chat_id, thread_id, path, None).await,
            TelegramAttachmentKind::Audio => self.send_audio(chat_id, thread_id, path, None).await,
            TelegramAttachmentKind::Voice => self.send_voice(chat_id, thread_id, path, None).await,
        }
    }

    /// Send a document/file to a Telegram chat
    pub async fn send_document(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");

        let file_bytes = tokio::fs::read(file_path).await?;
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendDocument"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendDocument failed: {err}");
        }

        tracing::info!("Telegram document sent to {chat_id}: {file_name}");
        Ok(())
    }

    /// Send a document from bytes (in-memory) to a Telegram chat
    pub async fn send_document_bytes(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_bytes: Vec<u8>,
        file_name: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendDocument"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendDocument failed: {err}");
        }

        tracing::info!("Telegram document sent to {chat_id}: {file_name}");
        Ok(())
    }

    /// Send a photo to a Telegram chat
    pub async fn send_photo(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("photo.jpg");

        let file_bytes = tokio::fs::read(file_path).await?;
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("photo", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendPhoto"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendPhoto failed: {err}");
        }

        tracing::info!("Telegram photo sent to {chat_id}: {file_name}");
        Ok(())
    }

    /// Send a photo from bytes (in-memory) to a Telegram chat
    pub async fn send_photo_bytes(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_bytes: Vec<u8>,
        file_name: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("photo", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendPhoto"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendPhoto failed: {err}");
        }

        tracing::info!("Telegram photo sent to {chat_id}: {file_name}");
        Ok(())
    }

    /// Send a video to a Telegram chat
    pub async fn send_video(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video.mp4");

        let file_bytes = tokio::fs::read(file_path).await?;
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("video", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendVideo"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendVideo failed: {err}");
        }

        tracing::info!("Telegram video sent to {chat_id}: {file_name}");
        Ok(())
    }

    /// Send an audio file to a Telegram chat
    pub async fn send_audio(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio.mp3");

        let file_bytes = tokio::fs::read(file_path).await?;
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("audio", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendAudio"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendAudio failed: {err}");
        }

        tracing::info!("Telegram audio sent to {chat_id}: {file_name}");
        Ok(())
    }

    /// Send a voice message to a Telegram chat
    pub async fn send_voice(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        file_path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("voice.ogg");

        let file_bytes = tokio::fs::read(file_path).await?;
        let part = Part::bytes(file_bytes).file_name(file_name.to_string());

        let mut form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("voice", part);

        if let Some(tid) = thread_id {
            form = form.text("message_thread_id", tid.to_string());
        }

        if let Some(cap) = caption {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendVoice"))
            .multipart(form)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendVoice failed: {err}");
        }

        tracing::info!("Telegram voice sent to {chat_id}: {file_name}");
        Ok(())
    }

    /// Send a file by URL (Telegram will download it)
    pub async fn send_document_by_url(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "document": url
        });

        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::String(tid.to_string());
        }

        if let Some(cap) = caption {
            body["caption"] = serde_json::Value::String(cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendDocument"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendDocument by URL failed: {err}");
        }

        tracing::info!("Telegram document (URL) sent to {chat_id}: {url}");
        Ok(())
    }

    /// Send a photo by URL (Telegram will download it)
    pub async fn send_photo_by_url(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "photo": url
        });

        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::String(tid.to_string());
        }

        if let Some(cap) = caption {
            body["caption"] = serde_json::Value::String(cap.to_string());
        }

        let resp = self
            .http_client()
            .post(self.api_url("sendPhoto"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await?;
            anyhow::bail!("Telegram sendPhoto by URL failed: {err}");
        }

        tracing::info!("Telegram photo (URL) sent to {chat_id}: {url}");
        Ok(())
    }

    /// Send a video by URL (Telegram will download it)
    pub async fn send_video_by_url(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_media_by_url("sendVideo", "video", chat_id, thread_id, url, caption)
            .await
    }

    /// Send an audio file by URL (Telegram will download it)
    pub async fn send_audio_by_url(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_media_by_url("sendAudio", "audio", chat_id, thread_id, url, caption)
            .await
    }

    /// Send a voice message by URL (Telegram will download it)
    pub async fn send_voice_by_url(
        &self,
        chat_id: &str,
        thread_id: Option<&str>,
        url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        self.send_media_by_url("sendVoice", "voice", chat_id, thread_id, url, caption)
            .await
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    fn delivery_instructions(&self) -> Option<&'static str> {
        Some(TELEGRAM_DELIVERY_INSTRUCTIONS)
    }

    fn render_target(&self) -> crate::channels::format::RenderTarget {
        crate::channels::format::RenderTarget::TelegramHtml
    }

    /// Replace the live allowlist from a reloaded config.
    ///
    /// Normalized through the same helper the constructor uses, so an entry
    /// applied here and the same entry supplied at boot end up identical — a
    /// mismatch would make the gate depend on which path wrote it.
    fn apply_allowed_senders(&self, allowed: &[String]) {
        let normalized = Self::normalize_allowed_users(allowed.to_vec());
        if let Ok(mut users) = self.allowed_users.write() {
            if *users != normalized {
                tracing::info!(
                    target: "channels",
                    channel = "telegram",
                    count = normalized.len(),
                    "applied updated allowlist from config"
                );
                *users = normalized;
            }
        }
    }

    fn supports_draft_updates(&self) -> bool {
        self.stream_mode != StreamMode::Off
    }

    async fn send_draft(&self, message: &SendMessage) -> anyhow::Result<Option<String>> {
        if self.stream_mode == StreamMode::Off {
            return Ok(None);
        }

        let (chat_id, thread_id) = Self::parse_reply_target(&message.recipient);
        let initial_text = if message.content.is_empty() {
            "...".to_string()
        } else {
            message.content.clone()
        };

        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": initial_text,
        });
        if let Some(tid) = thread_id {
            body["message_thread_id"] = serde_json::Value::String(tid.to_string());
        }

        let resp = self
            .client
            .post(self.api_url("sendMessage"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("Telegram sendMessage (draft) failed: {err}");
        }

        let resp_json: serde_json::Value = resp.json().await?;
        let message_id = resp_json
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|id| id.as_i64())
            .map(|id| id.to_string());

        self.last_draft_edit
            .lock()
            .insert(chat_id.to_string(), std::time::Instant::now());

        Ok(message_id)
    }

    async fn update_draft(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let (chat_id, _) = Self::parse_reply_target(recipient);

        // Rate-limit edits per chat
        {
            let last_edits = self.last_draft_edit.lock();
            if let Some(last_time) = last_edits.get(&chat_id) {
                let elapsed = u64::try_from(last_time.elapsed().as_millis()).unwrap_or(u64::MAX);
                if elapsed < self.draft_update_interval_ms {
                    return Ok(());
                }
            }
        }

        // Stream Plain, never raw markdown or HTML: a mid-stream edit holds a
        // partial response, and a half-open `**`/`<b>` would send unbalanced
        // markup. Plain has no markup to leave open. Render first, THEN truncate,
        // so the UTF-8-safe cut runs on exactly what is sent.
        use crate::channels::format::{render_to_string, RenderTarget};
        let rendered = render_to_string(text, &RenderTarget::Plain);
        let display_text = Self::draft_display_text(&rendered);

        let message_id_parsed = match message_id.parse::<i64>() {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("Invalid Telegram message_id '{message_id}': {e}");
                return Ok(());
            }
        };

        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id_parsed,
            "text": display_text,
        });

        let resp = self
            .client
            .post(self.api_url("editMessageText"))
            .json(&body)
            .send()
            .await?;

        if resp.status().is_success() {
            self.last_draft_edit
                .lock()
                .insert(chat_id.clone(), std::time::Instant::now());
        } else {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            tracing::debug!("Telegram editMessageText failed ({status}): {err}");
        }

        Ok(())
    }

    async fn finalize_draft(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let text = &strip_tool_call_tags(text);
        let (chat_id, thread_id) = Self::parse_reply_target(recipient);

        // Clean up rate-limit tracking for this chat
        self.last_draft_edit.lock().remove(&chat_id);

        use crate::channels::format::{render_to_string, RenderTarget};
        // Render BEFORE gating: the edit now sends HTML, which is strictly longer
        // than the raw markdown, so a 4000-char reply can render past 4096 and get
        // a 400. Gate on the rendered length in `chars` — the splitter's unit —
        // not raw `len()` bytes. Target from the trait seam, twin always Plain.
        let html = render_to_string(text, &self.render_target());

        // If the rendered text exceeds the limit, delete draft and chunk-send.
        if html.chars().count() > TELEGRAM_MAX_MESSAGE_LENGTH {
            let msg_id = match message_id.parse::<i64>() {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!("Invalid Telegram message_id '{message_id}': {e}");
                    return self
                        .send_text_chunks(text, &chat_id, thread_id.as_deref(), None)
                        .await;
                }
            };

            // Delete the draft
            let _ = self
                .client
                .post(self.api_url("deleteMessage"))
                .json(&serde_json::json!({
                    "chat_id": chat_id,
                    "message_id": msg_id,
                }))
                .send()
                .await;

            // Fall back to chunked send
            return self
                .send_text_chunks(text, &chat_id, thread_id.as_deref(), None)
                .await;
        }

        let msg_id = match message_id.parse::<i64>() {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("Invalid Telegram message_id '{message_id}': {e}");
                return self
                    .send_text_chunks(text, &chat_id, thread_id.as_deref(), None)
                    .await;
            }
        };

        // Edit with the rendered HTML.
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": msg_id,
            "text": html,
            "parse_mode": "HTML",
        });

        let resp = self
            .client
            .post(self.api_url("editMessageText"))
            .json(&body)
            .send()
            .await?;

        if resp.status().is_success() {
            return Ok(());
        }

        // HTML rejected — retry with the Plain render, never the raw markdown.
        let plain = render_to_string(text, &RenderTarget::Plain);
        let plain_body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": msg_id,
            "text": plain,
        });

        let resp = self
            .client
            .post(self.api_url("editMessageText"))
            .json(&plain_body)
            .send()
            .await?;

        if resp.status().is_success() {
            return Ok(());
        }

        // Edit failed entirely — fall back to new message
        tracing::warn!("Telegram finalize_draft edit failed; falling back to sendMessage");
        self.send_text_chunks(text, &chat_id, thread_id.as_deref(), None)
            .await
    }

    async fn cancel_draft(&self, recipient: &str, message_id: &str) -> anyhow::Result<()> {
        let (chat_id, _) = Self::parse_reply_target(recipient);
        self.last_draft_edit.lock().remove(&chat_id);

        let message_id = match message_id.parse::<i64>() {
            Ok(id) => id,
            Err(e) => {
                tracing::debug!("Invalid Telegram draft message_id '{message_id}': {e}");
                return Ok(());
            }
        };

        let response = self
            .client
            .post(self.api_url("deleteMessage"))
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "message_id": message_id,
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::debug!("Telegram deleteMessage failed ({status}): {body}");
        }

        Ok(())
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        // Strip tool_call tags before processing to prevent Markdown parsing failures
        let content = strip_tool_call_tags(&message.content);

        // Parse recipient: "chat_id" or "chat_id:thread_id" format
        let (chat_id, thread_id) = match message.recipient.split_once(':') {
            Some((chat, thread)) => (chat, Some(thread)),
            None => (message.recipient.as_str(), None),
        };

        let (text_without_markers, attachments) = parse_attachment_markers(&content);

        if !attachments.is_empty() {
            if !text_without_markers.is_empty() {
                self.send_text_chunks(
                    &text_without_markers,
                    chat_id,
                    thread_id,
                    message.thread_ts.as_deref(),
                )
                .await?;
            }

            for attachment in &attachments {
                self.send_attachment(chat_id, thread_id, attachment).await?;
            }

            return Ok(());
        }

        if let Some(attachment) = parse_path_only_attachment(&content) {
            self.send_attachment(chat_id, thread_id, &attachment)
                .await?;
            return Ok(());
        }

        self.send_text_chunks(&content, chat_id, thread_id, message.thread_ts.as_deref())
            .await
    }

    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let mut offset: i64 = 0;

        if self.mention_only {
            let _ = self.get_bot_username().await;
        }

        tracing::info!("Telegram channel listening for messages...");

        loop {
            if self.mention_only {
                let missing_username = self.bot_username.lock().is_none();
                if missing_username {
                    let _ = self.get_bot_username().await;
                }
            }

            let url = self.api_url("getUpdates");
            let body = serde_json::json!({
                "offset": offset,
                "timeout": 30,
                "allowed_updates": ["message"]
            });

            let resp = tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!("Telegram channel shutting down");
                    return Ok(());
                }
                result = self.http_client().post(&url).json(&body).send() => {
                    match result {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!("Telegram poll error: {e}");
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            continue;
                        }
                    }
                }
            };

            let data: serde_json::Value = match resp.json().await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("Telegram parse error: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            let ok = data
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            if !ok {
                let error_code = data
                    .get("error_code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or_default();
                let description = data
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown Telegram API error");

                if error_code == 409 {
                    tracing::warn!(
                        "Telegram polling conflict (409): {description}. \
Ensure only one `rantaiclaw` process is using this bot token."
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                } else {
                    tracing::warn!(
                        "Telegram getUpdates API error (code={}): {description}",
                        error_code
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                continue;
            }

            if let Some(results) = data.get("result").and_then(serde_json::Value::as_array) {
                for update in results {
                    // Advance offset past this update
                    if let Some(uid) = update.get("update_id").and_then(serde_json::Value::as_i64) {
                        offset = uid + 1;
                    }

                    // Intercept on-demand store-minted pairing codes first
                    // (`rantaiclaw channels pair`) for both `/bind` and `/claim`
                    // — accepted without a daemon restart. Only consumes the
                    // update when the store actually owns the code; otherwise it
                    // falls through to the legacy in-memory PairingGuard path
                    // below so the startup code keeps working.
                    if let Some((text, chat_id, identities)) = Self::extract_pairing_context(update)
                    {
                        if self
                            .try_handle_store_pairing(&text, &chat_id, &identities)
                            .await
                        {
                            continue;
                        }
                    }

                    // Intercept `/claim <code>` (owner pairing) before routing —
                    // handled regardless of allowlist status, never forwarded.
                    if self.try_handle_claim(update).await {
                        continue;
                    }

                    let Some((mut msg, photo_file_id)) = self.parse_update_message(update) else {
                        self.handle_unauthorized_message(update).await;
                        continue;
                    };

                    // Resolve the photo to a marker — or to a note saying why
                    // it did not resolve. A dropped image used to be silent.
                    if let Some(file_id) = photo_file_id {
                        let marker = self
                            .resolve_photo_marker(&file_id, &msg.sender)
                            .await
                            .to_marker();
                        if msg.content.is_empty() {
                            msg.content = marker;
                        } else {
                            msg.content = format!("{}\n{}", msg.content, marker);
                        }
                    }

                    // Send "typing" indicator immediately when we receive a message
                    let typing_body = serde_json::json!({
                        "chat_id": &msg.reply_target,
                        "action": "typing"
                    });
                    let _ = self
                        .http_client()
                        .post(self.api_url("sendChatAction"))
                        .json(&typing_body)
                        .send()
                        .await; // Ignore errors for typing indicator

                    if tx.send(msg).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn health_check(&self) -> bool {
        let timeout_duration = Duration::from_secs(5);

        match tokio::time::timeout(
            timeout_duration,
            self.http_client().get(self.api_url("getMe")).send(),
        )
        .await
        {
            Ok(Ok(resp)) => resp.status().is_success(),
            Ok(Err(e)) => {
                tracing::debug!("Telegram health check failed: {e}");
                false
            }
            Err(_) => {
                tracing::debug!("Telegram health check timed out after 5s");
                false
            }
        }
    }

    /// One `sendChatAction` POST per call.
    ///
    /// There used to be a self-refreshing 4-second loop in here as well, while
    /// the runtime's own `spawn_scoped_typing_task` calls this on a 4-second
    /// interval — so every runtime tick aborted the task spawned four seconds
    /// earlier and spawned another. The runtime cadence (4s) is inside
    /// Telegram's ~5s indicator expiry, so the inner loop was redundant as
    /// well as self-defeating.
    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        self.stop_typing(recipient).await?;

        let client = self.http_client();
        let url = self.api_url("sendChatAction");
        let chat_id = recipient.to_string();

        // Spawned rather than awaited so a slow POST does not hold up the
        // reply it is announcing; the handle is kept so `stop_typing` can
        // cancel one still in flight.
        let handle = tokio::spawn(async move {
            let body = serde_json::json!({
                "chat_id": &chat_id,
                "action": "typing"
            });
            let _ = client.post(&url).json(&body).send().await;
        });

        self.typing_handles
            .lock()
            .insert(recipient.to_string(), handle);

        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()> {
        if let Some(handle) = self.typing_handles.lock().remove(recipient) {
            handle.abort();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── shared-store pairing (Task 4) ────────────────────────

    /// `extract_pairing_context` pulls `[numeric_id, username]` (both forms)
    /// plus the text and chat id from a representative Telegram update.
    #[test]
    fn extract_pairing_context_collects_both_identity_forms() {
        let update = serde_json::json!({
            "message": {
                "text": "/claim ABCD-EFGH",
                "chat": { "id": 4242 },
                "from": { "id": 999, "username": "carol" }
            }
        });
        let (text, chat_id, identities) =
            TelegramChannel::extract_pairing_context(&update).expect("should extract");
        assert_eq!(text, "/claim ABCD-EFGH");
        assert_eq!(chat_id, "4242");
        assert_eq!(identities, vec!["999".to_string(), "carol".to_string()]);
    }

    /// A `/bind` with no live store code falls through (returns false) so the
    /// legacy in-memory PairingGuard path still owns the startup code. This also
    /// exercises that no network reply is sent on the fall-through.
    #[tokio::test]
    async fn store_pairing_falls_through_when_no_store_code() {
        let ch = TelegramChannel::new("t".into(), vec![], false);
        let handled = ch
            .try_handle_store_pairing("/bind ABCD-EFGH", "123", &["999".to_string()])
            .await;
        assert!(
            !handled,
            "no store code => must fall through to legacy path"
        );
    }

    /// A store-minted "telegram" code (the kind `rantaiclaw channels pair`
    /// issues) is accepted on `/claim`: the shared core lands the sender in
    /// `allowed_users` AND `approval_owners`. Drives the same code path
    /// `try_handle_store_pairing` invokes after its `contains` gate, without the
    /// network `send`.
    #[tokio::test]
    async fn store_minted_telegram_code_claims_owner() {
        use crate::channels::pairing::{try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");

        // Seed a config with a telegram section so apply_pairing has a target.
        {
            let mut seed = crate::config::Config::load_or_init().await.unwrap();
            seed.channels_config.telegram = Some(crate::config::TelegramConfig {
                bot_token: "x".into(),
                allowed_users: vec![],
                stream_mode: crate::config::StreamMode::Off,
                draft_update_interval_ms: 500,
                interrupt_on_new_message: false,
                mention_only: false,
            });
            seed.save().await.unwrap();
        }

        // Mint an owner-capable "telegram" code into the same profile root.
        // `try_handle_pairing` consumes against the real wall clock, so mint at
        // real `now` with a generous TTL and probe the gate the same way.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let code = pairing_store::mint(root, "telegram", 3_600, None, true, now).unwrap();
        assert!(pairing_store::contains(root, "telegram", &code, now + 1).unwrap());

        let reply = try_handle_pairing(
            &format!("/claim {code}"),
            "telegram",
            AllowlistField::AllowedUsers,
            &["999".to_string(), "carol".to_string()],
            root,
        )
        .await
        .expect("a /claim must be handled");
        assert!(reply.contains("owner"), "reply was: {reply}");

        let config = crate::config::Config::load_or_init().await.unwrap();
        let users = &config
            .channels_config
            .telegram
            .as_ref()
            .unwrap()
            .allowed_users;
        assert!(users.contains(&"999".to_string()), "users: {users:?}");
        assert!(users.contains(&"carol".to_string()), "users: {users:?}");
        let owners = &config.channels_config.approval_owners;
        assert!(owners.contains(&"999".to_string()), "owners: {owners:?}");
        assert!(owners.contains(&"carol".to_string()), "owners: {owners:?}");

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }

    /// The legacy `/claim`/`/bind` owner-persist path must write to the config
    /// file the daemon actually reads (profile / env-dir aware), not a hardcoded
    /// `~/.rantaiclaw/config.toml`. Regression for the wrong-path bug: before the
    /// fix this either failed (legacy file absent) or wrote where the daemon
    /// never looks, so `approval_owners` stayed empty across restarts.
    #[tokio::test]
    async fn legacy_owner_persist_targets_the_active_config_dir() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");

        // Seed a config in the ACTIVE dir (this is where the daemon reads).
        crate::config::Config::load_or_init()
            .await
            .unwrap()
            .save()
            .await
            .unwrap();

        // Persist an owner via the legacy path (what `/claim` calls).
        let ch = TelegramChannel::new("t".into(), vec![], false);
        ch.persist_approval_owner(&["999".to_string()])
            .await
            .expect("owner persist should target the active config and succeed");

        // The daemon (load_or_init) must now see the owner.
        let config = crate::config::Config::load_or_init().await.unwrap();
        assert!(
            config
                .channels_config
                .approval_owners
                .contains(&"999".to_string()),
            "legacy owner-persist must land in the active config dir; owners: {:?}",
            config.channels_config.approval_owners
        );

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }

    #[test]
    fn telegram_channel_name() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);
        assert_eq!(ch.name(), "telegram");
    }

    /// The gate read raw bytes against a character limit, so a CJK reply was
    /// cut at roughly a third of its intended length.
    #[test]
    fn draft_gate_counts_characters_not_bytes() {
        // Just under the character limit, far over the byte limit: 3 bytes per
        // character.
        let cjk = "字".repeat(TELEGRAM_MAX_MESSAGE_LENGTH - 1);
        assert!(cjk.len() > TELEGRAM_MAX_MESSAGE_LENGTH, "precondition");
        assert_eq!(
            TelegramChannel::draft_display_text(&cjk),
            cjk,
            "a reply inside the character limit must not be truncated"
        );

        // Over the character limit: cut to exactly the limit, on a boundary.
        let over = "字".repeat(TELEGRAM_MAX_MESSAGE_LENGTH + 10);
        let cut = TelegramChannel::draft_display_text(&over);
        assert_eq!(cut.chars().count(), TELEGRAM_MAX_MESSAGE_LENGTH);
        assert!(over.starts_with(cut));
    }

    #[test]
    fn typing_handles_start_empty() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);
        assert!(ch.typing_handles.lock().is_empty());
    }

    #[tokio::test]
    async fn stop_typing_clears_that_recipients_handle() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);

        ch.typing_handles.lock().insert(
            "123".to_string(),
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_mins(1)).await;
            }),
        );

        ch.stop_typing("123").await.unwrap();

        assert!(ch.typing_handles.lock().is_empty());
    }

    /// Two concurrent conversations used to share one typing slot, so starting
    /// typing for chat B silently killed chat A's indicator. With the runtime's
    /// parallel message path that is the normal case, not an edge case.
    #[tokio::test]
    async fn concurrent_typing_handles_are_independent() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);

        for chat in ["chat_a", "chat_b"] {
            ch.typing_handles.lock().insert(
                chat.to_string(),
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_mins(1)).await;
                }),
            );
        }

        ch.stop_typing("chat_b").await.unwrap();

        let guard = ch.typing_handles.lock();
        assert!(
            guard.contains_key("chat_a"),
            "stopping B must leave A's indicator alone"
        );
        assert!(!guard.contains_key("chat_b"));
    }

    #[tokio::test]
    async fn stop_typing_only_stops_that_recipient() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);
        ch.typing_handles.lock().insert(
            "chat_a".to_string(),
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_mins(1)).await;
            }),
        );

        // A recipient with no indicator running is a no-op, not a clear-all.
        ch.stop_typing("chat_zzz").await.unwrap();

        assert!(ch.typing_handles.lock().contains_key("chat_a"));
    }

    #[tokio::test]
    async fn start_typing_replaces_that_recipients_handle_only() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);

        for chat in ["123", "456"] {
            ch.typing_handles.lock().insert(
                chat.to_string(),
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_mins(1)).await;
                }),
            );
        }

        let _ = ch.start_typing("123").await;

        let guard = ch.typing_handles.lock();
        assert!(guard.contains_key("123"));
        assert!(
            guard.contains_key("456"),
            "another chat's indicator must survive"
        );
    }

    #[test]
    fn supports_draft_updates_respects_stream_mode() {
        let off = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);
        assert!(!off.supports_draft_updates());

        let partial = TelegramChannel::new("fake-token".into(), vec!["*".into()], false)
            .with_streaming(StreamMode::Partial, 750);
        assert!(partial.supports_draft_updates());
        assert_eq!(partial.draft_update_interval_ms, 750);
    }

    #[tokio::test]
    async fn send_draft_returns_none_when_stream_mode_off() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);
        let id = ch
            .send_draft(&SendMessage::new("draft", "123"))
            .await
            .unwrap();
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn update_draft_rate_limit_short_circuits_network() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false)
            .with_streaming(StreamMode::Partial, 60_000);
        ch.last_draft_edit
            .lock()
            .insert("123".to_string(), std::time::Instant::now());

        let result = ch.update_draft("123", "42", "delta text").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn update_draft_utf8_truncation_is_safe_for_multibyte_text() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false)
            .with_streaming(StreamMode::Partial, 0);
        let long_emoji_text = "😀".repeat(TELEGRAM_MAX_MESSAGE_LENGTH + 20);

        // Invalid message_id returns early after building display_text.
        // This asserts truncation never panics on UTF-8 boundaries.
        let result = ch
            .update_draft("123", "not-a-number", &long_emoji_text)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn finalize_draft_invalid_message_id_falls_back_to_chunk_send() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false)
            .with_streaming(StreamMode::Partial, 0);
        let long_text = "a".repeat(TELEGRAM_MAX_MESSAGE_LENGTH + 64);

        // For oversized text + invalid draft message_id, finalize_draft should
        // fall back to chunked send instead of returning early.
        let result = ch.finalize_draft("123", "not-a-number", &long_text).await;
        assert!(result.is_err());
    }

    #[test]
    fn telegram_api_url() {
        let ch = TelegramChannel::new("123:ABC".into(), vec![], false);
        assert_eq!(
            ch.api_url("getMe"),
            format!("{TELEGRAM_API_BASE}/bot123:ABC/getMe")
        );
    }

    #[test]
    fn telegram_user_allowed_wildcard() {
        let ch = TelegramChannel::new("t".into(), vec!["*".into()], false);
        assert!(ch.is_user_allowed("anyone"));
    }

    #[test]
    fn telegram_user_allowed_specific() {
        let ch = TelegramChannel::new("t".into(), vec!["alice".into(), "bob".into()], false);
        assert!(ch.is_user_allowed("alice"));
        assert!(!ch.is_user_allowed("eve"));
    }

    #[test]
    fn telegram_user_allowed_with_at_prefix_in_config() {
        let ch = TelegramChannel::new("t".into(), vec!["@alice".into()], false);
        assert!(ch.is_user_allowed("alice"));
    }

    #[test]
    fn telegram_user_denied_empty() {
        let ch = TelegramChannel::new("t".into(), vec![], false);
        assert!(!ch.is_user_allowed("anyone"));
    }

    #[test]
    fn telegram_user_exact_match_not_substring() {
        let ch = TelegramChannel::new("t".into(), vec!["alice".into()], false);
        assert!(!ch.is_user_allowed("alice_bot"));
        assert!(!ch.is_user_allowed("alic"));
        assert!(!ch.is_user_allowed("malice"));
    }

    #[test]
    fn telegram_user_empty_string_denied() {
        let ch = TelegramChannel::new("t".into(), vec!["alice".into()], false);
        assert!(!ch.is_user_allowed(""));
    }

    #[test]
    fn telegram_user_case_sensitive() {
        let ch = TelegramChannel::new("t".into(), vec!["Alice".into()], false);
        assert!(ch.is_user_allowed("Alice"));
        assert!(!ch.is_user_allowed("alice"));
        assert!(!ch.is_user_allowed("ALICE"));
    }

    #[test]
    fn telegram_wildcard_with_specific_users() {
        let ch = TelegramChannel::new("t".into(), vec!["alice".into(), "*".into()], false);
        assert!(ch.is_user_allowed("alice"));
        assert!(ch.is_user_allowed("bob"));
        assert!(ch.is_user_allowed("anyone"));
    }

    #[test]
    fn telegram_user_allowed_by_numeric_id_identity() {
        let ch = TelegramChannel::new("t".into(), vec!["123456789".into()], false);
        assert!(ch.is_any_user_allowed(["unknown", "123456789"]));
    }

    #[test]
    fn telegram_user_denied_when_none_of_identities_match() {
        let ch = TelegramChannel::new("t".into(), vec!["alice".into(), "987654321".into()], false);
        assert!(!ch.is_any_user_allowed(["unknown", "123456789"]));
    }

    /// An entry applied through the runtime path and the same entry supplied at
    /// construction must end up in the identical stored form. If they diverge,
    /// whether a sender is allowed depends on which path wrote the list.
    #[test]
    fn telegram_apply_allowed_senders_normalizes_like_the_constructor() {
        use crate::channels::traits::Channel;

        let via_ctor = TelegramChannel::new(
            "111:aaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            vec!["@Alice".to_string(), "  bob  ".to_string()],
            false,
        );
        let via_runtime =
            TelegramChannel::new("111:aaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), vec![], false);
        via_runtime.apply_allowed_senders(&["@Alice".to_string(), "  bob  ".to_string()]);

        let ctor_list = via_ctor.allowed_users.read().expect("ctor lock").clone();
        let runtime_list = via_runtime
            .allowed_users
            .read()
            .expect("runtime lock")
            .clone();
        assert_eq!(
            ctor_list, runtime_list,
            "runtime-applied allowlist must normalize identically to the constructor's"
        );
    }

    #[test]
    fn telegram_pairing_enabled_with_empty_allowlist() {
        let ch = TelegramChannel::new("t".into(), vec![], false);
        assert!(ch.pairing_code_active());
    }

    #[test]
    fn telegram_pairing_disabled_with_nonempty_allowlist() {
        let ch = TelegramChannel::new("t".into(), vec!["alice".into()], false);
        assert!(!ch.pairing_code_active());
    }

    #[test]
    fn telegram_extract_bind_code_plain_command() {
        assert_eq!(
            TelegramChannel::extract_bind_code("/bind 123456"),
            Some("123456")
        );
    }

    #[test]
    fn telegram_extract_claim_code_parses_and_rejects() {
        assert_eq!(
            TelegramChannel::extract_claim_code("/claim 654321"),
            Some("654321")
        );
        assert_eq!(
            TelegramChannel::extract_claim_code("/claim@rantaiclaw_bot 99"),
            Some("99")
        );
        // Not a claim / missing code.
        assert_eq!(TelegramChannel::extract_claim_code("/claim"), None);
        assert_eq!(TelegramChannel::extract_claim_code("/bind 123456"), None);
        assert_eq!(TelegramChannel::extract_claim_code("hello"), None);
    }

    #[test]
    fn telegram_extract_bind_code_supports_bot_mention() {
        assert_eq!(
            TelegramChannel::extract_bind_code("/bind@rantaiclaw_bot 654321"),
            Some("654321")
        );
    }

    #[test]
    fn telegram_extract_bind_code_rejects_invalid_forms() {
        assert_eq!(TelegramChannel::extract_bind_code("/bind"), None);
        assert_eq!(TelegramChannel::extract_bind_code("/start"), None);
    }

    #[test]
    fn parse_attachment_markers_extracts_multiple_types() {
        let message = "Here are files [IMAGE:/tmp/a.png] and [DOCUMENT:https://example.com/a.pdf]";
        let (cleaned, attachments) = parse_attachment_markers(message);

        assert_eq!(cleaned, "Here are files  and");
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].kind, TelegramAttachmentKind::Image);
        assert_eq!(attachments[0].target, "/tmp/a.png");
        assert_eq!(attachments[1].kind, TelegramAttachmentKind::Document);
        assert_eq!(attachments[1].target, "https://example.com/a.pdf");
    }

    #[test]
    fn parse_attachment_markers_keeps_invalid_markers_in_text() {
        let message = "Report [UNKNOWN:/tmp/a.bin]";
        let (cleaned, attachments) = parse_attachment_markers(message);

        assert_eq!(cleaned, "Report [UNKNOWN:/tmp/a.bin]");
        assert!(attachments.is_empty());
    }

    #[test]
    fn parse_path_only_attachment_detects_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let image_path = dir.path().join("snap.png");
        std::fs::write(&image_path, b"fake-png").unwrap();

        let parsed = parse_path_only_attachment(image_path.to_string_lossy().as_ref())
            .expect("expected attachment");

        assert_eq!(parsed.kind, TelegramAttachmentKind::Image);
        assert_eq!(parsed.target, image_path.to_string_lossy());
    }

    #[test]
    fn parse_path_only_attachment_rejects_sentence_text() {
        assert!(parse_path_only_attachment("Screenshot saved to /tmp/snap.png").is_none());
    }

    #[test]
    fn attachment_path_within_workspace_allows_file_inside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("report.pdf");
        std::fs::write(&file, b"data").unwrap();
        assert!(attachment_path_within_workspace(&file, workspace.path()));
    }

    #[test]
    fn attachment_path_within_workspace_blocks_file_outside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("config.toml");
        std::fs::write(&secret, b"bot_token = \"secret\"").unwrap();
        // A reply containing [DOCUMENT:<secret>] must not read a file living
        // outside the workspace and upload it to the chat.
        assert!(!attachment_path_within_workspace(&secret, workspace.path()));
    }

    #[test]
    fn attachment_path_within_workspace_blocks_symlink_escape() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, b"PRIVATE KEY").unwrap();
        let link = workspace.path().join("innocent.txt");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&secret, &link).unwrap();
            // canonicalize resolves the symlink to its out-of-workspace target.
            assert!(!attachment_path_within_workspace(&link, workspace.path()));
        }
    }

    #[test]
    fn attachment_path_within_workspace_fails_closed_on_missing_file() {
        let workspace = tempfile::tempdir().unwrap();
        let missing = workspace.path().join("does-not-exist");
        assert!(!attachment_path_within_workspace(
            &missing,
            workspace.path()
        ));
    }

    #[test]
    fn infer_attachment_kind_from_target_detects_document_extension() {
        assert_eq!(
            infer_attachment_kind_from_target("https://example.com/files/specs.pdf?download=1"),
            Some(TelegramAttachmentKind::Document)
        );
    }

    /// `is_any_user_allowed` was tested on its own, never through the function
    /// `listen()` actually calls. Deleting the gate from `parse_update_message`
    /// left the suite green.
    #[test]
    fn parse_update_message_drops_an_unlisted_sender() {
        let ch = TelegramChannel::new("token".into(), vec!["555".into()], false);
        let update = |id: i64| {
            serde_json::json!({
                "update_id": 1,
                "message": {
                    "message_id": 33,
                    "text": "status please",
                    "from": { "id": id },
                    "chat": { "id": id }
                }
            })
        };

        assert!(
            ch.parse_update_message(&update(999)).is_none(),
            "a sender outside allowed_users must not produce a message"
        );
        // Control on the same fixture: the allowlisted id does.
        let (msg, _) = ch
            .parse_update_message(&update(555))
            .expect("the allowlisted sender parses");
        assert_eq!(msg.sender, "555");
        assert_eq!(msg.content, "status please");
    }

    #[test]
    fn parse_update_message_uses_chat_id_as_reply_target() {
        let ch = TelegramChannel::new("token".into(), vec!["*".into()], false);
        let update = serde_json::json!({
            "update_id": 1,
            "message": {
                "message_id": 33,
                "text": "hello",
                "from": {
                    "id": 555,
                    "username": "alice"
                },
                "chat": {
                    "id": -100_200_300
                }
            }
        });

        let msg = ch
            .parse_update_message(&update)
            .map(|(m, _)| m)
            .expect("message should parse");

        // `sender` is the numeric id: a username can be released and
        // re-registered, the id cannot.
        assert_eq!(msg.sender, "555");
        assert_eq!(msg.reply_target, "-100200300");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.id, "telegram_-100200300_33");
    }

    #[test]
    fn parse_update_message_allows_numeric_id_without_username() {
        let ch = TelegramChannel::new("token".into(), vec!["555".into()], false);
        let update = serde_json::json!({
            "update_id": 2,
            "message": {
                "message_id": 9,
                "text": "ping",
                "from": {
                    "id": 555
                },
                "chat": {
                    "id": 12345
                }
            }
        });

        let msg = ch
            .parse_update_message(&update)
            .map(|(m, _)| m)
            .expect("numeric allowlist should pass");

        assert_eq!(msg.sender, "555");
        assert_eq!(msg.reply_target, "12345");
    }

    fn update_from(id: i64, username: &str) -> serde_json::Value {
        serde_json::json!({
            "update_id": 4,
            "message": {
                "message_id": 7,
                "text": "hi",
                "from": { "id": id, "username": username },
                "chat": { "id": 4242 }
            }
        })
    }

    /// A Telegram `@username` can be released and re-registered, and pairing
    /// writes whatever form the channel reports into `approval_owners` — so
    /// reporting the handle meant whoever took it inherited owner authority.
    #[test]
    fn sender_is_the_numeric_id_and_the_username_is_an_alias() {
        let ch = TelegramChannel::new("token".into(), vec!["*".into()], false);
        let msg = ch
            .parse_update_message(&update_from(1_360_247_715, "rantaiclaw_user"))
            .map(|(m, _)| m)
            .expect("wildcard allowlist should pass");

        assert_eq!(msg.sender, "1360247715");
        assert_eq!(msg.sender_aliases, vec!["rantaiclaw_user".to_string()]);
    }

    /// The alias path must keep working, or the swap silently demotes every
    /// owner recorded by handle.
    #[test]
    fn owner_listed_by_username_is_still_recognised() {
        let ch = TelegramChannel::new("token".into(), vec!["*".into()], false);
        let msg = ch
            .parse_update_message(&update_from(1_360_247_715, "rantaiclaw_user"))
            .map(|(m, _)| m)
            .expect("wildcard allowlist should pass");

        let owners = vec!["rantaiclaw_user".to_string()];
        assert!(crate::approval::can_approve_any(
            &owners,
            msg.sender_identities()
        ));
        // And an owner recorded by numeric id, which is now the primary form.
        let owners = vec!["1360247715".to_string()];
        assert!(crate::approval::can_approve(&owners, &msg.sender));
    }

    #[test]
    fn parse_update_message_extracts_thread_id_for_forum_topic() {
        let ch = TelegramChannel::new("token".into(), vec!["*".into()], false);
        let update = serde_json::json!({
            "update_id": 3,
            "message": {
                "message_id": 42,
                "text": "hello from topic",
                "from": {
                    "id": 555,
                    "username": "alice"
                },
                "chat": {
                    "id": -100_200_300
                },
                "message_thread_id": 789
            }
        });

        let msg = ch
            .parse_update_message(&update)
            .map(|(m, _)| m)
            .expect("message with thread_id should parse");

        assert_eq!(msg.sender, "555");
        assert_eq!(msg.reply_target, "-100200300:789");
        assert_eq!(msg.content, "hello from topic");
        assert_eq!(msg.id, "telegram_-100200300_42");
    }

    // ── File sending API URL tests ──────────────────────────────────

    #[test]
    fn telegram_api_url_send_document() {
        let ch = TelegramChannel::new("123:ABC".into(), vec![], false);
        assert_eq!(
            ch.api_url("sendDocument"),
            format!("{TELEGRAM_API_BASE}/bot123:ABC/sendDocument")
        );
    }

    #[test]
    fn telegram_api_url_send_photo() {
        let ch = TelegramChannel::new("123:ABC".into(), vec![], false);
        assert_eq!(
            ch.api_url("sendPhoto"),
            format!("{TELEGRAM_API_BASE}/bot123:ABC/sendPhoto")
        );
    }

    #[test]
    fn telegram_api_url_send_video() {
        let ch = TelegramChannel::new("123:ABC".into(), vec![], false);
        assert_eq!(
            ch.api_url("sendVideo"),
            format!("{TELEGRAM_API_BASE}/bot123:ABC/sendVideo")
        );
    }

    #[test]
    fn telegram_api_url_send_audio() {
        let ch = TelegramChannel::new("123:ABC".into(), vec![], false);
        assert_eq!(
            ch.api_url("sendAudio"),
            format!("{TELEGRAM_API_BASE}/bot123:ABC/sendAudio")
        );
    }

    #[test]
    fn telegram_api_url_send_voice() {
        let ch = TelegramChannel::new("123:ABC".into(), vec![], false);
        assert_eq!(
            ch.api_url("sendVoice"),
            format!("{TELEGRAM_API_BASE}/bot123:ABC/sendVoice")
        );
    }

    // ── File sending integration tests (with mock server) ──────────

    /// A local stand-in for the Bot API. The tests below used to POST to
    /// `api.telegram.org` with a fake token and assert only `is_err()` — which
    /// is what a malformed request, an empty body and an unplugged network all
    /// produce, so they asserted nothing about what was sent.
    async fn spawn_bot_api() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    ) {
        use axum::body::Bytes;
        use axum::extract::State;
        use axum::http::Uri;

        type Captured = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;
        let captured: Captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        async fn record(State(captured): State<Captured>, uri: Uri, body: Bytes) -> &'static str {
            captured.lock().expect("capture lock").push((
                uri.path().to_string(),
                String::from_utf8_lossy(&body).into_owned(),
            ));
            r#"{"ok":true,"result":{}}"#
        }

        let app = axum::Router::new()
            .fallback(axum::routing::post(record))
            .with_state(std::sync::Arc::clone(&captured));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{addr}"), captured)
    }

    fn only_request(captured: &std::sync::Mutex<Vec<(String, String)>>) -> (String, String) {
        let requests = captured.lock().expect("capture lock");
        assert_eq!(requests.len(), 1, "expected exactly one request");
        requests[0].clone()
    }

    /// A forum topic is a DESTINATION and stays in `reply_target`; the reply
    /// anchor is the prompting message and lives in `thread_ts`. Carrying the
    /// topic in both is the failure this asserts against — they could disagree.
    #[test]
    fn telegram_forum_topic_and_reply_anchor_are_different_fields() {
        let ch = TelegramChannel::new("token".into(), vec!["*".into()], false);
        let update = serde_json::json!({
            "update_id": 1,
            "message": {
                "message_id": 4242,
                "message_thread_id": 789,
                "text": "in a topic",
                "from": { "id": 555 },
                "chat": { "id": -100_200_300 }
            }
        });

        let (msg, _) = ch
            .parse_update_message(&update)
            .expect("the message parses");
        assert_eq!(msg.reply_target, "-100200300:789", "topic = destination");
        assert_eq!(
            msg.thread_ts.as_deref(),
            Some("4242"),
            "anchor = the message"
        );
    }

    #[tokio::test]
    async fn telegram_reply_anchors_on_the_prompting_message() {
        let (base, captured) = spawn_bot_api().await;
        let ch =
            TelegramChannel::new("123:ABC".into(), vec!["*".into()], false).with_api_base(base);

        ch.send(
            &SendMessage::new("threaded reply", "-100200300:789")
                .in_thread(Some("4242".to_string())),
        )
        .await
        .expect("the local Bot API accepts it");

        let (path, body) = only_request(&captured);
        assert_eq!(path, "/bot123:ABC/sendMessage");
        let json: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(json["message_thread_id"], "789", "topic still routes");
        assert_eq!(json["reply_parameters"]["message_id"], 4242);
        // A deleted anchor must not fail the send.
        assert_eq!(
            json["reply_parameters"]["allow_sending_without_reply"],
            true
        );
    }

    #[tokio::test]
    async fn telegram_unanchored_reply_carries_no_reply_parameters() {
        let (base, captured) = spawn_bot_api().await;
        let ch =
            TelegramChannel::new("123:ABC".into(), vec!["*".into()], false).with_api_base(base);

        ch.send(&SendMessage::new("flat reply", "-100200300"))
            .await
            .expect("the local Bot API accepts it");

        let (_, body) = only_request(&captured);
        let json: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert!(
            json.get("reply_parameters").is_none(),
            "a non-threaded inbound must not produce a threaded reply"
        );
    }

    /// A photo that cannot be resolved used to vanish: the caller dropped every
    /// error with `if let Ok(..)`, so the user got no answer and no reason.
    #[tokio::test]
    async fn telegram_photo_failure_becomes_a_visible_note() {
        let (base, _captured) = spawn_bot_api().await;
        let ch =
            TelegramChannel::new("123:ABC".into(), vec!["*".into()], false).with_api_base(base);

        // The stub answers `getFile` with `{"ok":true,"result":{}}` — no
        // `file_path` — which is the shape a revoked/expired file id produces.
        let marker = ch
            .resolve_photo_marker("file-1", "tg_user_a")
            .await
            .to_marker();
        assert!(marker.contains("Attachment unavailable"), "got: {marker}");
        assert!(!marker.starts_with("[IMAGE:"));
    }

    /// The budget key is channel-qualified, so a Telegram id cannot spend a
    /// Discord id's allowance. Dropping the `telegram:` prefix fails this.
    #[tokio::test]
    async fn telegram_charges_the_media_budget_under_a_channel_qualified_key() {
        use crate::channels::media;
        use axum::response::IntoResponse;

        use std::sync::atomic::{AtomicUsize, Ordering};
        static GET_FILE_HITS: AtomicUsize = AtomicUsize::new(0);

        async fn get_file() -> impl IntoResponse {
            GET_FILE_HITS.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({"ok": true, "result": {"file_path": "photos/x.jpg"}}))
        }

        let sender = "telegram_budget_user";
        for _ in 0..media::BUDGET_IMAGES {
            assert!(media::charge(&format!("telegram:{sender}")).is_ok());
        }

        // Only `getFile` is mounted: the download route is deliberately absent,
        // so reaching it at all would be a 404 rather than the budget note.
        let app = axum::Router::new().route("/bot123:ABC/getFile", axum::routing::get(get_file));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let ch = TelegramChannel::new("123:ABC".into(), vec!["*".into()], false)
            .with_api_base(format!("http://{addr}"));
        // Control first: an unrelated sender with budget left DOES reach getFile,
        // so the zero below cannot come from an unreachable server.
        let fresh = ch
            .resolve_photo_marker("file-1", "telegram_budget_control")
            .await
            .to_marker();
        assert!(!fresh.contains("media budget spent"), "got: {fresh}");
        assert_eq!(
            GET_FILE_HITS.load(Ordering::SeqCst),
            1,
            "the control must reach getFile"
        );

        let marker = ch.resolve_photo_marker("file-1", sender).await.to_marker();
        assert!(marker.contains("media budget spent"), "got: {marker}");
        // The point of this change: `getFile` is an authenticated round trip,
        // and an exhausted sender must not be able to make it either.
        assert_eq!(
            GET_FILE_HITS.load(Ordering::SeqCst),
            1,
            "the refused attachment still called getFile — the budget is being \
             checked after the lookup instead of before it"
        );
    }

    /// A photo whose bytes are not an image must reach the user as a note, not
    /// as silence and not as a broken `[IMAGE:]` marker.
    #[tokio::test]
    async fn telegram_photo_rejected_by_the_policy_becomes_a_note() {
        use axum::response::IntoResponse;

        async fn get_file() -> impl IntoResponse {
            axum::Json(serde_json::json!({"ok": true, "result": {"file_path": "photos/x.jpg"}}))
        }
        async fn download() -> impl IntoResponse {
            // Claims nothing; the bytes are not an image.
            axum::body::Bytes::from_static(b"%PDF-1.7 not an image")
        }

        let app = axum::Router::new()
            .route("/bot123:ABC/getFile", axum::routing::get(get_file))
            .route(
                "/file/bot123:ABC/photos/x.jpg",
                axum::routing::get(download),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let ch = TelegramChannel::new("123:ABC".into(), vec!["*".into()], false)
            .with_api_base(format!("http://{addr}"));
        let marker = ch
            .resolve_photo_marker("file-1", "tg_user_b")
            .await
            .to_marker();
        assert!(marker.contains("unsupported type"), "got: {marker}");
        assert!(!marker.starts_with("[IMAGE:"));
    }

    /// The 25 MiB constant this path used to carry is gone: the cap is the
    /// operator's `[multimodal].max_image_size_mb`, applied by `media::`.
    #[test]
    fn telegram_photo_cap_comes_from_multimodal_config() {
        let src = include_str!("telegram.rs");
        let production = src
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("source");
        assert!(
            !production.contains("MAX_PHOTO_BYTES"),
            "the channel-local photo cap must be gone"
        );
        let resolver = production
            .split("async fn resolve_photo_marker(")
            .nth(1)
            .expect("resolve_photo_marker exists");
        assert!(
            resolver.contains("media::fetch_image_bytes")
                && resolver.contains("media::max_bytes(&self.multimodal)"),
            "the fetch must go through the shared policy with the operator's cap"
        );
    }

    #[tokio::test]
    async fn telegram_send_document_bytes_posts_the_multipart_form() {
        let (base, captured) = spawn_bot_api().await;
        let ch =
            TelegramChannel::new("123:ABC".into(), vec!["*".into()], false).with_api_base(base);

        ch.send_document_bytes(
            "123456",
            Some("77"),
            b"file content".to_vec(),
            "report.txt",
            Some("Test caption"),
        )
        .await
        .expect("the local Bot API accepts it");

        let (path, body) = only_request(&captured);
        assert_eq!(path, "/bot123:ABC/sendDocument");
        for expected in [
            "name=\"chat_id\"",
            "123456",
            "name=\"document\"",
            "filename=\"report.txt\"",
            "file content",
            "name=\"caption\"",
            "Test caption",
            "name=\"message_thread_id\"",
        ] {
            assert!(body.contains(expected), "multipart body missing {expected}");
        }
    }

    #[tokio::test]
    async fn telegram_send_document_bytes_omits_an_absent_caption() {
        let (base, captured) = spawn_bot_api().await;
        let ch =
            TelegramChannel::new("123:ABC".into(), vec!["*".into()], false).with_api_base(base);

        ch.send_document_bytes("123456", None, b"x".to_vec(), "a.txt", None)
            .await
            .expect("the local Bot API accepts it");

        let (_, body) = only_request(&captured);
        assert!(
            !body.contains("name=\"caption\""),
            "no caption was supplied, so none may be sent"
        );
        assert!(!body.contains("name=\"message_thread_id\""));
    }

    #[tokio::test]
    async fn telegram_send_photo_bytes_posts_to_send_photo() {
        let (base, captured) = spawn_bot_api().await;
        let ch =
            TelegramChannel::new("123:ABC".into(), vec!["*".into()], false).with_api_base(base);

        ch.send_photo_bytes(
            "123456",
            None,
            vec![0x89, 0x50, 0x4E, 0x47],
            "shot.png",
            Some("Photo caption"),
        )
        .await
        .expect("the local Bot API accepts it");

        let (path, body) = only_request(&captured);
        assert_eq!(path, "/bot123:ABC/sendPhoto");
        assert!(body.contains("name=\"photo\""), "the field is `photo`");
        assert!(body.contains("filename=\"shot.png\""));
        assert!(body.contains("Photo caption"));
    }

    #[tokio::test]
    async fn telegram_send_document_by_url_posts_json_not_multipart() {
        let (base, captured) = spawn_bot_api().await;
        let ch =
            TelegramChannel::new("123:ABC".into(), vec!["*".into()], false).with_api_base(base);

        ch.send_document_by_url(
            "123456",
            None,
            "https://example.com/file.pdf",
            Some("PDF doc"),
        )
        .await
        .expect("the local Bot API accepts it");

        let (path, body) = only_request(&captured);
        assert_eq!(path, "/bot123:ABC/sendDocument");
        let json: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(json["chat_id"], "123456");
        assert_eq!(json["document"], "https://example.com/file.pdf");
        assert_eq!(json["caption"], "PDF doc");
        assert!(json.get("message_thread_id").is_none());
    }

    #[tokio::test]
    async fn telegram_send_photo_by_url_posts_json_to_send_photo() {
        let (base, captured) = spawn_bot_api().await;
        let ch =
            TelegramChannel::new("123:ABC".into(), vec!["*".into()], false).with_api_base(base);

        ch.send_photo_by_url("123456", Some("9"), "https://example.com/image.jpg", None)
            .await
            .expect("the local Bot API accepts it");

        let (path, body) = only_request(&captured);
        assert_eq!(path, "/bot123:ABC/sendPhoto");
        let json: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(json["photo"], "https://example.com/image.jpg");
        assert_eq!(json["message_thread_id"], "9");
        assert!(json.get("caption").is_none());
    }

    /// Telegram rejects an empty `chat_id`; the channel must surface that as an
    /// error rather than logging a success. The old test asserted `is_err()`
    /// against an unreachable host, which proved nothing about this path.
    #[tokio::test]
    async fn telegram_send_document_bytes_reports_an_api_rejection() {
        async fn reject() -> (axum::http::StatusCode, &'static str) {
            (
                axum::http::StatusCode::BAD_REQUEST,
                r#"{"ok":false,"description":"Bad Request: chat_id is empty"}"#,
            )
        }
        let app = axum::Router::new().fallback(axum::routing::post(reject));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let ch = TelegramChannel::new("123:ABC".into(), vec!["*".into()], false)
            .with_api_base(format!("http://{addr}"));
        let err = ch
            .send_document_bytes("", None, b"content".to_vec(), "test.txt", None)
            .await
            .expect_err("an HTTP 400 from the Bot API is an error");
        assert!(
            err.to_string().contains("chat_id is empty"),
            "the API's reason must survive: {err}"
        );
    }

    // ── File path handling tests ────────────────────────────────────

    #[tokio::test]
    async fn telegram_send_document_nonexistent_file() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);
        let path = Path::new("/nonexistent/path/to/file.txt");

        let result = ch.send_document("123456", None, path, None).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // Should fail with file not found error
        assert!(
            err.contains("No such file") || err.contains("not found") || err.contains("os error"),
            "Expected file not found error, got: {err}"
        );
    }

    #[tokio::test]
    async fn telegram_send_photo_nonexistent_file() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);
        let path = Path::new("/nonexistent/path/to/photo.jpg");

        let result = ch.send_photo("123456", None, path, None).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn telegram_send_video_nonexistent_file() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);
        let path = Path::new("/nonexistent/path/to/video.mp4");

        let result = ch.send_video("123456", None, path, None).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn telegram_send_audio_nonexistent_file() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);
        let path = Path::new("/nonexistent/path/to/audio.mp3");

        let result = ch.send_audio("123456", None, path, None).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn telegram_send_voice_nonexistent_file() {
        let ch = TelegramChannel::new("fake-token".into(), vec!["*".into()], false);
        let path = Path::new("/nonexistent/path/to/voice.ogg");

        let result = ch.send_voice("123456", None, path, None).await;

        assert!(result.is_err());
    }

    // ── Message splitting tests ─────────────────────────────────────

    // ── Caption handling tests ──────────────────────────────────────

    // ── Empty/edge case tests ───────────────────────────────────────

    // ── Message ID edge cases ─────────────────────────────────────

    #[test]
    fn telegram_message_id_format_includes_chat_and_message_id() {
        // Verify that message IDs follow the format: telegram_{chat_id}_{message_id}
        let chat_id = "123456";
        let message_id = 789;
        let expected_id = format!("telegram_{chat_id}_{message_id}");
        assert_eq!(expected_id, "telegram_123456_789");
    }

    #[test]
    fn telegram_message_id_is_deterministic() {
        // Same chat_id + same message_id = same ID (prevents duplicates after restart)
        let chat_id = "123456";
        let message_id = 789;
        let id1 = format!("telegram_{chat_id}_{message_id}");
        let id2 = format!("telegram_{chat_id}_{message_id}");
        assert_eq!(id1, id2);
    }

    #[test]
    fn telegram_message_id_different_message_different_id() {
        // Different message IDs produce different IDs
        let chat_id = "123456";
        let id1 = format!("telegram_{chat_id}_789");
        let id2 = format!("telegram_{chat_id}_790");
        assert_ne!(id1, id2);
    }

    #[test]
    fn telegram_message_id_different_chat_different_id() {
        // Different chats produce different IDs even with same message_id
        let message_id = 789;
        let id1 = format!("telegram_123456_{message_id}");
        let id2 = format!("telegram_789012_{message_id}");
        assert_ne!(id1, id2);
    }

    #[test]
    fn telegram_message_id_no_uuid_randomness() {
        // Verify format doesn't contain random UUID components
        let chat_id = "123456";
        let message_id = 789;
        let id = format!("telegram_{chat_id}_{message_id}");
        assert!(!id.contains('-')); // No UUID dashes
        assert!(id.starts_with("telegram_"));
    }

    #[test]
    fn telegram_message_id_handles_zero_message_id() {
        // Edge case: message_id can be 0 (fallback/missing case)
        let chat_id = "123456";
        let message_id = 0;
        let id = format!("telegram_{chat_id}_{message_id}");
        assert_eq!(id, "telegram_123456_0");
    }

    // ── Tool call tag stripping tests ───────────────────────────────────

    #[test]
    fn strip_tool_call_tags_removes_standard_tags() {
        let input =
            "Hello <tool>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool> world";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "Hello  world");
    }

    #[test]
    fn strip_tool_call_tags_removes_alias_tags() {
        let input = "Hello <toolcall>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</toolcall> world";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "Hello  world");
    }

    #[test]
    fn strip_tool_call_tags_removes_dash_tags() {
        let input = "Hello <tool-call>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool-call> world";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "Hello  world");
    }

    #[test]
    fn strip_tool_call_tags_removes_tool_call_tags() {
        let input = "Hello <tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool_call> world";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "Hello  world");
    }

    #[test]
    fn strip_tool_call_tags_removes_invoke_tags() {
        let input = "Hello <invoke>{\"name\":\"shell\",\"arguments\":{\"command\":\"date\"}}</invoke> world";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "Hello  world");
    }

    #[test]
    fn strip_tool_call_tags_handles_multiple_tags() {
        let input = "Start <tool>a</tool> middle <tool>b</tool> end";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "Start  middle  end");
    }

    #[test]
    fn strip_tool_call_tags_handles_mixed_tags() {
        let input = "A <tool>a</tool> B <toolcall>b</toolcall> C <tool-call>c</tool-call> D";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "A  B  C  D");
    }

    #[test]
    fn strip_tool_call_tags_preserves_normal_text() {
        let input = "Hello world! This is a test.";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "Hello world! This is a test.");
    }

    #[test]
    /// Was an assertion that the raw tags come back out — the function
    /// re-emitted exactly what it exists to remove.
    fn unterminated_tool_tag_is_dropped_not_reemitted() {
        assert_eq!(strip_tool_call_tags("Hello <tool>world"), "Hello");
        assert_eq!(
            strip_tool_call_tags("before <function_calls> leftover"),
            "before"
        );
    }

    #[test]
    fn strip_tool_call_tags_handles_unclosed_tool_call_with_json() {
        let input =
            "Status:\n<tool_call>\n{\"name\":\"shell\",\"arguments\":{\"command\":\"uptime\"}}";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "Status:");
    }

    #[test]
    fn strip_tool_call_tags_handles_mismatched_close_tag() {
        let input =
            "<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"uptime\"}}</arg_value>";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "");
    }

    #[test]
    fn strip_tool_call_tags_cleans_extra_newlines() {
        let input = "Hello\n\n<tool>\ntest\n</tool>\n\n\nworld";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "Hello\n\nworld");
    }

    #[test]
    fn strip_tool_call_tags_handles_empty_input() {
        let input = "";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "");
    }

    #[test]
    fn strip_tool_call_tags_handles_only_tags() {
        let input = "<tool>{\"name\":\"test\"}</tool>";
        let result = strip_tool_call_tags(input);
        assert_eq!(result, "");
    }

    #[test]
    fn telegram_contains_bot_mention_finds_mention() {
        assert!(TelegramChannel::contains_bot_mention(
            "Hello @mybot",
            "mybot"
        ));
        assert!(TelegramChannel::contains_bot_mention(
            "@mybot help",
            "mybot"
        ));
        assert!(TelegramChannel::contains_bot_mention(
            "Hey @mybot how are you?",
            "mybot"
        ));
        assert!(TelegramChannel::contains_bot_mention(
            "Hello @MyBot, can you help?",
            "mybot"
        ));
    }

    #[test]
    fn telegram_contains_bot_mention_no_false_positives() {
        assert!(!TelegramChannel::contains_bot_mention(
            "Hello @otherbot",
            "mybot"
        ));
        assert!(!TelegramChannel::contains_bot_mention(
            "Hello mybot",
            "mybot"
        ));
        assert!(!TelegramChannel::contains_bot_mention(
            "Hello @mybot2",
            "mybot"
        ));
        assert!(!TelegramChannel::contains_bot_mention("", "mybot"));
    }

    #[test]
    fn telegram_normalize_incoming_content_strips_mention() {
        let result = TelegramChannel::normalize_incoming_content("@mybot hello", "mybot");
        assert_eq!(result, Some("hello".to_string()));
    }

    #[test]
    fn telegram_normalize_incoming_content_handles_multiple_mentions() {
        let result = TelegramChannel::normalize_incoming_content("@mybot @mybot test", "mybot");
        assert_eq!(result, Some("test".to_string()));
    }

    #[test]
    fn telegram_normalize_incoming_content_returns_none_for_empty() {
        let result = TelegramChannel::normalize_incoming_content("@mybot", "mybot");
        assert_eq!(result, None);
    }

    #[test]
    fn parse_update_message_mention_only_group_requires_exact_mention() {
        let ch = TelegramChannel::new("token".into(), vec!["*".into()], true);
        {
            let mut cache = ch.bot_username.lock();
            *cache = Some("mybot".to_string());
        }

        let update = serde_json::json!({
            "update_id": 10,
            "message": {
                "message_id": 44,
                "text": "hello @mybot2",
                "from": {
                    "id": 555,
                    "username": "alice"
                },
                "chat": {
                    "id": -100_200_300,
                    "type": "group"
                }
            }
        });

        assert!(ch.parse_update_message(&update).is_none());
    }

    #[test]
    fn telegram_render_target_is_html() {
        // Assert on the CHANNEL, not just `format::*`: a test that only called
        // the renderer would pass before this wiring existed and prove nothing.
        let ch = TelegramChannel::new("token".into(), vec!["*".into()], true);
        assert_eq!(
            ch.render_target(),
            crate::channels::format::RenderTarget::TelegramHtml
        );
    }

    #[test]
    fn telegram_paired_chunks_have_plain_twins() {
        use crate::channels::format::{render_pair, split_paired, RenderTarget};
        let (html, plain) = render_pair(
            "## Hi\n\n**bold**",
            &RenderTarget::TelegramHtml,
            &RenderTarget::Plain,
        );
        let pairs = split_paired(&html, &plain, 4096);
        assert_eq!(pairs.len(), 1);
        // Headings and bold render to <b>, not leak as ## / **.
        assert!(pairs[0].0.contains("<b>Hi</b>"));
        assert!(pairs[0].0.contains("<b>bold</b>"));
        assert!(!pairs[0].0.contains("##"));
        // The Plain twin covers the same blocks, ready as the 400 fallback.
        assert_eq!(pairs[0].1, "HI\n\nbold");
    }

    #[test]
    fn parse_update_message_mention_only_group_strips_mention_and_drops_empty() {
        let ch = TelegramChannel::new("token".into(), vec!["*".into()], true);
        {
            let mut cache = ch.bot_username.lock();
            *cache = Some("mybot".to_string());
        }

        let update = serde_json::json!({
            "update_id": 11,
            "message": {
                "message_id": 45,
                "text": "Hi @MyBot status please",
                "from": {
                    "id": 555,
                    "username": "alice"
                },
                "chat": {
                    "id": -100_200_300,
                    "type": "group"
                }
            }
        });

        let parsed = ch
            .parse_update_message(&update)
            .map(|(m, _)| m)
            .expect("mention should parse");
        assert_eq!(parsed.content, "Hi status please");

        let empty_update = serde_json::json!({
            "update_id": 12,
            "message": {
                "message_id": 46,
                "text": "@mybot",
                "from": {
                    "id": 555,
                    "username": "alice"
                },
                "chat": {
                    "id": -100_200_300,
                    "type": "group"
                }
            }
        });

        assert!(ch.parse_update_message(&empty_update).is_none());
    }

    #[test]
    fn telegram_is_group_message_detects_groups() {
        let group_msg = serde_json::json!({
            "chat": { "type": "group" }
        });
        assert!(TelegramChannel::is_group_message(&group_msg));

        let supergroup_msg = serde_json::json!({
            "chat": { "type": "supergroup" }
        });
        assert!(TelegramChannel::is_group_message(&supergroup_msg));

        let private_msg = serde_json::json!({
            "chat": { "type": "private" }
        });
        assert!(!TelegramChannel::is_group_message(&private_msg));
    }

    #[test]
    fn telegram_mention_only_enabled_by_config() {
        let ch = TelegramChannel::new("token".into(), vec!["*".into()], true);
        assert!(ch.mention_only);

        let ch_disabled = TelegramChannel::new("token".into(), vec!["*".into()], false);
        assert!(!ch_disabled.mention_only);
    }
}
