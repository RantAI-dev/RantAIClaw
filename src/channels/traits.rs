use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// A message received from or sent to a channel
#[derive(Debug, Clone, Default)]
pub struct ChannelMessage {
    pub id: String,
    pub sender: String,
    pub reply_target: String,
    pub content: String,
    pub channel: String,
    pub timestamp: u64,
    /// Platform thread identifier (e.g. Slack `ts`, Discord thread ID).
    /// When set, replies should be posted as threaded responses.
    pub thread_ts: Option<String>,
    /// Additional identity forms for `sender` when a channel resolves one user
    /// to more than one (e.g. Telegram exposes both a numeric id and a
    /// username, but `sender` can only be one). The owner gate checks these
    /// alongside `sender`, matching the per-channel chat allowlist which already
    /// considers every form. Empty for channels with a single identity form.
    pub sender_aliases: Vec<String>,
}

impl ChannelMessage {
    /// Every identity form for the sender: the primary `sender` followed by any
    /// `sender_aliases`. The owner gate matches against all of them so an owner
    /// recorded under any single form (e.g. a Telegram numeric id) is
    /// recognized even when the runtime resolves the sender to another form
    /// (the username) — parity with the two-form chat allowlist.
    pub fn sender_identities(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.sender.as_str()).chain(self.sender_aliases.iter().map(String::as_str))
    }
}

/// Message to send through a channel
#[derive(Debug, Clone)]
pub struct SendMessage {
    pub content: String,
    pub recipient: String,
    pub subject: Option<String>,
    /// Platform thread identifier for threaded replies (e.g. Slack `thread_ts`).
    pub thread_ts: Option<String>,
}

impl SendMessage {
    /// Create a new message with content and recipient
    pub fn new(content: impl Into<String>, recipient: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            recipient: recipient.into(),
            subject: None,
            thread_ts: None,
        }
    }

    /// Create a new message with content, recipient, and subject
    pub fn with_subject(
        content: impl Into<String>,
        recipient: impl Into<String>,
        subject: impl Into<String>,
    ) -> Self {
        Self {
            content: content.into(),
            recipient: recipient.into(),
            subject: Some(subject.into()),
            thread_ts: None,
        }
    }

    /// Set the thread identifier for threaded replies.
    pub fn in_thread(mut self, thread_ts: Option<String>) -> Self {
        self.thread_ts = thread_ts;
        self
    }
}

/// Core channel trait — implement for any messaging platform
#[async_trait]
pub trait Channel: Send + Sync {
    /// Human-readable channel name
    fn name(&self) -> &str;

    /// Which markup dialect this channel renders the agent's markdown into.
    ///
    /// Defaults to [`RenderTarget::Plain`](crate::channels::format::RenderTarget)
    /// — strip markup to readable text — so a channel that has not opted in ships
    /// the safe baseline rather than leaking `##`/`**`. Each channel overrides
    /// this and calls `format::render*` in its own `send()`/`finalize_draft()`.
    fn render_target(&self) -> crate::channels::format::RenderTarget {
        crate::channels::format::RenderTarget::Plain
    }

