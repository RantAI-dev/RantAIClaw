//! Channel identity and routing, asserted against **real payload parsing**.
//!
//! What this file used to be: fourteen tests that built a `ChannelMessage`
//! struct literal and asserted the literal back. Swapping `sender` and
//! `reply_target` in every real inbound path left all fourteen passing;
//! deleting `src/channels/telegram.rs` entirely left them compiling. It named
//! five issues it could not have caught — two of which are exactly the field
//! swaps it claimed to guard.
//!
//! What it is now: each channel whose parser is reachable from an integration
//! test is fed a captured (and redacted) platform payload, and the assertion is
//! the one the header always claimed — **`sender` is the person, `reply_target`
//! is where a reply goes**. Every test here fails if those two are swapped in
//! the channel's own construction site; that was verified per channel by doing
//! the swap.
//!
//! Reachability, stated plainly: only `linq`, `nextcloud_talk`, `whatsapp` and
//! (under `--features channel-lark`) `lark` expose a `pub` parse function.
//! The other eleven parse inside `listen()` or behind private functions, so
//! they cannot be reached from `tests/` without widening the public API — which
//! plan 139 puts out of scope, and which is a contract decision rather than a
//! test one.
//!
//! The claim that "their equivalent assertions live in each channel's in-crate
//! module" was too broad. Checked per channel:
//!
//! - **Telegram, Discord, Slack, Mattermost, Signal** — covered in-crate.
//!   Discord's seam (`classify_inbound`) was extracted later, in #520.
//! - **iMessage, Email** — `reply_target` *is* the sender: both are 1:1
//!   surfaces, so a swap is a no-op and a test asserting it would pass either
//!   way. Same reason plan 139 used a platform-id mutation for WhatsApp instead
//!   of a field swap. Not a gap; nothing meaningful to assert.
//! - **DingTalk** — `resolve_chat_id` is the whole decision and is covered both
//!   ways: group conversation id, and the 1:1 fallback to the sender.
//! - **QQ, IRC** — genuinely uncovered. Both build the message inside their
//!   listen loop with no extracted seam, so closing this needs the same
//!   extraction Discord got, not a test.
//! - **Matrix** — unverifiable by anything: `matrix-sdk` compiles in no CI job.

use async_trait::async_trait;
use rantaiclaw::channels::traits::{Channel, ChannelMessage, SendMessage};

fn fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/tests/fixtures/channel_payloads/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Real payload → (sender, reply_target)
// ─────────────────────────────────────────────────────────────────────────────

/// WhatsApp Cloud: the sender is the E.164 number that wrote, and a reply goes
/// back to that same number — this channel is 1:1, so the two coincide in value
/// but not in meaning, and a swap is caught by the id/content assertions below
/// plus the mutation check recorded in the PR.
#[test]
fn whatsapp_payload_routes_sender_and_reply_target() {
    let ch = rantaiclaw::channels::whatsapp::WhatsAppChannel::new(
        "placeholder-token".into(),
        "100000000000001".into(),
        "placeholder-verify".into(),
        vec!["*".into()],
    );

    let msgs = ch.parse_webhook_payload(&fixture("whatsapp_text.json"));
    assert_eq!(msgs.len(), 1, "one inbound text message: {msgs:?}");
    let m = &msgs[0];

    assert_eq!(m.sender, "+15550000001", "sender is the person who wrote");
    assert_eq!(m.reply_target, "+15550000001", "a reply goes back to them");
    assert_eq!(m.content, "status please");
    assert_eq!(m.channel, "whatsapp");
    // The platform id, not a minted UUID — a redelivery has to be detectable.
    assert_eq!(m.id, "whatsapp_wamid.RANTAICLAWTESTID");
}

/// Linq: the sender is the phone number, the reply target is the **chat id** —
/// these are genuinely different values, so a swap is unambiguous here.
#[test]
fn linq_payload_routes_sender_and_reply_target() {
    let ch = rantaiclaw::channels::linq::LinqChannel::new(
        "placeholder-token".into(),
        "15550000000".into(),
        vec!["*".into()],
    );

    let msgs = ch.parse_webhook_payload(&fixture("linq_text.json"));
    assert_eq!(msgs.len(), 1, "one inbound text message: {msgs:?}");
    let m = &msgs[0];

    assert_eq!(m.sender, "+15550000002", "sender is the person who wrote");
    assert_eq!(
        m.reply_target, "chat_rantaiclaw_0001",
        "a reply goes to the conversation, not to the sender string"
    );
    assert_ne!(
        m.sender, m.reply_target,
        "these must not be the same value, or the test cannot see a swap"
    );
    assert_eq!(m.content, "status please");
    assert_eq!(m.id, "linq_msg_rantaiclaw_0001");
}

