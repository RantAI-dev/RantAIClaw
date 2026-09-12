use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::sync::{Arc, RwLock};

/// Slack channel — polls conversations.history via Web API
pub struct SlackChannel {
    bot_token: String,
    /// `[multimodal]` defaults; the factory overrides it with the operator's.
    multimodal: crate::config::MultimodalConfig,
    channel_id: Option<String>,
    /// App-level token (`xapp-`). Present means Socket Mode; absent means the
    /// polling transport. The schema has documented this key as Socket Mode
    /// since before anything read it.
    app_token: Option<String>,
    /// `Arc<RwLock<..>>` so a successful `/bind`/`/claim` can append the sender
    /// at runtime (immediate access without a channel restart).
    allowed_users: Arc<RwLock<Vec<String>>>,
    /// The "working…" placeholder currently posted for each conversation,
    /// keyed by recipient so two concurrent chats cannot delete each other's.
    /// Value is the Slack `ts` needed to remove it.
    working_notices: Arc<Mutex<std::collections::HashMap<String, String>>>,
}

/// Slack's own guidance for `chat.postMessage`: "For best results, limit the
/// number of characters in the text field to 4,000 characters."
/// <https://docs.slack.dev/changelog/2018-truncating-really-long-messages/>
const SLACK_MAX_MESSAGE_LENGTH: usize = 4000;

/// What an operator loses by leaving `app_token` unset, in one sentence.
///
/// Three surfaces have to say this and must not drift apart: the runtime
/// warning in `channels::factory`, the TUI provisioner prompt, and the
/// first-run wizard prompt. Two of the three used to say nothing at all, and
/// `doctor` sent operators to a config key no setup path could write, so the
/// wording is shared rather than copied.
pub const NO_APP_TOKEN_CONSEQUENCE: &str =
    "Without it the bot polls one conversation: it will not see direct messages, and it will \
     not see replies inside a thread — including replies to the approval prompt it posts there.";

/// The shape of a Slack app-level token. Checked for a warning only; `doctor`
/// decides whether a token actually works.
pub const APP_TOKEN_PREFIX: &str = "xapp-";

/// What the poll loop should do with one inbound Slack message.
///
/// Extracted so the allowlist gate is reachable from a test. It used to sit
/// inside `listen()`'s socket loop, which no test enters — deleting the gate
/// line left every test in this file passing. This is a **move**: the ordering
/// of the checks and their conditions are unchanged.
#[derive(Debug)]
pub(crate) enum SlackInbound {
    /// The bot's own message.
    Own,
    /// Not in the allowlist. The caller runs the pairing path, which needs the
    /// network, then drops the message.
    Unauthorized {
        user: String,
        text: String,
        ts: String,
    },
    /// Empty text, or older than the cursor.
    EmptyOrSeen,
    Deliver(ChannelMessage),
}

/// What the listener should do after one inbound message.
enum InboundOutcome {
    /// Keep listening.
    Continue,
    /// The agent-side receiver is gone; stop cleanly.
    ReceiverGone,
}