    /// Extra system-prompt text telling the model how to send attachments on
    /// this channel.
    ///
    /// Defaults to `None`: a channel that cannot deliver media must not tell
    /// the model it can, or the model emits markers that reach the user as
    /// literal text. Telegram overrides it with its marker syntax.
    ///
    /// Was a central `match` on the channel name in `mod.rs`, which meant a new
    /// channel's media support was declared in a file its author never opened.
    fn delivery_instructions(&self) -> Option<&'static str> {
        None
    }

    /// Replace this channel's runtime sender allowlist.
    ///
    /// Called by the channels runtime when `config.toml` changes, so an allowlist
    /// edit from the console or the CLI reaches a **running** listener without a
    /// restart. Before this existed, the console's only way to apply one was to
    /// restart the whole managed service — which is the process hosting the
    /// gateway, so saving an allowlist killed the request handler that saved it.
    ///
    /// Defaults to a no-op: a channel that holds its allowlist as a plain `Vec`
    /// keeps its boot-time list until it is restarted, exactly as before. Channels
    /// that hold it behind a lock override this — see `TelegramChannel`.
    fn apply_allowed_senders(&self, _allowed: &[String]) {}

    /// Send a message through this channel
    async fn send(&self, message: &SendMessage) -> anyhow::Result<()>;

    /// Start listening for incoming messages (long-running).
    ///
    /// **Return contract.** `Ok(())` means the listener finished for a reason
    /// that is not a fault: `cancel` fired, or `tx` was closed. `Err` means a
    /// transport fault — a dropped socket, a rejected ticket, a rate limit. The
    /// supervisor reads the two differently: a clean exit resets the reconnect
    /// backoff, an error escalates it. Reporting a fault as `Ok(())` therefore
    /// produces a reconnect storm that never backs off, which is exactly what
    /// DingTalk did before plan 128.
    ///
    /// **Cancellation.** Implementations SHOULD return promptly once `cancel`
    /// is triggered, and SHOULD complete any teardown the platform expects
    /// first — IRC `QUIT`, IMAP `LOGOUT`, a WebSocket close frame, an HTTP
    /// server's graceful shutdown. The supervisor also drops the future, but
    /// that is a backstop for channels with nothing to tear down, not the
    /// contract: a dropped future sends nothing and frees no port.
    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()>;

    /// Check if the channel is healthy.
    ///
    /// Called by `doctor channels` on demand, and by the supervisor on its
    /// heartbeat — so this **is** what the daemon reports, not just what an
    /// operator can ask for. Both run it under a 10-second timeout; the
    /// supervisor additionally requires three consecutive failures before it
    /// reports the channel unhealthy, so a single blip does not flap the
    /// status. The probe runs in its own task, so a slow one cannot stall
    /// message delivery.
    ///
    /// An implementation MUST be able to fail for the condition it exists to
    /// catch. A probe that only checks the HTTP status of an API that reports
    /// errors in its body reports healthy for a revoked token.
    async fn health_check(&self) -> bool {
        true
    }

    /// Signal that the bot is processing a response (e.g. "typing" indicator).
    /// Implementations should repeat the indicator as needed for their platform.
    async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Stop any active typing indicator.
    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Whether this channel supports progressive message updates via draft edits.
    fn supports_draft_updates(&self) -> bool {
        false
    }

    /// Send an initial draft message. Returns a platform-specific message ID for later edits.
    async fn send_draft(&self, _message: &SendMessage) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// Update a previously sent draft message with new accumulated content.
    async fn update_draft(
        &self,
        _recipient: &str,
        _message_id: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Finalize a draft with the complete response (e.g. apply Markdown formatting).
    async fn finalize_draft(
        &self,
        _recipient: &str,
        _message_id: &str,
        _text: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Cancel and remove a previously sent draft message if the channel supports it.
    async fn cancel_draft(&self, _recipient: &str, _message_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyChannel;

    #[async_trait]
    impl Channel for DummyChannel {
        fn name(&self) -> &str {
            "dummy"
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }

        async fn listen(
            &self,
            tx: tokio::sync::mpsc::Sender<ChannelMessage>,
            _cancel: CancellationToken,
        ) -> anyhow::Result<()> {
            tx.send(ChannelMessage {
                sender_aliases: Vec::new(),
                id: "1".into(),
                sender: "tester".into(),
                reply_target: "tester".into(),
                content: "hello".into(),
                channel: "dummy".into(),
                timestamp: 123,
                thread_ts: None,
            })
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
    }

    #[test]
    fn default_render_target_is_plain() {
        // A channel that has not opted in must ship the safe baseline, not leak
        // markup. Every one of the 17 production channels relies on this default
        // until its own PR wires a specific target.
        assert_eq!(
            DummyChannel.render_target(),
            crate::channels::format::RenderTarget::Plain
        );
    }

    #[test]
    fn channel_message_clone_preserves_fields() {
        let message = ChannelMessage {
            sender_aliases: Vec::new(),
            id: "42".into(),
            sender: "alice".into(),
            reply_target: "alice".into(),
            content: "ping".into(),
            channel: "dummy".into(),
            timestamp: 999,
            thread_ts: None,
        };

        let cloned = message.clone();
        assert_eq!(cloned.id, "42");
        assert_eq!(cloned.sender, "alice");
        assert_eq!(cloned.reply_target, "alice");
        assert_eq!(cloned.content, "ping");
        assert_eq!(cloned.channel, "dummy");
        assert_eq!(cloned.timestamp, 999);
    }

    #[tokio::test]
    async fn default_trait_methods_return_success() {
        let channel = DummyChannel;

        assert!(channel.health_check().await);
        assert!(channel.start_typing("bob").await.is_ok());
        assert!(channel.stop_typing("bob").await.is_ok());
        assert!(channel
            .send(&SendMessage::new("hello", "bob"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn default_draft_methods_return_success() {
        let channel = DummyChannel;

        assert!(!channel.supports_draft_updates());
        assert!(channel
            .send_draft(&SendMessage::new("draft", "bob"))
            .await
            .unwrap()
            .is_none());
        assert!(channel.update_draft("bob", "msg_1", "text").await.is_ok());
        assert!(channel
            .finalize_draft("bob", "msg_1", "final text")
            .await
            .is_ok());
        assert!(channel.cancel_draft("bob", "msg_1").await.is_ok());
    }

    #[tokio::test]
    async fn listen_sends_message_to_channel() {
        let channel = DummyChannel;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        channel.listen(tx, CancellationToken::new()).await.unwrap();

        let received = rx.recv().await.expect("message should be sent");
        assert_eq!(received.sender, "tester");
        assert_eq!(received.content, "hello");
        assert_eq!(received.channel, "dummy");
    }
}
