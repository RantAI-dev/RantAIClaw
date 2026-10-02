//! Recall at the channel door: what a user turn carries into the provider, from
//! the second turn of a conversation on.
//!
//! Each case drives `process_channel_message` against a real SQLite store and
//! reads the requests the provider received, so it asserts on what leaves the
//! process and not on a helper.

use super::dispatch::*;
use super::test_support::*;
use super::*;
use crate::memory::{Memory, MemoryCategory, SqliteMemory};
use std::sync::Arc;
use tempfile::TempDir;

const HEADER: &str = "[Memory context]";
const CHAT: &str = "chat-recall";

struct Door {
    ctx: Arc<ChannelRuntimeContext>,
    provider: Arc<HistoryCaptureProvider>,
    _tmp: TempDir,
}

impl Door {
    /// A door over `memory`, where `owner` is the one named owner.
    fn new(tmp: TempDir, memory: SqliteMemory, owner: &str) -> Self {
        let provider = Arc::new(HistoryCaptureProvider::default());
        let channel: Arc<dyn Channel> = Arc::new(RecordingChannel::default());
        let mut ctx = dispatch_ctx(
            vec![channel],
            provider.clone(),
            routing::RuntimeConfigSlot::default(),
        );
        let inner = Arc::get_mut(&mut ctx).expect("the context is not shared yet");
        inner.memory = Arc::new(memory);
        inner.min_relevance_score = crate::config::MemoryConfig::default().min_relevance_score;
        inner.approval_owners = Arc::new(vec![owner.to_string()]);
        Self {
            ctx,
            provider,
            _tmp: tmp,
        }
    }

    /// Lowers the relevance floor, for a case whose question is mostly the text
    /// it carries.
    fn with_threshold(mut self, threshold: f64) -> Self {
        Arc::get_mut(&mut self.ctx)
            .expect("the context is not shared yet")
            .min_relevance_score = threshold;
        self
    }