impl SlackChannel {
    /// Classify one message from a `conversations.history` page.
    pub(crate) fn classify_inbound(
        &self,
        msg: &serde_json::Value,
        bot_user_id: &str,
        last_ts: &str,
        channel_id: &str,
    ) -> SlackInbound {
        let ts = msg.get("ts").and_then(|t| t.as_str()).unwrap_or("");
        let user = msg
            .get("user")
            .and_then(|u| u.as_str())
            .unwrap_or("unknown");
        let text = msg.get("text").and_then(|t| t.as_str()).unwrap_or("");

        if user == bot_user_id {
            return SlackInbound::Own;
        }

        if !self.is_user_allowed(user) {
            return SlackInbound::Unauthorized {
                user: user.to_string(),
                text: text.to_string(),
                ts: ts.to_string(),
            };
        }

        // A photo posted with no caption has empty `text` and is still a
        // message. Only treat it as empty when it carries no files either.
        let has_files = msg
            .get("files")
            .and_then(|f| f.as_array())
            .is_some_and(|f| !f.is_empty());
        if (text.is_empty() && !has_files) || ts <= last_ts {
            return SlackInbound::EmptyOrSeen;
        }

        SlackInbound::Deliver(ChannelMessage {
            sender_aliases: Vec::new(),
            id: format!("slack_{channel_id}_{ts}"),
            sender: user.to_string(),
            reply_target: channel_id.to_string(),
            content: text.to_string(),
            channel: "slack".to_string(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            thread_ts: Self::inbound_thread_ts(msg, ts),
            reply_anchor: None,
        })
    }

    /// Handle one inbound message: classify it, deliver it, or let an
    /// unauthorized sender self-onboard with a pairing code.
    ///
    /// Extracted from the polling loop so a second transport can reuse it
    /// verbatim rather than growing a second copy of the allowlist and pairing
    /// rules — `factory.rs` already records what happens when two paths for one
    /// channel drift.
    async fn handle_inbound(
        &self,
        msg: &serde_json::Value,
        bot_user_id: &str,
        last_ts: &mut String,
        channel_id: &str,
        tx: &tokio::sync::mpsc::Sender<ChannelMessage>,
    ) -> InboundOutcome {
        // The decision — including the allowlist gate — lives in
        // `classify_inbound` so a test can reach it. Everything below needs the
        // network, which is why it stays here.
        let (user, text, ts) = match self.classify_inbound(msg, bot_user_id, last_ts, channel_id) {
            SlackInbound::Own | SlackInbound::EmptyOrSeen => return InboundOutcome::Continue,
            SlackInbound::Deliver(mut channel_msg) => {
                *last_ts = channel_msg
                    .id
                    .rsplit('_')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                // Downloads need the network, which is why they happen here and
                // not in `classify_inbound`.
                let markers = self.file_markers(msg, &channel_msg.sender).await;
                if !markers.is_empty() {
                    if !channel_msg.content.is_empty() {
                        channel_msg.content.push('\n');
                    }
                    channel_msg.content.push_str(&markers.join("\n"));
                }
                if tx.send(channel_msg).await.is_err() {
                    return InboundOutcome::ReceiverGone;
                }
                return InboundOutcome::Continue;
            }
            SlackInbound::Unauthorized { user, text, ts } => (user, text, ts),
        };
        let (user, text, ts) = (user.as_str(), text.as_str(), ts.as_str());

        // Before rejecting, let a not-yet-allowed user self-onboard with a
        // `/bind`/`/claim <code>` minted via `rantaiclaw channels pair`. On
        // success the sender lands in `allowed_users` (and, for an owner
        // `/claim`, `approval_owners`).
        if !text.is_empty() && ts > last_ts.as_str() {
            if let Some(root) = crate::channels::pairing::profile_root("slack") {
                let identities = vec![user.to_string()];
                if let Some(reply) = crate::channels::pairing::try_handle_pairing(
                    text,
                    "slack",
                    crate::channels::pairing::AllowlistField::AllowedUsers,
                    &identities,
                    &root,
                )
                .await
                {
                    // Advance the cursor so this command isn't re-processed,
                    // mirror into the runtime allowlist, and reply in-channel.
                    *last_ts = ts.to_string();
                    self.add_allowed_identity_runtime(user);
                    let reply_msg = SendMessage::new(reply, channel_id.to_string())
                        .in_thread(Self::inbound_thread_ts(msg, ts));
                    let _ = self.send(&reply_msg).await;
                    return InboundOutcome::Continue;
                }
            }
        }
        tracing::warn!("Slack: ignoring message from unauthorized user: {user}");
        InboundOutcome::Continue
    }

    /// POST one already-split chunk.
    /// Upload one attachment and share it into the conversation.
    ///
    /// Three calls, because `files.upload` is retired: reserve an upload URL,
    /// `POST` the bytes to it, then complete the upload naming the channel. All
    /// of it is covered by `files:write`; nothing here needs a scope beyond the
    /// one the operator granted.
    ///
    /// A remote URL is posted as text instead. Slack unfurls it, the file is
    /// already public, and re-uploading it would spend bandwidth to gain
    /// nothing — the same choice Discord and WhatsApp Web make.
    async fn send_attachment(
        &self,
        message: &SendMessage,
        attachment: &crate::channels::media::OutboundAttachment,
    ) -> anyhow::Result<()> {
        let target = attachment.target.trim();

        if crate::channels::media::is_http_url(target) {
            return self.post_chunk(message, target).await;
        }

        // One resolver for every channel: it expands `~`, resolves a relative
        // path against the workspace rather than the daemon's working directory,
        // checks the file exists, and fails closed outside the workspace — a
        // reply a guest can influence must not post the config into the chat.
        let path =
            crate::channels::media::resolve_attachment_path_in_workspace("Slack", target).await?;

        let bytes = {
            use anyhow::Context as _;
            tokio::fs::read(&path)
                .await
                .with_context(|| format!("cannot read attachment: {target}"))?
        };
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();

        // 1. Reserve an upload URL. `length` is required and must match what we
        //    actually send, so it comes from the bytes rather than metadata.
        let reserve = self
            .http_client()
            .get("https://slack.com/api/files.getUploadURLExternal")
            .bearer_auth(&self.bot_token)
            .query(&[
                ("filename", file_name.as_str()),
                ("length", &bytes.len().to_string()),
            ])
            .send()
            .await?;
        let reserve_body = reserve.text().await.unwrap_or_default();
        let reserve_json: serde_json::Value =
            serde_json::from_str(&reserve_body).unwrap_or_default();
        if !Self::api_response_is_ok(&reserve_json) {
            let err = reserve_json
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown");
            anyhow::bail!("Slack files.getUploadURLExternal failed: {err}");
        }
        let (upload_url, file_id) = Self::upload_ticket(&reserve_json)
            .ok_or_else(|| anyhow::anyhow!("Slack returned no upload URL or file id"))?;

        // 2. `POST` the bytes. **No credential is attached here**, which is the
        //    whole reason this needs no host pin: the URL came back from an
        //    authenticated call to `slack.com`, and the worst case is sending
        //    the file to Slack's own storage. That is the opposite of the
        //    inbound path, where `url_private` arrives in an event payload and
        //    *does* carry the bot token, so `is_slack_file_host` gates it.
        //    Pinning a host here would instead break uploads the day Slack
        //    moves external storage off `files.slack.com`.
        let put = self
            .http_client()
            .post(&upload_url)
            .body(bytes)
            .send()
            .await?;
        if !put.status().is_success() {
            let status = put.status();
            anyhow::bail!("Slack file upload failed ({status})");
        }

        // 3. Complete, which is what actually posts it into the conversation.
        let mut complete = serde_json::json!({
            "files": [{ "id": file_id, "title": file_name }],
            "channel_id": message.recipient,
        });
        if let Some(ref ts) = message.thread_ts {
            complete["thread_ts"] = serde_json::json!(ts);
        }
        let done = self
            .http_client()
            .post("https://slack.com/api/files.completeUploadExternal")
            .bearer_auth(&self.bot_token)
            .json(&complete)
            .send()
            .await?;
        let done_body = done.text().await.unwrap_or_default();
        let done_json: serde_json::Value = serde_json::from_str(&done_body).unwrap_or_default();
        if !Self::api_response_is_ok(&done_json) {
            let err = done_json
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown");
            anyhow::bail!("Slack files.completeUploadExternal failed: {err}");
        }
        Ok(())
    }

    /// Pull the upload URL and file id out of a `getUploadURLExternal` reply.
    ///
    /// Separated so the parsing is reachable from a test: the call around it
    /// needs a workspace and a live token, and a missing field here is the
    /// difference between an upload and a confusing failure two calls later.
    pub(crate) fn upload_ticket(reply: &serde_json::Value) -> Option<(String, String)> {
        let url = reply.get("upload_url").and_then(|u| u.as_str())?;
        let id = reply.get("file_id").and_then(|i| i.as_str())?;
        if url.trim().is_empty() || id.trim().is_empty() {
            return None;
        }
        Some((url.to_string(), id.to_string()))
    }

    /// Delete one of the bot's own messages. Covered by `chat:write`; no new
    /// scope and no app reinstall.
    async fn delete_message(&self, channel: &str, ts: &str) -> anyhow::Result<()> {
        let resp = self
            .http_client()
            .post("https://slack.com/api/chat.delete")
            .bearer_auth(&self.bot_token)
            .json(&serde_json::json!({ "channel": channel, "ts": ts }))
            .send()
            .await?;
        let text = resp.text().await.unwrap_or_default();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if !Self::api_response_is_ok(&parsed) {
            // A placeholder that outlives its turn is the failure this plan is
            // about, so say so rather than swallowing it.
            tracing::warn!("Slack could not remove its working notice: {text}");
        }
        Ok(())
    }

    async fn post_chunk(&self, message: &SendMessage, chunk: &str) -> anyhow::Result<()> {
        let mut body = serde_json::json!({
            "channel": message.recipient,
            "text": chunk
        });

        if let Some(ref ts) = message.thread_ts {
            body["thread_ts"] = serde_json::json!(ts);
        }

        let resp = self
            .http_client()
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("<failed to read response body: {e}>"));

        if !status.is_success() {
            anyhow::bail!("Slack chat.postMessage failed ({status}): {body}");
        }

        // Slack returns 200 for most app-level errors; check JSON "ok" field
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        if !Self::api_response_is_ok(&parsed) {
            let err = parsed
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown");
            anyhow::bail!("Slack chat.postMessage failed: {err}");
        }

        Ok(())
    }

    /// Whether a Slack Web API response reports success.
    ///
    /// Slack signals application-level failure in the body with HTTP 200, so
    /// the status alone says nothing.
    fn api_response_is_ok(body: &serde_json::Value) -> bool {
        body.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
    }