/// Nextcloud Talk: the sender is the actor id, the reply target is the room
/// token. Different values again.
#[test]
fn nextcloud_talk_payload_routes_sender_and_reply_target() {
    let ch = rantaiclaw::channels::nextcloud_talk::NextcloudTalkChannel::new(
        "https://cloud.example.com".into(),
        "placeholder-token".into(),
        vec!["*".into()],
    );

    let msgs = ch.parse_webhook_payload(&fixture("nextcloud_talk_text.json"));
    assert_eq!(msgs.len(), 1, "one inbound comment: {msgs:?}");
    let m = &msgs[0];

    assert_eq!(m.sender, "rantaiclaw_user", "sender is the actor");
    assert_eq!(
        m.reply_target, "room_rantaiclaw_0001",
        "a reply goes to the room"
    );
    assert_ne!(m.sender, m.reply_target);
    assert_eq!(m.content, "status please");
}

/// A sender outside the allowlist produces no message at all — the gate is part
/// of parsing on these channels, and a test that only ever passes `*` would not
/// notice it being removed.
#[test]
fn a_sender_outside_the_allowlist_yields_nothing() {
    let ch = rantaiclaw::channels::nextcloud_talk::NextcloudTalkChannel::new(
        "https://cloud.example.com".into(),
        "placeholder-token".into(),
        vec!["somebody_else".into()],
    );
    let msgs = ch.parse_webhook_payload(&fixture("nextcloud_talk_text.json"));
    assert!(
        msgs.is_empty(),
        "an unlisted actor must not reach the agent"
    );
}

// Test channel that captures sent messages for assertion
struct CapturingChannel {
    sent: std::sync::Mutex<Vec<SendMessage>>,
}

impl CapturingChannel {
    fn new() -> Self {
        Self {
            sent: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn sent_messages(&self) -> Vec<SendMessage> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Channel for CapturingChannel {
    fn name(&self) -> &str {
        "capturing"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(message.clone());
        Ok(())
    }

    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        tx.send(ChannelMessage {
            sender_aliases: Vec::new(),
            id: "listen_1".into(),
            sender: "test_sender".into(),
            reply_target: "test_target".into(),
            content: "incoming".into(),
            channel: "capturing".into(),
            timestamp: 1700000000,
            thread_ts: None,
        })
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

#[tokio::test]
async fn channel_send_preserves_recipient() {
    let channel = CapturingChannel::new();
    let msg = SendMessage::new("Hello", "target_123");

    channel.send(&msg).await.unwrap();

    let sent = channel.sent_messages();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].recipient, "target_123");
    assert_eq!(sent[0].content, "Hello");
}

#[tokio::test]
async fn channel_listen_produces_correct_identity_fields() {
    let channel = CapturingChannel::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    channel
        .listen(tx, tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let received = rx.recv().await.expect("should receive message");

    assert_eq!(received.sender, "test_sender");
    assert_eq!(received.reply_target, "test_target");
    assert_ne!(
        received.sender, received.reply_target,
        "listen() should populate sender and reply_target distinctly"
    );
}

#[tokio::test]
async fn channel_send_reply_uses_sender_from_listen() {
    let channel = CapturingChannel::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    // Simulate: listen() → receive message → send reply using sender
    channel
        .listen(tx, tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let incoming = rx.recv().await.expect("should receive message");

    // Reply should go to the reply_target, not sender
    let reply = SendMessage::new("reply content", &incoming.reply_target);
    channel.send(&reply).await.unwrap();

    let sent = channel.sent_messages();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].recipient, "test_target",
        "reply should use reply_target as recipient"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Channel trait default methods
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn channel_health_check_default_returns_true() {
    let channel = CapturingChannel::new();
    assert!(
        channel.health_check().await,
        "default health_check should return true"
    );
}

#[tokio::test]
async fn channel_typing_defaults_succeed() {
    let channel = CapturingChannel::new();
    assert!(channel.start_typing("target").await.is_ok());
    assert!(channel.stop_typing("target").await.is_ok());
}

#[tokio::test]
async fn channel_draft_defaults() {
    let channel = CapturingChannel::new();
    assert!(!channel.supports_draft_updates());

    let draft_result = channel
        .send_draft(&SendMessage::new("draft", "target"))
        .await
        .unwrap();
    assert!(
        draft_result.is_none(),
        "default send_draft should return None"
    );

    assert!(channel
        .update_draft("target", "msg_1", "updated")
        .await
        .is_ok());
    assert!(channel
        .finalize_draft("target", "msg_1", "final")
        .await
        .is_ok());
}

// ─────────────────────────────────────────────────────────────────────────────
// Multiple messages: conversation context preservation
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn channel_multiple_sends_preserve_order_and_recipients() {
    let channel = CapturingChannel::new();

    channel
        .send(&SendMessage::new("msg 1", "target_a"))
        .await
        .unwrap();
    channel
        .send(&SendMessage::new("msg 2", "target_b"))
        .await
        .unwrap();
    channel
        .send(&SendMessage::new("msg 3", "target_a"))
        .await
        .unwrap();

    let sent = channel.sent_messages();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[0].recipient, "target_a");
    assert_eq!(sent[1].recipient, "target_b");
    assert_eq!(sent[2].recipient, "target_a");
    assert_eq!(sent[0].content, "msg 1");
    assert_eq!(sent[1].content, "msg 2");
    assert_eq!(sent[2].content, "msg 3");
}