    fn message(sender: &str, is_direct: bool, content: &str) -> traits::ChannelMessage {
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg".to_string(),
            sender: sender.to_string(),
            reply_target: CHAT.to_string(),
            content: content.to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
            reply_anchor: None,
            is_direct,
        }
    }

    /// Runs one message through dispatch and returns the messages the provider
    /// received for it.
    async fn turn(&self, msg: traits::ChannelMessage) -> Vec<(String, String)> {
        let before = self.provider.calls.lock().unwrap().len();
        process_channel_message(Arc::clone(&self.ctx), msg, CancellationToken::new()).await;
        let calls = self.provider.calls.lock().unwrap();
        calls[before].clone()
    }

    fn stored_user_turns(&self) -> Vec<String> {
        let key = conversation::ConversationKey::new("test-channel", CHAT).resolve();
        self.ctx
            .conversation_histories
            .lock()
            .unwrap()
            .get(&key)
            .map(|turns| {
                turns
                    .iter()
                    .filter(|t| t.role == "user")
                    .map(|t| t.content.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn last_user(request: &[(String, String)]) -> &str {
    &request
        .iter()
        .rev()
        .find(|(role, _)| role == "user")
        .expect("a user turn reached the provider")
        .1
}

fn blocks_in(request: &[(String, String)]) -> usize {
    request
        .iter()
        .map(|(_, content)| content.matches(HEADER).count())
        .sum()
}

async fn owner_door_with_deploy_note() -> Door {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    mem.store(
        "deploy_window",
        "The deployment window is Friday afternoons after the freeze check passes",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    Door::new(tmp, mem, OWNER_SENDER)
}

/// A note is recalled whenever the question matches it, on the second and third
/// turn as on the first. The stored history keeps the raw words, so the notes
/// of one turn are not carried into the next.
#[tokio::test]
async fn the_second_and_third_turn_of_a_conversation_recall_a_matching_note() {
    let door = owner_door_with_deploy_note().await;
    let questions = [
        "when is the deployment window",
        "and the deployment window again?",
        "remind me of the deployment window",
    ];

    for question in questions {
        let request = door.turn(Door::message(OWNER_SENDER, true, question)).await;
        let user = last_user(&request);
        assert!(
            user.contains("- deploy_window:"),
            "the note did not reach the turn {question:?}:\n{user}"
        );
        assert!(user.ends_with(question));
    }

    assert_eq!(door.stored_user_turns(), questions);
}

/// Every request holds one block: the one in front of the question being
/// answered. The earlier turns are in the request as they were said.
#[tokio::test]
async fn a_turn_carries_exactly_one_block() {
    let door = owner_door_with_deploy_note().await;

    for question in [
        "when is the deployment window",
        "and the deployment window again?",
        "remind me of the deployment window",
    ] {
        let request = door.turn(Door::message(OWNER_SENDER, true, question)).await;
        assert_eq!(blocks_in(&request), 1, "{request:#?}");
    }
}

/// A message that arrives already holding a block is not given a second one.
#[tokio::test]
async fn a_turn_that_already_carries_a_block_is_not_enriched_twice() {
    let door = owner_door_with_deploy_note().await.with_threshold(0.0);
    let carried = format!("{HEADER}\n- earlier: a note\n\nwhen is the deployment window");

    let request = door.turn(Door::message(OWNER_SENDER, true, &carried)).await;

    assert_eq!(blocks_in(&request), 1, "{request:#?}");
    assert!(!last_user(&request).contains("- deploy_window:"));
}

/// The raw rows of a busy chat do not take the places a saved note needs.
#[tokio::test]
async fn a_busy_conversations_raw_rows_do_not_crowd_out_a_saved_note() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    for i in 0..40 {
        mem.store(
            &format!("test-channel_chat_{i}"),
            "deployment window",
            MemoryCategory::Conversation,
            None,
        )
        .await
        .unwrap();
    }
    mem.store(
        "deploy_window",
        "The deployment window is Friday afternoons after the freeze check passes",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    let door = Door::new(tmp, mem, OWNER_SENDER);

    let request = door
        .turn(Door::message(OWNER_SENDER, true, "deployment window?"))
        .await;

    let user = last_user(&request);
    assert!(user.contains("- deploy_window:"), "{user}");
    assert!(!user.contains("- test-channel_chat_"), "{user}");
}

/// A question made only of common words has nothing to match, so no note rides
/// in on "what" and "that". The control is a question with one real word.
#[tokio::test]
async fn a_question_of_only_stopwords_recalls_nothing() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    mem.store(
        "widget_note",
        "what is that widget the team uses",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    let door = Door::new(tmp, mem, OWNER_SENDER);

    let request = door
        .turn(Door::message(OWNER_SENDER, true, "What is that?"))
        .await;
    assert_eq!(blocks_in(&request), 0, "{request:#?}");

    let request = door
        .turn(Door::message(OWNER_SENDER, true, "What is that widget?"))
        .await;
    assert_eq!(blocks_in(&request), 1, "{request:#?}");
}

/// A guest keeps reading only its own conversation, from the second turn on as
/// on the first: the shared tier and another chat's notes stay out.
#[tokio::test]
async fn a_guest_recalls_only_its_own_conversation_on_every_turn() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    let guest_msg = Door::message(GUEST_SENDER, false, "x");
    let own_scope = dispatch::conversation_memory_scope(&guest_msg);
    mem.store(
        "shared_note",
        "The deployment window shared by everyone is Friday",
        MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();
    mem.store(
        "own_note",
        "The deployment window of this chat is Thursday",
        MemoryCategory::Core,
        Some(&own_scope),
    )
    .await
    .unwrap();
    mem.store(
        "other_note",
        "The deployment window of another chat is Monday",
        MemoryCategory::Core,
        Some("another-chat-scope"),
    )
    .await
    .unwrap();
    let door = Door::new(tmp, mem, OWNER_SENDER);

    for question in [
        "when is the deployment window",
        "and the deployment window again?",
    ] {
        let request = door
            .turn(Door::message(GUEST_SENDER, false, question))
            .await;
        let user = last_user(&request);
        assert!(user.contains("- own_note:"), "{user}");
        assert!(!user.contains("shared_note"), "{user}");
        assert!(!user.contains("other_note"), "{user}");
    }
}