    pub fn new(bot_token: String, channel_id: Option<String>, allowed_users: Vec<String>) -> Self {
        Self {
            bot_token,
            multimodal: crate::config::MultimodalConfig::default(),
            channel_id,
            app_token: None,
            allowed_users: Arc::new(RwLock::new(allowed_users)),
            working_notices: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// What the placeholder says. Short and unmistakably transient.
    pub(crate) const WORKING_NOTICE: &'static str = "_working on it…_";

    /// The `chat.postMessage` body for the placeholder.
    ///
    /// Extracted so the threading is reachable from a test: posting needs a
    /// live workspace, and a placeholder that lands in the main channel while
    /// the conversation is in a thread is noise for everyone else in it.
    pub(crate) fn working_notice_body(
        recipient: &str,
        thread_ts: Option<&str>,
    ) -> serde_json::Value {
        let mut body = serde_json::json!({
            "channel": recipient,
            "text": Self::WORKING_NOTICE,
        });
        if let Some(ts) = thread_ts {
            body["thread_ts"] = serde_json::json!(ts);
        }
        body
    }

    /// Has a placeholder already been posted for this conversation?
    ///
    /// `start_typing` is driven on a 4-second refresh
    /// (`supervisor::spawn_scoped_typing_task`), which is right for a platform
    /// typing indicator that expires and wrong for a message. Posting one per
    /// tick would fill the chat, so the second and later calls must be no-ops.
    pub(crate) fn has_working_notice(&self, recipient: &str) -> bool {
        self.working_notices.lock().contains_key(recipient)
    }

    /// Record the placeholder posted for `recipient`, returning any previous
    /// one so it can be cleaned up rather than orphaned.
    pub(crate) fn remember_working_notice(&self, recipient: &str, ts: String) -> Option<String> {
        self.working_notices
            .lock()
            .insert(recipient.to_string(), ts)
    }

    /// Take this conversation's placeholder, leaving every other one alone.
    pub(crate) fn take_working_notice(&self, recipient: &str) -> Option<String> {
        self.working_notices.lock().remove(recipient)
    }

    /// Apply the operator's `[multimodal]` limits to inbound images.
    #[must_use]
    pub fn with_multimodal(mut self, multimodal: crate::config::MultimodalConfig) -> Self {
        self.multimodal = multimodal;
        self
    }

    /// Slack hosts uploaded files behind `url_private`, which needs the bot
    /// token. That makes this different from Discord, whose CDN links are
    /// pre-authorised and which therefore sends no credential at all.
    ///
    /// The URL comes out of the event payload, so the host is attacker-chosen
    /// in the sense that matters: a crafted event naming an external host would
    /// walk the bot token straight off the workspace. The token goes only to
    /// hosts Slack actually serves files from.
    fn is_slack_file_host(url: &str) -> bool {
        let Ok(parsed) = reqwest::Url::parse(url) else {
            return false;
        };
        if parsed.scheme() != "https" {
            return false;
        }
        parsed
            .host_str()
            .is_some_and(|h| h == "slack.com" || h.ends_with(".slack.com"))
    }

    /// One marker per inbound file: an `[IMAGE:…]` for anything that survives
    /// the shared policy, a visible note for anything that does not. Never
    /// silent — a user who sent a screenshot and got no answer cannot tell a
    /// policy rejection from a bug.
    async fn file_markers(&self, message: &serde_json::Value, sender: &str) -> Vec<String> {
        let Some(files) = message.get("files").and_then(|f| f.as_array()) else {
            return Vec::new();
        };
        let (max_images, _) = self.multimodal.effective_limits();
        let cap = crate::channels::media::max_bytes(&self.multimodal);
        let client = self.http_client();
        let sender_key = format!("slack:{sender}");

        let mut markers = Vec::new();
        for file in files.iter().take(max_images) {
            let Some(url) = file.get("url_private").and_then(|u| u.as_str()) else {
                continue;
            };
            if !Self::is_slack_file_host(url) {
                tracing::warn!("Slack: refusing to send the bot token to a non-Slack file host");
                markers.push(
                    crate::channels::media::MediaOutcome::Rejected(
                        "Attachment rejected: the file is not hosted by Slack".into(),
                    )
                    .to_marker(),
                );
                continue;
            }
            let claimed = file.get("mimetype").and_then(|c| c.as_str());
            let outcome = crate::channels::media::fetch_image(
                &client,
                url,
                Some(&self.bot_token),
                claimed,
                cap,
                &sender_key,
            )
            .await;
            markers.push(outcome.to_marker());
        }
        markers
    }

    /// Supply the app-level token, which selects Socket Mode.
    ///
    /// A separate builder rather than a fourth constructor argument: every
    /// existing caller keeps compiling, and the factory opts in explicitly.
    #[must_use]
    /// The app-level token this channel was built with, if any.
    ///
    /// Read-only, and it exists so a test can prove the token survived
    /// construction. Presence here is what decides Socket Mode over polling in
    /// [`Self::listen`], and #778 had to wire it through two separate
    /// constructors by hand.
    pub(crate) fn app_token(&self) -> Option<&str> {
        self.app_token.as_deref()
    }

    pub fn with_app_token(mut self, app_token: Option<String>) -> Self {
        self.app_token = app_token.filter(|t| !t.trim().is_empty());
        self
    }

    fn http_client(&self) -> reqwest::Client {
        crate::config::build_runtime_proxy_client("channel.slack")
    }

    /// Check if a Slack user ID is in the allowlist.
    /// Empty list means deny everyone until explicitly configured.
    /// `"*"` means allow everyone.
    fn is_user_allowed(&self, user_id: &str) -> bool {
        self.allowed_users
            .read()
            .map(|users| users.iter().any(|u| u == "*" || u == user_id))
            .unwrap_or(false)
    }

    /// Append a freshly-paired identity to the runtime allowlist (deduped) so
    /// access is effective immediately. The persisted config (saved by the
    /// pairing core) is the source of truth across restarts.
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

    /// Get the bot's own user ID so we can ignore our own messages
    async fn get_bot_user_id(&self) -> Option<String> {
        let resp: serde_json::Value = self
            .http_client()
            .get("https://slack.com/api/auth.test")
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;

        resp.get("user_id")
            .and_then(|u| u.as_str())
            .map(String::from)
    }

    /// Resolve the thread identifier for inbound Slack messages.
    /// Replies carry `thread_ts` (root thread id); top-level messages only have `ts`.
    fn inbound_thread_ts(msg: &serde_json::Value, ts: &str) -> Option<String> {
        msg.get("thread_ts")
            .and_then(|t| t.as_str())
            .or(if ts.is_empty() { None } else { Some(ts) })
            .map(str::to_string)
    }
}

#[async_trait]
impl Channel for SlackChannel {
    fn name(&self) -> &str {
        "slack"
    }

    fn render_target(&self) -> crate::channels::format::RenderTarget {
        // Slack renders its own mrkdwn (`*bold*`, `_italic_`, `<url|text>`), not
        // CommonMark, so the agent's `**bold**`/`[](url)`/tables leak today.
        // LightMarkup{Slack} converts to mrkdwn and escapes `&`/`<`/`>` as Slack's
        // text field requires.
        crate::channels::format::RenderTarget::LightMarkup {
            links: crate::channels::format::LinkStyle::Slack,
        }
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        // Attachments first: the markers must come out of the text before it is
        // rendered, or they reach the reader as literal `[IMAGE:…]`.
        let (text, attachments) =
            crate::channels::media::parse_attachment_markers(&message.content);
        if !attachments.is_empty() {
            if !text.is_empty() {
                self.send_text(&text, message).await?;
            }
            for attachment in &attachments {
                self.send_attachment(message, attachment).await?;
            }
            return Ok(());
        }

        self.send_text(&message.content, message).await
    }

    /// Slack has no typing indicator a bot can drive, so "working" is shown by
    /// posting a short placeholder and deleting it when the answer is ready.
    ///
    /// Shape chosen deliberately over editing the placeholder into the answer:
    /// the runtime already tears this down on **every** exit path
    /// (`dispatch.rs` cancels the token and awaits the task before it matches
    /// on the turn's result), so a failed, errored or cancelled turn cleans up
    /// for free. Editing would need `send` to own the handle as well, giving
    /// one resource two owners.
    ///
    /// Idempotent: the runtime calls this every 4 seconds.
    async fn start_typing(&self, recipient: &str, thread_ts: Option<&str>) -> anyhow::Result<()> {
        if self.has_working_notice(recipient) {
            return Ok(());
        }

        let body = Self::working_notice_body(recipient, thread_ts);

        let resp = self
            .http_client()
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await?;
        let text = resp.text().await.unwrap_or_default();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if !Self::api_response_is_ok(&parsed) {
            // Not an error worth failing the turn over: the answer still comes.
            tracing::debug!("Slack working notice not posted: {text}");
            return Ok(());
        }
        if let Some(ts) = parsed.get("ts").and_then(|t| t.as_str()) {
            if let Some(stale) = self.remember_working_notice(recipient, ts.to_string()) {
                // Should not happen while `has_working_notice` guards the post,
                // but an orphaned notice is exactly the lie this plan exists to
                // avoid, so remove it rather than leak it.
                let _ = self.delete_message(recipient, &stale).await;
            }
        }
        Ok(())
    }

    /// Remove this conversation's placeholder. Runs on every exit path,
    /// including a turn that errored or was cancelled.
    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()> {
        let Some(ts) = self.take_working_notice(recipient) else {
            return Ok(());
        };
        self.delete_message(recipient, &ts).await
    }

    /// Slack can deliver attachments, so the model is told the marker syntax.
    /// Telling a channel that cannot leaks `[IMAGE:…]` to the reader as literal
    /// text, which is why this is per-channel rather than a default.
    fn delivery_instructions(&self) -> Option<&'static str> {
        static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        Some(TEXT.get_or_init(|| crate::channels::media::delivery_instructions_for("Slack")))
    }

    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        self.listen_inner(tx, cancel).await
    }
}

impl SlackChannel {
    async fn send_text(&self, content: &str, message: &SendMessage) -> anyhow::Result<()> {
        // Render per-platform, then split without cutting a fenced block. The
        // whole reply used to go out in one request, so anything past Slack's
        // limit failed the entire send and the user got nothing at all.
        let blocks = crate::channels::format::render(content, &self.render_target());
        let chunks = crate::channels::format::split_non_empty(&blocks, SLACK_MAX_MESSAGE_LENGTH);

        for (index, chunk) in chunks.iter().enumerate() {
            self.post_chunk(message, chunk).await?;
            if index + 1 < chunks.len() {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }

        Ok(())
    }

    async fn listen_inner(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        // Socket Mode when the operator supplied an app token, polling
        // otherwise. Not a silent fallback: the two differ in what they can
        // see, and `channels.md` says which is which. Polling reads one
        // `conversations.history` page, so it cannot see DMs and cannot see
        // replies inside a thread — including replies to the approval prompt
        // this channel posts into a thread.
        if self
            .app_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
        {
            return self.listen_socket_mode(tx, cancel).await;
        }
        self.listen_polling(tx, cancel).await
    }
}

impl SlackChannel {
    /// Socket Mode: one outbound WebSocket carrying events for every
    /// conversation the bot is in — channels, threads and DMs alike.
    ///
    /// `channel_id` stops being a requirement here and becomes what the schema
    /// always called it: an optional filter.
    async fn listen_socket_mode(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let app_token = self
            .app_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Slack app_token required for Socket Mode"))?;

        let resp = self
            .http_client()
            .post("https://slack.com/api/apps.connections.open")
            .bearer_auth(&app_token)
            .send()
            .await?;
        // A dead app token must reach the supervisor as `Err` so its backoff
        // rises, not be retried at the connect rate forever.
        if crate::channels::fault::is_fatal_auth_status(resp.status()) {
            anyhow::bail!(
                "Slack Socket Mode authentication failed ({}); check the app_token",
                resp.status()
            );
        }
        let body: serde_json::Value = resp.json().await?;
        if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            let err = body
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if crate::channels::fault::slack_error_is_fatal(err) {
                anyhow::bail!("Slack apps.connections.open failed ({err}); check the app_token");
            }
            anyhow::bail!("Slack apps.connections.open returned ok=false ({err})");
        }
        let url = body
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Slack apps.connections.open returned no url"))?;

        let (ws, _) = tokio_tungstenite::connect_async(url).await?;
        let (mut write, mut read) = ws.split();
        let bot_user_id = self.get_bot_user_id().await.unwrap_or_default();
        tracing::info!("Slack channel listening over Socket Mode...");

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!("Slack channel shutting down");
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                frame = read.next() => {
                    let text = match frame {
                        Some(Ok(Message::Text(t))) => t.to_string(),
                        Some(Ok(Message::Close(_))) | None => return Ok(()),
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => return Err(e.into()),
                    };
                    let env: serde_json::Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("Slack Socket Mode: unparseable frame: {e}");
                            continue;
                        }
                    };
                    // Acknowledge first. Slack redelivers anything unacked
                    // within three seconds, and handling can outlast that.
                    if let Some(id) = env.get("envelope_id").and_then(serde_json::Value::as_str) {
                        let ack = serde_json::json!({ "envelope_id": id });
                        if write.send(Message::Text(ack.to_string().into())).await.is_err() {
                            return Ok(());
                        }
                    }
                    if let Some(msg) = Self::socket_event_message(&env, self.channel_id.as_deref()) {
                        let channel_id = msg
                            .get("channel")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        // Socket Mode delivers each event once and the ack above
                        // is the dedup, so the polling cursor has no job here —
                        // and a shared cursor would drop events from a second
                        // conversation whose timestamps run behind the first.
                        let mut cursor = String::new();
                        if matches!(
                            self.handle_inbound(&msg, &bot_user_id, &mut cursor, &channel_id, &tx)
                                .await,
                            InboundOutcome::ReceiverGone
                        ) {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// The inner `event` of a Socket Mode envelope, when it is a user message
    /// this channel should consider.
    ///
    /// Returns the event with `channel` intact so the caller can route the
    /// reply; `classify_inbound` reads `user`, `text`, `ts` and `thread_ts` from
    /// the same object, which is why the shape is passed through rather than
    /// rebuilt.
    pub(crate) fn socket_event_message(
        envelope: &serde_json::Value,
        only_channel: Option<&str>,
    ) -> Option<serde_json::Value> {
        if envelope.get("type").and_then(serde_json::Value::as_str) != Some("events_api") {
            return None;
        }
        let event = envelope.get("payload")?.get("event")?;
        if event.get("type").and_then(serde_json::Value::as_str) != Some("message") {
            return None;
        }
        // Edits, deletions and joins arrive as `message` with a subtype. None of
        // them is someone talking to the bot — except `file_share`, which is
        // how Slack delivers a message that carries an upload. Dropping every
        // subtype meant a photo with a caption was discarded whole, so the bot
        // did not merely miss the image, it never answered at all.
        if !matches!(
            event.get("subtype").and_then(serde_json::Value::as_str),
            None | Some("file_share")
        ) {
            return None;
        }
        if let Some(want) = only_channel.filter(|c| !c.trim().is_empty()) {
            if event.get("channel").and_then(serde_json::Value::as_str) != Some(want) {
                return None;
            }
        }
        Some(event.clone())
    }

    /// The original transport: one `conversations.history` page per tick.
    async fn listen_polling(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let channel_id = self
            .channel_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Slack channel_id required for listening"))?;

        let bot_user_id = self.get_bot_user_id().await.unwrap_or_default();
        let mut last_ts = String::new();

        tracing::info!("Slack channel listening on #{channel_id}...");

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::info!("Slack channel shutting down");
                    return Ok(());
                }
                () = tokio::time::sleep(std::time::Duration::from_secs(3)) => {}
            }

            let mut params = vec![("channel", channel_id.clone()), ("limit", "10".to_string())];
            if !last_ts.is_empty() {
                params.push(("oldest", last_ts.clone()));
            }

            let resp = match self
                .http_client()
                .get("https://slack.com/api/conversations.history")
                .bearer_auth(&self.bot_token)
                .query(&params)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Slack poll error: {e}");
                    continue;
                }
            };

            // See the Mattermost arm: a dead credential must reach the
            // supervisor as `Err`, not be retried at the poll rate forever.
            if crate::channels::fault::is_fatal_auth_status(resp.status()) {
                anyhow::bail!(
                    "Slack authentication failed ({}); check the bot token",
                    resp.status()
                );
            }

            let data: serde_json::Value = match resp.json().await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("Slack parse error: {e}");
                    continue;
                }
            };

            // Slack answers 200 with `{"ok": false, "error": "..."}`, so the
            // status check above cannot see a revoked token on its own.
            if data.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
                let err = data
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if crate::channels::fault::slack_error_is_fatal(err) {
                    anyhow::bail!("Slack authentication failed ({err}); check the bot token");
                }
                tracing::warn!("Slack poll returned ok=false ({err}); retrying");
                continue;
            }

            if let Some(messages) = data.get("messages").and_then(|m| m.as_array()) {
                // Messages come newest-first, reverse to process oldest first
                for msg in messages.iter().rev() {
                    match self
                        .handle_inbound(msg, &bot_user_id, &mut last_ts, &channel_id, &tx)
                        .await
                    {
                        InboundOutcome::Continue => {}
                        InboundOutcome::ReceiverGone => return Ok(()),
                    }
                }
            }
        }
    }

    /// Slack answers `auth.test` with **HTTP 200 and `{"ok": false}`** for a
    /// revoked or invalid token, so a status-only probe reported healthy for
    /// exactly the condition it exists to catch. `send()` in this same file
    /// already reads the `ok` field; this now does too.
    async fn health_check(&self) -> bool {
        let Ok(resp) = self
            .http_client()
            .get("https://slack.com/api/auth.test")
            .bearer_auth(&self.bot_token)
            .send()
            .await
        else {
            return false;
        };
        if !resp.status().is_success() {
            return false;
        }
        let Ok(body) = resp.json::<serde_json::Value>().await else {
            return false;
        };
        Self::api_response_is_ok(&body)
    }
}

#[cfg(test)]
mod working_notice_tests {
    use super::*;

    fn channel() -> SlackChannel {
        SlackChannel::new("xoxb-test".into(), Some("C_CHAN".into()), vec!["*".into()])
    }

    /// Two conversations must not clear each other's placeholder. Mirrors
    /// `start_typing_replaces_that_recipients_handle_only` in `telegram.rs`.
    #[test]
    fn a_notice_is_kept_per_recipient() {
        let ch = channel();
        ch.remember_working_notice("C_ONE", "111.1".into());
        ch.remember_working_notice("C_TWO", "222.2".into());

        assert_eq!(ch.take_working_notice("C_ONE").as_deref(), Some("111.1"));
        assert!(
            ch.has_working_notice("C_TWO"),
            "taking one conversation's notice must leave the other alone"
        );
        assert_eq!(ch.take_working_notice("C_TWO").as_deref(), Some("222.2"));
    }

    /// The runtime calls `start_typing` every four seconds. Without this the
    /// chat fills with placeholders instead of showing one.
    #[test]
    fn a_second_start_does_not_post_a_second_notice() {
        let ch = channel();
        assert!(!ch.has_working_notice("C_ONE"), "nothing posted yet");
        ch.remember_working_notice("C_ONE", "111.1".into());
        assert!(
            ch.has_working_notice("C_ONE"),
            "a refresh tick must find the existing notice and do nothing"
        );
    }

    /// Taking a notice that was never posted is not an error: `stop_typing`
    /// runs on every exit path, including turns where the post failed.
    #[test]
    fn stopping_without_a_notice_is_harmless() {
        assert!(channel().take_working_notice("C_NONE").is_none());
    }

    /// A notice is removed exactly once, so a second `stop_typing` cannot
    /// delete a message that a later turn has since posted.
    #[test]
    fn a_notice_is_taken_only_once() {
        let ch = channel();
        ch.remember_working_notice("C_ONE", "111.1".into());
        assert_eq!(ch.take_working_notice("C_ONE").as_deref(), Some("111.1"));
        assert_eq!(
            ch.take_working_notice("C_ONE"),
            None,
            "the second stop must find nothing left to delete"
        );
    }

    /// The placeholder goes where the answer goes. In a thread, a notice
    /// posted to the main channel is noise for everyone else in it.
    #[test]
    fn a_notice_follows_the_reply_into_its_thread() {
        let threaded = SlackChannel::working_notice_body("C_CHAN", Some("1700000001.000100"));
        assert_eq!(
            threaded.get("thread_ts").and_then(|t| t.as_str()),
            Some("1700000001.000100"),
            "a threaded reply must get a threaded notice: {threaded}"
        );
        assert_eq!(
            threaded.get("channel").and_then(|c| c.as_str()),
            Some("C_CHAN")
        );

        // A channel-level turn carries no thread, and must not invent one.
        let plain = SlackChannel::working_notice_body("C_CHAN", None);
        assert!(
            plain.get("thread_ts").is_none(),
            "a channel reply must not be forced into a thread: {plain}"
        );
    }

    /// The text has to read as transient. A placeholder that looks like an
    /// answer is worse than silence.
    #[test]
    fn the_notice_reads_as_temporary() {
        assert!(SlackChannel::WORKING_NOTICE
            .to_lowercase()
            .contains("working"));
        assert!(
            SlackChannel::WORKING_NOTICE.len() < 40,
            "the placeholder must be short: {}",
            SlackChannel::WORKING_NOTICE
        );
    }
}

#[cfg(test)]
mod outbound_media_tests {
    use super::*;
    use crate::channels::media::{parse_attachment_markers, AttachmentKind};

    fn channel() -> SlackChannel {
        SlackChannel::new("xoxb-test".into(), Some("C_CHAN".into()), vec!["*".into()])
    }

    /// Slack was the last tier channel that could not deliver an attachment,
    /// and the model was never told the syntax because of it.
    #[test]
    fn slack_tells_the_model_the_marker_syntax() {
        let text = channel()
            .delivery_instructions()
            .expect("slack can deliver attachments");
        assert!(text.contains("Slack"), "{text}");
        for marker in ["[IMAGE:", "[DOCUMENT:", "[VIDEO:", "[AUDIO:", "[VOICE:"] {
            assert!(text.contains(marker), "missing {marker}: {text}");
        }
    }

    /// One vocabulary across every delivering channel: a reply written on one
    /// must not leak literal markers on another.
    #[test]
    fn slack_and_discord_teach_the_same_markers() {
        let slack = channel().delivery_instructions().expect("slack");
        let discord = crate::channels::discord::DiscordChannel::new(
            "t".into(),
            None,
            vec!["*".into()],
            false,
            false,
        )
        .delivery_instructions()
        .expect("discord");
        let strip = |s: &str| {
            s.replace("Slack", "<platform>")
                .replace("Discord", "<platform>")
        };
        assert_eq!(
            strip(slack),
            strip(discord),
            "the marker vocabulary must not fork per channel"
        );
    }

    /// A marker becomes an upload and leaves the text clean, so the reader
    /// never sees `[IMAGE:…]`.
    #[test]
    fn a_marker_is_split_off_and_the_text_stays_clean() {
        let (text, attachments) =
            parse_attachment_markers("here is the chart [IMAGE:/w/chart.png] enjoy");
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].kind, AttachmentKind::Image);
        assert_eq!(attachments[0].target, "/w/chart.png");
        assert!(
            !text.contains("[IMAGE:"),
            "the marker must not reach the reader: {text}"
        );
    }

    /// The reserve call answers with the URL to `POST` to and the id to
    /// complete with. A missing field here is the difference between an upload
    /// and a confusing failure two calls later.
    #[test]
    fn an_upload_ticket_needs_both_the_url_and_the_file_id() {
        let good = serde_json::json!({
            "ok": true,
            "upload_url": "https://files.slack.com/upload/v1/ABC",
            "file_id": "F123"
        });
        assert_eq!(
            SlackChannel::upload_ticket(&good),
            Some((
                "https://files.slack.com/upload/v1/ABC".to_string(),
                "F123".to_string()
            ))
        );

        for missing in [
            serde_json::json!({ "ok": true, "file_id": "F123" }),
            serde_json::json!({ "ok": true, "upload_url": "https://x/y" }),
            serde_json::json!({ "ok": true, "upload_url": "", "file_id": "F123" }),
            serde_json::json!({ "ok": true, "upload_url": "https://x/y", "file_id": "  " }),
            serde_json::json!({ "ok": false, "error": "invalid_auth" }),
        ] {
            assert_eq!(
                SlackChannel::upload_ticket(&missing),
                None,
                "an incomplete ticket must not be used: {missing}"
            );
        }
    }

    /// The confinement that stops a reply exfiltrating host files. A prompt
    /// injection naming the config would otherwise upload provider keys and the
    /// bot token into the channel.
    #[test]
    fn an_attachment_outside_the_workspace_is_refused() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");

        let inside = workspace.path().join("ok.txt");
        std::fs::write(&inside, b"fine").expect("write");
        let secret = outside.path().join("config.toml");
        std::fs::write(&secret, b"api_key = 'x'").expect("write");

        assert!(crate::channels::media::path_within_workspace(
            &inside,
            workspace.path()
        ));
        assert!(
            !crate::channels::media::path_within_workspace(&secret, workspace.path()),
            "a file outside the workspace must be blocked"
        );
        assert!(
            !crate::channels::media::path_within_workspace(
                &workspace.path().join("..").join("config.toml"),
                workspace.path()
            ),
            "a traversal must be blocked"
        );
    }
}

#[cfg(test)]
mod media_tests {
    use super::*;

    fn channel() -> SlackChannel {
        SlackChannel::new("xoxb-test".into(), Some("C_CHAN".into()), vec!["*".into()])
    }

    /// Slack delivers an upload as `message` with `subtype: "file_share"`.
    /// Dropping every subtype meant a captioned screenshot was discarded whole,
    /// so the bot did not answer at all — a worse symptom than the missing
    /// image this plan set out to fix.
    #[test]
    fn a_file_share_event_is_a_message_and_the_other_subtypes_are_not() {
        let envelope = |subtype: serde_json::Value| {
            serde_json::json!({
                "type": "events_api",
                "payload": { "event": {
                    "type": "message", "subtype": subtype,
                    "channel": "C_CHAN", "user": "U1", "text": "look",
                    "ts": "1700000001.000100"
                }}
            })
        };
        assert!(
            SlackChannel::socket_event_message(&envelope(serde_json::json!("file_share")), None)
                .is_some(),
            "an upload must reach the agent"
        );
        for ignored in ["message_changed", "message_deleted", "channel_join"] {
            assert!(
                SlackChannel::socket_event_message(&envelope(serde_json::json!(ignored)), None)
                    .is_none(),
                "{ignored} is not someone talking to the bot"
            );
        }
    }

    /// A photo posted with no caption has empty `text`. Keying "is this a
    /// message" on text alone dropped it.
    #[test]
    fn an_upload_with_no_caption_is_still_delivered() {
        let ch = channel();
        let msg = serde_json::json!({
            "user": "U1", "text": "", "ts": "1700000001.000100",
            "files": [{ "url_private": "https://files.slack.com/f/x.png", "mimetype": "image/png" }]
        });
        assert!(
            matches!(
                ch.classify_inbound(&msg, "U_BOT", "", "C_CHAN"),
                SlackInbound::Deliver(_)
            ),
            "an image with no caption is still a message"
        );

        // Still empty when there is genuinely nothing.
        let bare = serde_json::json!({ "user": "U1", "text": "", "ts": "1700000001.000100" });
        assert!(matches!(
            ch.classify_inbound(&bare, "U_BOT", "", "C_CHAN"),
            SlackInbound::EmptyOrSeen
        ));
    }

    /// The bot token is a workspace credential and the file URL comes out of
    /// the event payload. A crafted event naming an external host would walk
    /// the token off the workspace, so the host is checked before the token is
    /// attached. Discord has no equivalent because its CDN links carry no
    /// credential at all.
    #[test]
    fn the_bot_token_only_goes_to_slack_file_hosts() {
        for good in [
            "https://files.slack.com/files-pri/T1-F1/x.png",
            "https://slack.com/files-pri/x.png",
        ] {
            assert!(SlackChannel::is_slack_file_host(good), "{good}");
        }
        for bad in [
            "https://files.slack.com.evil.test/x.png",
            "https://evil.test/x.png",
            "http://files.slack.com/x.png",
            "https://notslack.com/x.png",
            "not a url",
        ] {
            assert!(
                !SlackChannel::is_slack_file_host(bad),
                "the token must not be sent to {bad}"
            );
        }
    }

    /// A file on a host Slack does not serve becomes a visible note rather than
    /// silence, and no token is sent to fetch it.
    #[tokio::test]
    async fn a_file_on_a_foreign_host_is_refused_with_a_note() {
        let ch = channel();
        let msg = serde_json::json!({
            "files": [{ "url_private": "https://evil.test/x.png", "mimetype": "image/png" }]
        });
        let markers = ch.file_markers(&msg, "U1").await;
        assert_eq!(markers.len(), 1, "the refusal must be visible: {markers:?}");
        assert!(
            markers[0].contains("not hosted by Slack"),
            "the note must say why: {markers:?}"
        );
    }

    /// The operator's `[multimodal]` cap decides how many images are fetched,
    /// not a number this channel invented.
    #[tokio::test]
    async fn no_more_files_are_fetched_than_the_operator_allows() {
        let mut multimodal = crate::config::MultimodalConfig::default();
        multimodal.max_images = 1;
        let ch = channel().with_multimodal(multimodal);
        let msg = serde_json::json!({
            "files": [
                { "url_private": "https://evil.test/a.png", "mimetype": "image/png" },
                { "url_private": "https://evil.test/b.png", "mimetype": "image/png" }
            ]
        });
        let markers = ch.file_markers(&msg, "U1").await;
        assert_eq!(
            markers.len(),
            1,
            "the second file is over the cap: {markers:?}"
        );
    }

    /// No `files` array at all must produce nothing, not an empty marker that
    /// would read to the model as a failed attachment.
    #[tokio::test]
    async fn a_message_without_files_produces_no_markers() {
        let ch = channel();
        let msg = serde_json::json!({ "text": "just words" });
        assert!(ch.file_markers(&msg, "U1").await.is_empty());
    }
}

#[cfg(test)]
mod tests {
    /// The allowlist gate lived inside `listen()`'s poll loop, which no test
    /// enters — deleting the gate line left every test in this file passing.
    #[test]
    fn an_unlisted_user_is_rejected() {
        let listed = SlackChannel::new("xoxb-placeholder".into(), None, vec!["U_ALLOWED".into()]);
        let msg = serde_json::json!({
            "ts": "1700000001.000100",
            "user": "U_OTHER",
            "text": "status please"
        });

        assert!(
            matches!(
                listed.classify_inbound(&msg, "U_BOT", "", "C_CHAN"),
                SlackInbound::Unauthorized { .. }
            ),
            "a user outside allowed_users must not be delivered"
        );

        // Control: the same message from the allowlisted user IS delivered, so
        // this cannot pass because the fixture was malformed.
        let mut allowed = msg.clone();
        allowed["user"] = serde_json::json!("U_ALLOWED");
        match listed.classify_inbound(&allowed, "U_BOT", "", "C_CHAN") {
            SlackInbound::Deliver(m) => {
                assert_eq!(m.sender, "U_ALLOWED");
                assert_eq!(m.reply_target, "C_CHAN");
                assert_eq!(m.content, "status please");
            }
            other => panic!("expected Deliver, got {other:?}"),
        }
    }

    #[test]
    fn the_bots_own_message_and_stale_cursors_are_skipped() {
        let ch = SlackChannel::new("xoxb-placeholder".into(), None, vec!["*".into()]);
        let msg = serde_json::json!({
            "ts": "1700000001.000100",
            "user": "U_BOT",
            "text": "my own reply"
        });
        assert!(matches!(
            ch.classify_inbound(&msg, "U_BOT", "", "C_CHAN"),
            SlackInbound::Own
        ));

        // Older than the cursor, and empty text.
        let mut old = msg.clone();
        old["user"] = serde_json::json!("U_SOMEONE");
        assert!(matches!(
            ch.classify_inbound(&old, "U_BOT", "1700000009.000100", "C_CHAN"),
            SlackInbound::EmptyOrSeen
        ));
        let mut empty = old.clone();
        empty["text"] = serde_json::json!("");
        assert!(matches!(
            ch.classify_inbound(&empty, "U_BOT", "", "C_CHAN"),
            SlackInbound::EmptyOrSeen
        ));
    }

    /// The splitter test above proves the constant and the splitter behave; a
    /// `send()` that never calls the splitter passes it anyway. This asserts
    /// the wiring, since `send()` needs a live API to drive.
    #[test]
    fn slack_send_routes_through_the_splitter() {
        let src = include_str!("slack.rs");
        let production = src.split("#[cfg(test)]").next().expect("source");

        // One hop since outbound attachments split `send`: the text half moved
        // to `send_text`, and the splitter moved with it. Follow the hop rather
        // than dropping the assertion — a `send` that reaches neither is the
        // regression this exists for.
        let send_body = production
            .split("async fn send(")
            .nth(1)
            .expect("send exists");
        let next_fn = send_body.find("\n    async fn ").unwrap_or(send_body.len());
        let send_only = &send_body[..next_fn];
        assert!(
            send_only.contains("self.send_text("),
            "send() must delegate its text to send_text()"
        );
        // Mentioning `send_text` is not the same as routing through it: the
        // attachment branch names it too, so a `send` that posted the plain
        // path directly still satisfied the line above. Only `send_text` and
        // `send_attachment` may reach the API from here.
        let chunk_call = format!("self.post_{}(", "chunk");
        assert!(
            !send_only.contains(chunk_call.as_str()),
            "send() must not post directly; the splitter lives behind send_text()"
        );

        let text_body = production
            .split("async fn send_text(")
            .nth(1)
            .expect("send_text exists");
        let split_at = text_body
            .find("format::split_non_empty(")
            .expect("send_text must route through format::split_non_empty");
        let text_next = text_body.find("\n    async fn ").unwrap_or(text_body.len());
        assert!(
            split_at < text_next,
            "the split call must be inside send_text(), not a later function"
        );
    }

    /// The whole reply used to go out in one request, so anything past the
    /// limit failed the entire send and the user got nothing at all.
    #[test]
    fn long_reply_is_split_on_slack() {
        let long = "word ".repeat(SLACK_MAX_MESSAGE_LENGTH);
        let blocks = crate::channels::format::render(
            &long,
            &crate::channels::format::RenderTarget::LightMarkup {
                links: crate::channels::format::LinkStyle::Slack,
            },
        );
        let chunks = crate::channels::format::split(&blocks, SLACK_MAX_MESSAGE_LENGTH);
        assert!(
            chunks.len() > 1,
            "expected several chunks, got {}",
            chunks.len()
        );
        for chunk in &chunks {
            assert!(
                chunk.chars().count() <= SLACK_MAX_MESSAGE_LENGTH,
                "a chunk exceeded the limit: {} chars",
                chunk.chars().count()
            );
        }
    }

    /// Slack answers a revoked token with HTTP 200 and `{"ok": false}`, so the
    /// old status-only probe could not fail for the one condition it existed
    /// to catch.
    #[test]
    fn slack_health_check_fails_on_ok_false() {
        let revoked = serde_json::json!({"ok": false, "error": "invalid_auth"});
        assert!(
            !SlackChannel::api_response_is_ok(&revoked),
            "a 200 with ok:false is not healthy"
        );

        let good = serde_json::json!({"ok": true, "user_id": "U0000000000"});
        assert!(SlackChannel::api_response_is_ok(&good));

        // A body with no `ok` at all is not evidence of health either.
        assert!(!SlackChannel::api_response_is_ok(&serde_json::json!({})));
    }

    use super::*;

    fn envelope(event: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": { "event": event }
        })
    }

    /// The gap Socket Mode exists to close. Polling reads one
    /// `conversations.history` page, so a DM never arrived at all — the owner
    /// sent one and the bot stayed silent.
    #[test]
    fn socket_mode_accepts_a_direct_message() {
        let env = envelope(serde_json::json!({
            "type": "message", "user": "U1", "text": "hai",
            "ts": "1.1", "channel": "D0PRIVATE"
        }));
        let got = SlackChannel::socket_event_message(&env, None).expect("a DM is a message");
        assert_eq!(got.get("channel").unwrap(), "D0PRIVATE");
        assert_eq!(got.get("text").unwrap(), "hai");
    }

    /// The other half: the approval prompt is posted into a thread, and a reply
    /// typed there was invisible to the polling transport, so every gated tool
    /// call auto-denied.
    #[test]
    fn socket_mode_accepts_a_reply_inside_a_thread() {
        let env = envelope(serde_json::json!({
            "type": "message", "user": "U1", "text": "approve shell",
            "ts": "2.2", "thread_ts": "1.1", "channel": "C0CHAN"
        }));
        let got = SlackChannel::socket_event_message(&env, None).expect("a thread reply arrives");
        assert_eq!(got.get("thread_ts").unwrap(), "1.1");
    }

    #[test]
    fn socket_mode_filters_by_channel_id_when_one_is_configured() {
        let env = envelope(serde_json::json!({
            "type": "message", "user": "U1", "text": "hai",
            "ts": "1.1", "channel": "C0OTHER"
        }));
        assert!(SlackChannel::socket_event_message(&env, Some("C0WANTED")).is_none());
        assert!(SlackChannel::socket_event_message(&env, Some("C0OTHER")).is_some());
        // Blank is not a filter: under Socket Mode the key is optional.
        assert!(SlackChannel::socket_event_message(&env, Some("   ")).is_some());
    }

    #[test]
    fn socket_mode_ignores_what_is_not_someone_talking() {
        // Edits, deletions and joins all arrive as `message` with a subtype.
        let edited = envelope(serde_json::json!({
            "type": "message", "subtype": "message_changed",
            "user": "U1", "text": "hai", "ts": "1.1", "channel": "C0"
        }));
        assert!(SlackChannel::socket_event_message(&edited, None).is_none());

        let reaction = envelope(serde_json::json!({
            "type": "reaction_added", "user": "U1", "ts": "1.1", "channel": "C0"
        }));
        assert!(SlackChannel::socket_event_message(&reaction, None).is_none());

        let hello = serde_json::json!({ "type": "hello" });
        assert!(SlackChannel::socket_event_message(&hello, None).is_none());
    }

    /// An event still classifies through the same allowlist the polling
    /// transport used — one gate, not two.
    #[test]
    fn socket_mode_events_go_through_the_same_allowlist() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["U_ALLOWED".into()]);
        let env = envelope(serde_json::json!({
            "type": "message", "user": "U_DENIED", "text": "hai",
            "ts": "1.1", "channel": "C0"
        }));
        let event = SlackChannel::socket_event_message(&env, None).unwrap();
        assert!(matches!(
            ch.classify_inbound(&event, "UBOT", "", "C0"),
            SlackInbound::Unauthorized { .. }
        ));
    }

    #[test]
    fn slack_render_target_is_lightmarkup_slack() {
        // Assert on the CHANNEL — fails against the pre-wiring Plain default.
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec![]);
        assert_eq!(
            ch.render_target(),
            crate::channels::format::RenderTarget::LightMarkup {
                links: crate::channels::format::LinkStyle::Slack
            }
        );
    }

    #[test]
    fn slack_converts_commonmark_to_mrkdwn() {
        // The real fix: `**bold**` (which Slack shows literally) becomes `*bold*`
        // (Slack mrkdwn), and a markdown link becomes Slack's `<url|text>`.
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec![]);
        let out = crate::channels::format::render_to_string(
            "**bold** and [docs](https://x.io)",
            &ch.render_target(),
        );
        assert_eq!(out, "*bold* and <https://x.io|docs>");
    }

    #[test]
    fn slack_channel_name() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec![]);
        assert_eq!(ch.name(), "slack");
    }

    #[test]
    fn slack_channel_with_channel_id() {
        let ch = SlackChannel::new("xoxb-fake".into(), Some("C12345".into()), vec![]);
        assert_eq!(ch.channel_id, Some("C12345".to_string()));
    }

    #[test]
    fn empty_allowlist_denies_everyone() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec![]);
        assert!(!ch.is_user_allowed("U12345"));
        assert!(!ch.is_user_allowed("anyone"));
    }

    #[test]
    fn wildcard_allows_everyone() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["*".into()]);
        assert!(ch.is_user_allowed("U12345"));
    }

    #[test]
    fn specific_allowlist_filters() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["U111".into(), "U222".into()]);
        assert!(ch.is_user_allowed("U111"));
        assert!(ch.is_user_allowed("U222"));
        assert!(!ch.is_user_allowed("U333"));
    }

    #[test]
    fn allowlist_exact_match_not_substring() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["U111".into()]);
        assert!(!ch.is_user_allowed("U1111"));
        assert!(!ch.is_user_allowed("U11"));
    }

    #[test]
    fn allowlist_empty_user_id() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["U111".into()]);
        assert!(!ch.is_user_allowed(""));
    }

    #[test]
    fn allowlist_case_sensitive() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["U111".into()]);
        assert!(ch.is_user_allowed("U111"));
        assert!(!ch.is_user_allowed("u111"));
    }

    #[test]
    fn allowlist_wildcard_and_specific() {
        let ch = SlackChannel::new("xoxb-fake".into(), None, vec!["U111".into(), "*".into()]);
        assert!(ch.is_user_allowed("U111"));
        assert!(ch.is_user_allowed("anyone"));
    }

    // ── Message ID edge cases ─────────────────────────────────────

    #[test]
    fn slack_message_id_format_includes_channel_and_ts() {
        // Verify that message IDs follow the format: slack_{channel_id}_{ts}
        let ts = "1234567890.123456";
        let channel_id = "C12345";
        let expected_id = format!("slack_{channel_id}_{ts}");
        assert_eq!(expected_id, "slack_C12345_1234567890.123456");
    }

    #[test]
    fn slack_message_id_is_deterministic() {
        // Same channel_id + same ts = same ID (prevents duplicates after restart)
        let ts = "1234567890.123456";
        let channel_id = "C12345";
        let id1 = format!("slack_{channel_id}_{ts}");
        let id2 = format!("slack_{channel_id}_{ts}");
        assert_eq!(id1, id2);
    }

    #[test]
    fn slack_message_id_different_ts_different_id() {
        // Different timestamps produce different IDs
        let channel_id = "C12345";
        let id1 = format!("slack_{channel_id}_1234567890.123456");
        let id2 = format!("slack_{channel_id}_1234567890.123457");
        assert_ne!(id1, id2);
    }

    #[test]
    fn slack_message_id_different_channel_different_id() {
        // Different channels produce different IDs even with same ts
        let ts = "1234567890.123456";
        let id1 = format!("slack_C12345_{ts}");
        let id2 = format!("slack_C67890_{ts}");
        assert_ne!(id1, id2);
    }

    #[test]
    fn slack_message_id_no_uuid_randomness() {
        // Verify format doesn't contain random UUID components
        let ts = "1234567890.123456";
        let channel_id = "C12345";
        let id = format!("slack_{channel_id}_{ts}");
        assert!(!id.contains('-')); // No UUID dashes
        assert!(id.starts_with("slack_"));
    }

    #[test]
    fn inbound_thread_ts_prefers_explicit_thread_ts() {
        let msg = serde_json::json!({
            "ts": "123.002",
            "thread_ts": "123.001"
        });

        let thread_ts = SlackChannel::inbound_thread_ts(&msg, "123.002");
        assert_eq!(thread_ts.as_deref(), Some("123.001"));
    }

    #[test]
    fn inbound_thread_ts_falls_back_to_ts() {
        let msg = serde_json::json!({
            "ts": "123.001"
        });

        let thread_ts = SlackChannel::inbound_thread_ts(&msg, "123.001");
        assert_eq!(thread_ts.as_deref(), Some("123.001"));
    }

    #[test]
    fn inbound_thread_ts_none_when_ts_missing() {
        let msg = serde_json::json!({});

        let thread_ts = SlackChannel::inbound_thread_ts(&msg, "");
        assert_eq!(thread_ts, None);
    }

    // ── pairing (/bind, /claim) ──────────────────────────────

    #[test]
    fn add_allowed_identity_runtime_grants_immediate_access() {
        let ch = SlackChannel::new("fake".into(), Some("C1".into()), vec![]);
        assert!(!ch.is_user_allowed("U999"));
        ch.add_allowed_identity_runtime("U999");
        assert!(ch.is_user_allowed("U999"));
        // Dedupes.
        ch.add_allowed_identity_runtime("U999");
        assert_eq!(ch.allowed_users.read().unwrap().len(), 1);
    }

    /// A store-minted "slack" code (the kind `rantaiclaw channels pair` issues)
    /// is accepted on `/claim`: the shared core lands the sender in `allowed_users`
    /// AND `approval_owners`. Drives the same code path the inbound loop invokes.
    #[tokio::test]
    async fn store_minted_slack_code_claims_owner() {
        use crate::channels::pairing::{try_handle_pairing, AllowlistField};
        use crate::security::pairing_store;

        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");

        // Seed a config with a slack section so apply_pairing has a target.
        {
            let mut seed = crate::config::Config::load_or_init().await.unwrap();
            seed.channels_config.slack = Some(crate::config::SlackConfig {
                bot_token: "x".into(),
                app_token: None,
                channel_id: Some("C1".into()),
                allowed_users: vec![],
            });
            seed.save().await.unwrap();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let code = pairing_store::mint(root, "slack", 3_600, None, true, now).unwrap();

        let reply = try_handle_pairing(
            &format!("/claim {code}"),
            "slack",
            AllowlistField::AllowedUsers,
            &["U999".to_string()],
            root,
        )
        .await
        .expect("a /claim must be handled");
        assert!(reply.contains("owner"), "reply was: {reply}");

        let config = crate::config::Config::load_or_init().await.unwrap();
        let users = &config.channels_config.slack.as_ref().unwrap().allowed_users;
        assert!(users.contains(&"U999".to_string()), "users: {users:?}");
        let owners = &config.channels_config.approval_owners;
        assert!(owners.contains(&"U999".to_string()), "owners: {owners:?}");

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }
}
