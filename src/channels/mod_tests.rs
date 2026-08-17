//! Tests for the dispatch core that stayed in `mod.rs`.
//!
//! Split out verbatim (plan 121, row 10). They still see the module's
//! private items through `use super::*`, so nothing was widened to move
//! them; only the file boundary changed.

/// The seam moved from a central `match` on the channel name to a trait
/// method. The default must stay `None`: telling a channel that cannot
/// deliver an attachment that it can makes the model emit markers the user
/// sees as literal text.
#[test]
fn delivery_instructions_default_is_none() {
    use crate::channels::traits::Channel;

    let slack = crate::channels::slack::SlackChannel::new(
        "xoxb-placeholder".into(),
        None,
        vec!["*".into()],
    );
    assert!(
        slack.delivery_instructions().is_none(),
        "a channel that cannot deliver media must not claim it can"
    );

    let telegram =
        crate::channels::telegram::TelegramChannel::new("t".into(), vec!["*".into()], false);
    let instructions = telegram
        .delivery_instructions()
        .expect("Telegram is the channel that can deliver media");
    assert!(instructions.contains("[IMAGE:"));

    // Behaviour-preserving: the prompt is assembled exactly as the central
    // `match` assembled it.
    let with =
        prompt::build_channel_system_prompt("BASE", "telegram", "1", false, Some(instructions));
    assert!(with.starts_with("BASE\n\n"));
    assert!(with.contains("[DOCUMENT:"));
    let without = prompt::build_channel_system_prompt("BASE", "irc", "#room", false, None);
    assert!(!without.contains("[DOCUMENT:"));
}

/// Threading moves WHERE replies appear, so the switch has to work — and it
/// is enforced in one place, from a map built here.
#[test]
fn thread_replies_opt_out_disables_threading() {
    use crate::config::schema::{ChannelsConfig, MattermostConfig};

    let mut cc = ChannelsConfig::default();
    assert!(cc.thread_replies, "threading ships on");
    let on = routing::channel_thread_replies(&cc);
    assert_eq!(on.get("discord"), Some(&true));
    assert_eq!(on.get("telegram"), Some(&true));

    // The shared default turns every channel off at once.
    cc.thread_replies = false;
    let off = routing::channel_thread_replies(&cc);
    assert_eq!(off.get("discord"), Some(&false));
    assert_eq!(off.get("slack"), Some(&false));

    // A per-channel key overrides the shared default in both directions.
    cc.mattermost = Some(MattermostConfig {
        url: "https://mm.example.com".into(),
        bot_token: "token".into(),
        channel_id: Some("chan".into()),
        allowed_users: vec!["*".into()],
        thread_replies: Some(true),
        mention_only: None,
    });
    let overridden = routing::channel_thread_replies(&cc);
    assert_eq!(
        overridden.get("mattermost"),
        Some(&true),
        "the per-channel key wins over the shared default"
    );
    assert_eq!(overridden.get("discord"), Some(&false));

    cc.thread_replies = true;
    if let Some(mm) = cc.mattermost.as_mut() {
        mm.thread_replies = Some(false);
    }
    let overridden = routing::channel_thread_replies(&cc);
    assert_eq!(overridden.get("mattermost"), Some(&false));
    assert_eq!(overridden.get("discord"), Some(&true));
}

/// Every channel keeps its allowlist gate inside the polling loop that
/// `listen()` runs, and no test enters that loop — the gate line can be
/// deleted with the whole suite still green. Slack's and Discord's are
/// extracted into a callable function (`classify_inbound`, behaviourally
/// tested in each channel's own file); the rest
/// cannot be reached without a fake transport per channel, so this asserts
/// the wiring by source position instead: the gate call must be present in
/// the function that receives messages, and `listen()` must reach that
/// function. It is deliberately weaker than a behavioural test — it proves
/// the call exists, not that it decides anything. The per-channel
/// `is_*_allowed` unit tests cover the decision.
#[test]
fn every_channel_listen_path_calls_its_allowlist_gate() {
    // (channel, source, fn that receives messages, gate call in it,
    //  fn `listen()` must delegate to — empty when the gate is in listen)
    let wiring: &[(&str, &str, &str, &str, &str)] = &[
        (
            "dingtalk",
            include_str!("dingtalk.rs"),
            "fn listen(",
            "self.is_user_allowed(",
            "",
        ),
        (
            "discord",
            include_str!("discord.rs"),
            "fn classify_inbound(",
            "self.is_user_allowed(",
            "self.classify_inbound(",
        ),
        (
            "imessage",
            include_str!("imessage.rs"),
            "fn listen(",
            "self.is_contact_allowed(",
            "",
        ),
        (
            "irc",
            include_str!("irc.rs"),
            "fn run_session(",
            "self.is_user_allowed(",
            "self.run_session(",
        ),
        (
            "lark (websocket)",
            include_str!("lark.rs"),
            "fn listen_ws(",
            "self.is_user_allowed(",
            "self.listen_ws(",
        ),
        (
            // The webhook half gates inside `parse_event_payload`, which the
            // axum handler calls; `listen_http` only mounts the router.
            "lark (webhook)",
            include_str!("lark.rs"),
            "fn parse_event_payload(",
            "self.is_user_allowed(",
            "",
        ),
        (
            "matrix",
            include_str!("matrix.rs"),
            "fn listen(",
            "MatrixChannel::is_sender_allowed(",
            "",
        ),
        (
            "mattermost",
            include_str!("mattermost.rs"),
            "fn listen(",
            "self.is_user_allowed(",
            "",
        ),
        // QQ gates twice — once per message shape — so both are named.
        // One entry, not two: the C2C and group events used to be separate
        // `listen` arms with a gate each, and this table pinned both by their
        // argument name. They now share one path through `classify_inbound`,
        // so there is one gate to pin. That both event types still reach it is
        // covered by the `classify_inbound_*` unit tests in `qq.rs`.
        (
            "qq",
            include_str!("qq.rs"),
            "fn listen(",
            "self.is_user_allowed(&routed.sender)",
            "Self::classify_inbound(",
        ),
        (
            "signal",
            include_str!("signal.rs"),
            "fn process_envelope(",
            "self.is_sender_allowed(",
            "self.process_envelope(",
        ),
        (
            "slack",
            include_str!("slack.rs"),
            "fn classify_inbound(",
            "self.is_user_allowed(",
            "self.classify_inbound(",
        ),
        (
            "telegram",
            include_str!("telegram.rs"),
            "fn parse_update_message(",
            "self.is_any_user_allowed(",
            "self.parse_update_message(",
        ),
        (
            "whatsapp_web",
            include_str!("whatsapp_web.rs"),
            "fn listen(",
            "Self::allow_inbound(",
            "",
        ),
    ];

    for (channel, src, receiver, gate, delegate) in wiring {
        let production = production_half(src);
        let body = fn_body(production, receiver)
            .unwrap_or_else(|| panic!("{channel}: `{receiver}` not found in production code"));
        assert!(
            body.contains(gate),
            "{channel}: `{receiver}` no longer calls `{gate}` — an inbound \
                 message can reach the agent without passing the allowlist"
        );
        if !delegate.is_empty() {
            let listen = fn_body(production, "fn listen(")
                .unwrap_or_else(|| panic!("{channel}: no `listen` in production code"));
            assert!(
                listen.contains(delegate),
                "{channel}: `listen()` no longer reaches `{delegate}`, so the \
                     gate asserted above is on a dead path"
            );
        }
    }
}

/// Everything before the test module. Cutting at the first `#[cfg(test)]`
/// would be wrong twice over: telegram has a `#[cfg(test)]` helper among
/// its production methods, and whatsapp_web gates its test module on a
/// feature as well.
fn production_half(src: &str) -> &str {
    let cut = ["\n#[cfg(test)]\nmod ", "\n#[cfg(all(test"]
        .iter()
        .filter_map(|marker| src.find(marker))
        .min()
        .unwrap_or(src.len());
    &src[..cut]
}

/// Body of the first function whose signature starts with `header`, ending
/// at the next method declared at the same indentation.
fn fn_body<'a>(production: &'a str, header: &str) -> Option<&'a str> {
    let after = production.split(header).nth(1)?;
    let end = [
        "\n    fn ",
        "\n    async fn ",
        "\n    pub fn ",
        "\n    pub async fn ",
        "\n    pub(crate) fn ",
        "\n    pub(crate) async fn ",
    ]
    .iter()
    .filter_map(|marker| after.find(marker))
    .min()
    .unwrap_or(after.len());
    Some(&after[..end])
}

use super::*;
// The dispatch core moved to its own file in plan 121 row 10; these tests keep
// reaching it, now by name.
use super::dispatch::*;
use crate::memory::{Memory, MemoryCategory, SqliteMemory};
use crate::observability::NoopObserver;
use crate::providers::{ChatMessage, Provider};
use crate::tools::{Tool, ToolResult};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

/// An owner turn must tell the model the sender is a verified owner so a
/// cautious model does not self-refuse owner-only tools; a guest turn must
/// not. The base prompt is preserved either way.
#[test]
fn channel_system_prompt_marks_owner_turns_only() {
    let owner = prompt::build_channel_system_prompt("BASE-PROMPT", "telegram", "12345", true, None);
    let guest =
        prompt::build_channel_system_prompt("BASE-PROMPT", "telegram", "12345", false, None);

    assert!(
        owner.to_lowercase().contains("verified owner"),
        "owner turn must tell the model the sender is a verified owner; got: {owner}"
    );
    assert!(
        !guest.to_lowercase().contains("verified owner"),
        "guest turn must NOT grant owner context; got: {guest}"
    );
    assert!(owner.contains("BASE-PROMPT") && guest.contains("BASE-PROMPT"));
}

#[test]
fn cron_delivery_instruction_present_for_announce_channels() {
    let p = prompt::build_channel_system_prompt("BASE", "telegram", "123456789", false, None);
    assert!(p.contains("BASE"));
    assert!(
        p.contains("cron_add"),
        "must tell the agent how to schedule a delivered message"
    );
    assert!(
        p.contains("123456789"),
        "must carry the reply target as delivery.to"
    );
    assert!(p.contains("telegram"), "must name the origin channel");
}

#[test]
fn no_cron_delivery_instruction_for_unsupported_channel() {
    // A channel the scheduler can't deliver to must NOT promise delivery.
    let p = prompt::build_channel_system_prompt("BASE", "irc", "#room", false, None);
    assert!(
        !p.contains("route the output back"),
        "irc has no announce delivery"
    );
    assert!(
        !p.contains("\"mode\": \"announce\""),
        "irc must not get a delivery template"
    );
}

/// Build a saveable `Config` whose Telegram allowlist is `users`, backed by
/// a `config.toml` inside `dir`. Returns the config plus its path so tests
/// can reload and assert the persisted allowlist.
fn telegram_config_in(dir: &TempDir, users: &[&str]) -> (Config, std::path::PathBuf) {
    let cfg_path = dir.path().join("config.toml");
    let mut config = Config::default();
    config.config_path = cfg_path.clone();
    config.channels_config.telegram = Some(crate::config::TelegramConfig {
        bot_token: "test-token".into(),
        allowed_users: users.iter().map(|u| u.to_string()).collect(),
        stream_mode: crate::config::StreamMode::default(),
        draft_update_interval_ms: 1000,
        interrupt_on_new_message: false,
        mention_only: false,
    });
    (config, cfg_path)
}

fn reload_allowed_users(cfg_path: &std::path::Path) -> Vec<String> {
    let contents = std::fs::read_to_string(cfg_path).unwrap();
    let reloaded: Config = toml::from_str(&contents).unwrap();
    reloaded.channels_config.telegram.unwrap().allowed_users
}

#[tokio::test]
async fn unbind_telegram_removes_wildcard_and_keeps_explicit_entries() {
    let tmp = TempDir::new().unwrap();
    let (config, cfg_path) = telegram_config_in(&tmp, &["*", "rantaiclaw_user"]);

    admin::unbind_telegram_identity(&config, "*").await.unwrap();

    assert_eq!(reload_allowed_users(&cfg_path), vec!["rantaiclaw_user"]);
}

#[tokio::test]
async fn unbind_telegram_normalizes_at_prefix_when_matching() {
    let tmp = TempDir::new().unwrap();
    let (config, cfg_path) = telegram_config_in(&tmp, &["rantaiclaw_user", "123456789"]);

    // Leading '@' is stripped before comparison, mirroring bind/auth.
    admin::unbind_telegram_identity(&config, "@rantaiclaw_user")
        .await
        .unwrap();

    assert_eq!(reload_allowed_users(&cfg_path), vec!["123456789"]);
}

#[tokio::test]
async fn unbind_telegram_missing_identity_is_noop_and_does_not_write() {
    let tmp = TempDir::new().unwrap();
    let (config, cfg_path) = telegram_config_in(&tmp, &["rantaiclaw_user"]);

    admin::unbind_telegram_identity(&config, "someone_else")
        .await
        .unwrap();

    // Nothing removed ⇒ no save ⇒ file was never written.
    assert!(!cfg_path.exists());
}

// ── channels pair (Task 3) ───────────────────────────────

/// Minting an on-demand code into a tempdir profile (the work `channels
/// pair` does after resolving the profile root) returns a non-empty,
/// dash-grouped code that immediately validates for the same surface.
#[test]
fn channels_pair_mints_non_empty_code() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let now = 1_000;

    let code = crate::security::pairing_store::mint(root, "telegram", 15 * 60, None, true, now)
        .expect("mint should succeed");

    assert!(!code.is_empty(), "minted code must not be empty");
    assert!(code.contains('-'), "code should be grouped: {code}");

    // A daemon validating against the same store accepts it without restart.
    let outcome = crate::security::pairing_store::try_consume(root, "telegram", &code, now + 5)
        .expect("consume should succeed");
    assert!(
        outcome.map(|o| o.grant_owner).unwrap_or(false),
        "owner-capable code should consume with grant_owner",
    );
}

fn make_workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    // Create minimal workspace files
    std::fs::write(tmp.path().join("SOUL.md"), "# Soul\nBe helpful.").unwrap();
    std::fs::write(
        tmp.path().join("IDENTITY.md"),
        "# Identity\nName: RantaiClaw",
    )
    .unwrap();
    std::fs::write(tmp.path().join("USER.md"), "# User\nName: Test User").unwrap();
    std::fs::write(
        tmp.path().join("AGENTS.md"),
        "# Agents\nFollow instructions.",
    )
    .unwrap();
    std::fs::write(tmp.path().join("TOOLS.md"), "# Tools\nUse shell carefully.").unwrap();
    std::fs::write(
        tmp.path().join("HEARTBEAT.md"),
        "# Heartbeat\nCheck status.",
    )
    .unwrap();
    std::fs::write(tmp.path().join("MEMORY.md"), "# Memory\nUser likes Rust.").unwrap();
    tmp
}

#[test]
fn effective_channel_message_timeout_secs_clamps_to_minimum() {
    assert_eq!(
        effective_channel_message_timeout_secs(0),
        MIN_CHANNEL_MESSAGE_TIMEOUT_SECS
    );
    assert_eq!(
        effective_channel_message_timeout_secs(15),
        MIN_CHANNEL_MESSAGE_TIMEOUT_SECS
    );
    assert_eq!(effective_channel_message_timeout_secs(300), 300);
}

#[test]
fn channel_message_timeout_budget_scales_with_tool_iterations() {
    assert_eq!(channel_message_timeout_budget_secs(300, 1), 300);
    assert_eq!(channel_message_timeout_budget_secs(300, 2), 600);
    assert_eq!(channel_message_timeout_budget_secs(300, 3), 900);
}

#[test]
fn channel_message_timeout_budget_uses_safe_defaults_and_cap() {
    // 0 iterations falls back to 1x timeout budget.
    assert_eq!(channel_message_timeout_budget_secs(300, 0), 300);
    // Large iteration counts are capped to avoid runaway waits.
    assert_eq!(
        channel_message_timeout_budget_secs(300, 10),
        300 * CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP
    );
}

#[test]
fn context_window_overflow_error_detector_matches_known_messages() {
    let overflow_err = anyhow::anyhow!(
        "OpenAI Codex stream error: Your input exceeds the context window of this model."
    );
    assert!(is_context_window_overflow_error(&overflow_err));

    let other_err = anyhow::anyhow!("OpenAI Codex API error (502 Bad Gateway): error code: 502");
    assert!(!is_context_window_overflow_error(&other_err));
}

#[test]
fn normalize_cached_channel_turns_merges_consecutive_user_turns() {
    let turns = vec![
        ChatMessage::user("forwarded content"),
        ChatMessage::user("summarize this"),
    ];

    let normalized = normalize_cached_channel_turns(turns);
    assert_eq!(normalized.len(), 1);
    assert_eq!(normalized[0].role, "user");
    assert!(normalized[0].content.contains("forwarded content"));
    assert!(normalized[0].content.contains("summarize this"));
}

#[test]
fn normalize_cached_channel_turns_merges_consecutive_assistant_turns() {
    let turns = vec![
        ChatMessage::user("first user"),
        ChatMessage::assistant("assistant part 1"),
        ChatMessage::assistant("assistant part 2"),
        ChatMessage::user("next user"),
    ];

    let normalized = normalize_cached_channel_turns(turns);
    assert_eq!(normalized.len(), 3);
    assert_eq!(normalized[0].role, "user");
    assert_eq!(normalized[1].role, "assistant");
    assert_eq!(normalized[2].role, "user");
    assert!(normalized[1].content.contains("assistant part 1"));
    assert!(normalized[1].content.contains("assistant part 2"));
}

/// Durable history, end to end. Every `ChannelRuntimeContext` in this test
/// module set `history_store: None`, so making the write-through a no-op
/// failed nothing — and history silently stopping surviving restarts is the
/// bug this module exists to fix.
#[test]
fn durable_history_writes_through_and_reloads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store =
        crate::channels::history_store::ChannelHistoryStore::open(dir.path()).expect("open");
    let histories = HashMap::new();
    let sender = "telegram_u1".to_string();

    let ctx = ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(HashMap::new()),
        provider: Arc::new(DummyProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("system".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(histories)),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
    };

    let ctx = ChannelRuntimeContext {
        history_store: Some(Arc::new(store)),
        ..ctx
    };

    // Enough turns to cross the cap, so eviction is exercised too.
    for idx in 0..(MAX_CHANNEL_HISTORY + 5) {
        history::append_sender_turn(&ctx, &sender, ChatMessage::user(format!("msg-{idx}")));
    }

    // The live map is capped, oldest-first.
    {
        let live = ctx
            .conversation_histories
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let turns = live.get(&sender).expect("live history");
        assert_eq!(turns.len(), MAX_CHANNEL_HISTORY);
        assert_eq!(turns[0].content, "msg-5", "oldest turns are evicted first");
        assert_eq!(
            turns[MAX_CHANNEL_HISTORY - 1].content,
            format!("msg-{}", MAX_CHANNEL_HISTORY + 4)
        );
    }

    // And a fresh reader reproduces it from disk — the whole point.
    let reopened =
        crate::channels::history_store::ChannelHistoryStore::open(dir.path()).expect("reopen");
    let loaded = reopened.load_all().expect("load_all");
    let persisted = loaded.get(&sender).expect("persisted history");
    assert_eq!(
        persisted.len(),
        MAX_CHANNEL_HISTORY,
        "the persisted history must match the live cap"
    );
    assert_eq!(persisted[0].content, "msg-5");
    assert_eq!(
        persisted[MAX_CHANNEL_HISTORY - 1].content,
        format!("msg-{}", MAX_CHANNEL_HISTORY + 4)
    );
}

#[test]
fn compact_sender_history_keeps_recent_truncated_messages() {
    let mut histories = HashMap::new();
    let sender = "telegram_u1".to_string();
    histories.insert(
        sender.clone(),
        (0..20)
            .map(|idx| {
                let content = format!("msg-{idx}-{}", "x".repeat(700));
                if idx % 2 == 0 {
                    ChatMessage::user(content)
                } else {
                    ChatMessage::assistant(content)
                }
            })
            .collect::<Vec<_>>(),
    );

    let ctx = ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(HashMap::new()),
        provider: Arc::new(DummyProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("system".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(histories)),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
    };

    assert!(history::compact_sender_history(&ctx, &sender));

    let histories = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let kept = histories
        .get(&sender)
        .expect("sender history should remain");
    assert_eq!(kept.len(), CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES);
    assert!(kept.iter().all(|turn| {
        let len = turn.content.chars().count();
        len <= CHANNEL_HISTORY_COMPACT_CONTENT_CHARS
            || (len <= CHANNEL_HISTORY_COMPACT_CONTENT_CHARS + 3 && turn.content.ends_with("..."))
    }));
}

struct DummyProvider;

#[async_trait::async_trait]
impl Provider for DummyProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("ok".to_string())
    }
}

#[derive(Default)]
struct RecordingChannel {
    sent_messages: tokio::sync::Mutex<Vec<String>>,
    start_typing_calls: AtomicUsize,
    stop_typing_calls: AtomicUsize,
}

#[derive(Default)]
struct TelegramRecordingChannel {
    sent_messages: tokio::sync::Mutex<Vec<String>>,
    /// Every `apply_allowed_senders` call, in order. A `std::sync::Mutex`
    /// because the trait method is sync.
    applied_allowlists: std::sync::Mutex<Vec<Vec<String>>>,
}

#[async_trait::async_trait]
impl Channel for TelegramRecordingChannel {
    // Mirrors the real channel: the instructions come from the channel
    // impl now, not from a `match` on its name, so a stub that claims the
    // name must also claim the capability.
    fn delivery_instructions(&self) -> Option<&'static str> {
        Some(crate::channels::telegram::TELEGRAM_DELIVERY_INSTRUCTIONS)
    }

    fn name(&self) -> &str {
        "telegram"
    }

    fn apply_allowed_senders(&self, allowed: &[String]) {
        self.applied_allowlists
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(allowed.to_vec());
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.sent_messages
            .lock()
            .await
            .push(format!("{}:{}", message.recipient, message.content));
        Ok(())
    }

    async fn listen(
        &self,
        _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Channel for RecordingChannel {
    fn name(&self) -> &str {
        "test-channel"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        self.sent_messages
            .lock()
            .await
            .push(format!("{}:{}", message.recipient, message.content));
        Ok(())
    }

    async fn listen(
        &self,
        _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        self.start_typing_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        self.stop_typing_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct SlowProvider {
    delay: Duration,
}

#[async_trait::async_trait]
impl Provider for SlowProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        tokio::time::sleep(self.delay).await;
        Ok(format!("echo: {message}"))
    }
}

/// Fails every turn with an error carrying a secret-shaped token, so a test
/// can assert what actually reaches the chat on the LLM-error arm.
struct FailingProvider {
    error: String,
}

#[async_trait::async_trait]
impl Provider for FailingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("{}", self.error))
    }
}

struct ToolCallingProvider;

fn tool_call_payload() -> String {
    r#"<tool_call>
{"name":"mock_price","arguments":{"symbol":"BTC"}}
</tool_call>"#
        .to_string()
}

fn tool_call_payload_with_alias_tag() -> String {
    r#"<toolcall>
{"name":"mock_price","arguments":{"symbol":"BTC"}}
</toolcall>"#
        .to_string()
}

#[async_trait::async_trait]
impl Provider for ToolCallingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(tool_call_payload())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let has_tool_results = messages
            .iter()
            .any(|msg| msg.role == "user" && msg.content.contains("[Tool results]"));
        if has_tool_results {
            Ok("BTC is currently around $65,000 based on latest tool output.".to_string())
        } else {
            Ok(tool_call_payload())
        }
    }
}

struct ToolCallingAliasProvider;

#[async_trait::async_trait]
impl Provider for ToolCallingAliasProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(tool_call_payload_with_alias_tag())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let has_tool_results = messages
            .iter()
            .any(|msg| msg.role == "user" && msg.content.contains("[Tool results]"));
        if has_tool_results {
            Ok("BTC alias-tag flow resolved to final text output.".to_string())
        } else {
            Ok(tool_call_payload_with_alias_tag())
        }
    }
}

struct RawToolArtifactProvider;

#[async_trait::async_trait]
impl Provider for RawToolArtifactProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("fallback".to_string())
    }

    async fn chat_with_history(
        &self,
        _messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(r#"{"name":"mock_price","parameters":{"symbol":"BTC"}}
{"result":{"symbol":"BTC","price_usd":65000}}
BTC is currently around $65,000 based on latest tool output."#
            .to_string())
    }
}

struct IterativeToolProvider {
    required_tool_iterations: usize,
}

impl IterativeToolProvider {
    fn completed_tool_iterations(messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .filter(|msg| msg.role == "user" && msg.content.contains("[Tool results]"))
            .count()
    }
}

#[async_trait::async_trait]
impl Provider for IterativeToolProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(tool_call_payload())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        // A force-summary call (loop-detector or soft-cap) pushes a
        // tools-disabled nudge asking for a plain-language wrap-up. A real
        // model answers it with text, not another tool call — mimic that so
        // the graceful-summary path is exercised end to end.
        let force_summary_requested = messages.iter().any(|msg| {
            msg.role == "user"
                && (msg.content.contains("stuck in a loop")
                    || msg.content.contains("reached the maximum of"))
        });
        if force_summary_requested {
            return Ok("Summary: I kept calling mock_price for BTC and got the \
                           same result, so I stopped. Next step: try a different \
                           symbol or narrow the question."
                .to_string());
        }

        let completed_iterations = Self::completed_tool_iterations(messages);
        if completed_iterations >= self.required_tool_iterations {
            Ok(format!(
                "Completed after {completed_iterations} tool iterations."
            ))
        } else {
            Ok(tool_call_payload())
        }
    }
}

#[derive(Default)]
struct HistoryCaptureProvider {
    calls: std::sync::Mutex<Vec<Vec<(String, String)>>>,
}

#[async_trait::async_trait]
impl Provider for HistoryCaptureProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("fallback".to_string())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let snapshot = messages
            .iter()
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect::<Vec<_>>();
        let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        calls.push(snapshot);
        Ok(format!("response-{}", calls.len()))
    }
}

struct DelayedHistoryCaptureProvider {
    delay: Duration,
    calls: std::sync::Mutex<Vec<Vec<(String, String)>>>,
}

#[async_trait::async_trait]
impl Provider for DelayedHistoryCaptureProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("fallback".to_string())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let snapshot = messages
            .iter()
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect::<Vec<_>>();
        let call_index = {
            let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
            calls.push(snapshot);
            calls.len()
        };
        tokio::time::sleep(self.delay).await;
        Ok(format!("response-{call_index}"))
    }
}

struct MockPriceTool;

#[derive(Default)]
struct ModelCaptureProvider {
    call_count: AtomicUsize,
    models: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for ModelCaptureProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("fallback".to_string())
    }

    async fn chat_with_history(
        &self,
        _messages: &[ChatMessage],
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(model.to_string());
        Ok("ok".to_string())
    }
}

#[async_trait::async_trait]
impl Tool for MockPriceTool {
    fn name(&self) -> &str {
        "mock_price"
    }

    fn description(&self) -> &str {
        "Return a mocked BTC price"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string" }
            },
            "required": ["symbol"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let symbol = args.get("symbol").and_then(serde_json::Value::as_str);
        if symbol != Some("BTC") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("unexpected symbol".to_string()),
            });
        }

        Ok(ToolResult {
            success: true,
            output: r#"{"symbol":"BTC","price_usd":65000}"#.to_string(),
            error: None,
        })
    }
}

#[tokio::test]
async fn process_channel_message_executes_tool_calls_instead_of_sending_raw_json() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(ToolCallingProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 10,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx,
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-42".to_string(),
            content: "What is the BTC price now?".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    let sent_messages = channel_impl.sent_messages.lock().await;
    assert_eq!(sent_messages.len(), 1);
    assert!(sent_messages[0].starts_with("chat-42:"));
    assert!(sent_messages[0].contains("BTC is currently around"));
    assert!(!sent_messages[0].contains("\"tool_calls\""));
    assert!(!sent_messages[0].contains("mock_price"));
}

#[tokio::test]
async fn process_channel_message_strips_unexecuted_tool_json_artifacts_from_reply() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(RawToolArtifactProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 10,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx,
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-raw-json".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-raw".to_string(),
            content: "What is the BTC price now?".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 3,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    let sent_messages = channel_impl.sent_messages.lock().await;
    assert_eq!(sent_messages.len(), 1);
    assert!(sent_messages[0].starts_with("chat-raw:"));
    assert!(sent_messages[0].contains("BTC is currently around"));
    assert!(!sent_messages[0].contains("\"name\":\"mock_price\""));
    assert!(!sent_messages[0].contains("\"result\""));
}

#[tokio::test]
async fn process_channel_message_executes_tool_calls_with_alias_tags() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(ToolCallingAliasProvider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 10,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx,
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-2".to_string(),
            sender: "bob".to_string(),
            reply_target: "chat-84".to_string(),
            content: "What is the BTC price now?".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 2,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    let sent_messages = channel_impl.sent_messages.lock().await;
    assert_eq!(sent_messages.len(), 1);
    assert!(sent_messages[0].starts_with("chat-84:"));
    assert!(sent_messages[0].contains("alias-tag flow resolved"));
    assert!(!sent_messages[0].contains("<toolcall>"));
    assert!(!sent_messages[0].contains("mock_price"));
}

#[tokio::test]
async fn process_channel_message_handles_models_command_without_llm_call() {
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let default_provider_impl = Arc::new(ModelCaptureProvider::default());
    let default_provider: Arc<dyn Provider> = default_provider_impl.clone();
    let fallback_provider_impl = Arc::new(ModelCaptureProvider::default());
    let fallback_provider: Arc<dyn Provider> = fallback_provider_impl.clone();

    let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    provider_cache_seed.insert("test-provider".to_string(), Arc::clone(&default_provider));
    provider_cache_seed.insert("openrouter".to_string(), fallback_provider);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::clone(&default_provider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("default-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx.clone(),
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-cmd-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "/models openrouter".to_string(),
            channel: "telegram".to_string(),
            timestamp: 1,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    let sent = channel_impl.sent_messages.lock().await;
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("Provider switched to `openrouter`"));

    let route_key = conversation::ConversationKey::new("telegram", "chat-1").resolve();
    let route_key = route_key.as_str();
    let route = runtime_ctx
        .route_overrides
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(route_key)
        .cloned()
        .expect("route should be stored for sender");
    assert_eq!(route.provider, "openrouter");
    assert_eq!(route.model, "default-model");

    assert_eq!(default_provider_impl.call_count.load(Ordering::SeqCst), 0);
    assert_eq!(fallback_provider_impl.call_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn process_channel_message_uses_route_override_provider_and_model() {
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let default_provider_impl = Arc::new(ModelCaptureProvider::default());
    let default_provider: Arc<dyn Provider> = default_provider_impl.clone();
    let routed_provider_impl = Arc::new(ModelCaptureProvider::default());
    let routed_provider: Arc<dyn Provider> = routed_provider_impl.clone();

    let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    provider_cache_seed.insert("test-provider".to_string(), Arc::clone(&default_provider));
    provider_cache_seed.insert("openrouter".to_string(), routed_provider);

    let route_key = conversation::ConversationKey::new("telegram", "chat-1").resolve();
    let mut route_overrides = HashMap::new();
    route_overrides.insert(
        route_key,
        ChannelRouteSelection {
            provider: "openrouter".to_string(),
            model: "route-model".to_string(),
        },
    );

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::clone(&default_provider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("default-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
        route_overrides: Arc::new(Mutex::new(route_overrides)),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx,
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-routed-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "hello routed provider".to_string(),
            channel: "telegram".to_string(),
            timestamp: 2,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    assert_eq!(default_provider_impl.call_count.load(Ordering::SeqCst), 0);
    assert_eq!(routed_provider_impl.call_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        routed_provider_impl
            .models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_slice(),
        &["route-model".to_string()]
    );
}

#[tokio::test]
async fn process_channel_message_prefers_cached_default_provider_instance() {
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let startup_provider_impl = Arc::new(ModelCaptureProvider::default());
    let startup_provider: Arc<dyn Provider> = startup_provider_impl.clone();
    let reloaded_provider_impl = Arc::new(ModelCaptureProvider::default());
    let reloaded_provider: Arc<dyn Provider> = reloaded_provider_impl.clone();

    let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    provider_cache_seed.insert("test-provider".to_string(), reloaded_provider);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::clone(&startup_provider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("default-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx,
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-default-provider-cache".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "hello cached default provider".to_string(),
            channel: "telegram".to_string(),
            timestamp: 3,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    assert_eq!(startup_provider_impl.call_count.load(Ordering::SeqCst), 0);
    assert_eq!(reloaded_provider_impl.call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn process_channel_message_uses_runtime_default_model_from_store() {
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let provider_impl = Arc::new(ModelCaptureProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    provider_cache_seed.insert("test-provider".to_string(), Arc::clone(&provider));

    let temp = tempfile::TempDir::new().expect("temp dir");

    // Owned by the context under test, not a process global — no teardown,
    // and no ordering coupling with any other test.
    let seeded_runtime_config = Arc::new(Mutex::new(routing::RuntimeConfigSlot {
        state: Some(routing::RuntimeConfigState {
            defaults: ChannelRuntimeDefaults {
                default_provider: "test-provider".to_string(),
                model: "hot-reloaded-model".to_string(),
                temperature: 0.5,
                api_key: None,
                api_url: None,
                reliability: crate::config::ReliabilityConfig::default(),
                approval_owners: Arc::new(Vec::new()),
                guest_gate: Arc::new(crate::approval::GuestGate::new(
                    Vec::<String>::new(),
                    &[],
                    &[],
                )),
                allowed_commands: Arc::new(Vec::new()),
                autonomy_level: crate::security::AutonomyLevel::Supervised,
                autonomy_preset: crate::approval::policy_writer::PolicyPreset::Manual,
                allowlists: Arc::new(HashMap::new()),
                message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
                max_tool_iterations: 5,
                auto_save_memory: false,
                min_relevance_score: 0.0,
                autonomous_tools: false,
                mention_only: Arc::new(HashMap::new()),
                thread_replies: Arc::new(HashMap::new()),
            },
            last_applied_stamp: None,
            last_reload_error: None,
        }),
        ..routing::RuntimeConfigSlot::default()
    }));

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::clone(&seeded_runtime_config),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::clone(&provider),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("startup-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions {
            rantaiclaw_dir: Some(temp.path().to_path_buf()),
            ..providers::ProviderRuntimeOptions::default()
        },
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx,
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-runtime-store-model".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "hello runtime defaults".to_string(),
            channel: "telegram".to_string(),
            timestamp: 4,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    assert_eq!(provider_impl.call_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider_impl
            .models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_slice(),
        &["hot-reloaded-model".to_string()]
    );
}

#[tokio::test]
async fn maybe_apply_runtime_config_update_hot_reloads_owners_guest_gate_and_allowed_commands() {
    // Owners / guest-gate / autonomy allowed-commands must hot-reload from
    // disk when the config-file stamp changes — no `channels run` restart.
    let temp = tempfile::TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");

    // Build a valid config via round-trip serialization so every required
    // schema field is present; only the three hot-reload-relevant fields
    // vary between the initial and the rewritten config.
    let write_config =
        |owners: Vec<String>, guest_commands: Vec<String>, allowed_commands: Vec<String>| {
            let mut config = crate::config::Config::default();
            config.default_provider = Some("openrouter".to_string());
            config.autonomy.level = crate::security::AutonomyLevel::Supervised;
            config.autonomy.allowed_commands = allowed_commands;
            config.channels_config.approval_owners = owners;
            config.channels_config.guest_allowed_commands = guest_commands;
            let toml = toml::to_string(&config).expect("serialize config");
            std::fs::write(&config_path, toml).expect("write config");
        };

    // Initial config: no owners, no guest commands, baseline allowlist.
    write_config(vec![], vec![], vec!["ls".to_string()]);

    let mut channels_by_name = HashMap::new();
    let channel: Arc<dyn Channel> = Arc::new(TelegramRecordingChannel::default());
    channels_by_name.insert(channel.name().to_string(), channel);

    let ctx = ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(DummyProvider),
        default_provider: Arc::new("openrouter".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("startup-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions {
            rantaiclaw_dir: Some(temp.path().to_path_buf()),
            ..providers::ProviderRuntimeOptions::default()
        },
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    };

    // First apply: seeds the store from the initial config.
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("initial apply");
    let initial = routing::runtime_defaults_snapshot(&ctx);
    assert!(initial.approval_owners.is_empty());
    assert!(!ctx.security.is_command_allowed("brew --version"));

    // Rewrite config with new owners, guest commands, and an extra
    // autonomy allowed-command. Sleep ensures the mtime/len stamp changes.
    std::thread::sleep(std::time::Duration::from_millis(10));
    write_config(
        vec!["rantaiclaw_user".to_string()],
        vec!["echo *".to_string()],
        vec!["ls".to_string(), "brew".to_string()],
    );

    // Second apply: stamp changed -> reload.
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("reload apply");

    let reloaded = routing::runtime_defaults_snapshot(&ctx);
    assert_eq!(
        reloaded.approval_owners.as_slice(),
        &["rantaiclaw_user".to_string()],
        "owners hot-reloaded into the runtime store"
    );
    assert!(
        crate::approval::can_approve(&reloaded.approval_owners, "rantaiclaw_user"),
        "new owner recognized by can_approve via the live snapshot"
    );
    // guest_gate rebuilt: a non-owner may now run the guest command.
    assert!(
        reloaded.guest_gate.command_permitted("echo hi"),
        "guest_gate hot-reloaded the new guest_allowed_command"
    );
    // Security policy picked up the newly-owner-allowed command.
    assert!(
        ctx.security.is_command_allowed("brew --version"),
        "autonomy allowed_commands synced into the live SecurityPolicy"
    );

    // live_approval_owners mirrors the snapshot owners for reply auth.
    assert_eq!(
        routing::live_approval_owners(&ctx).as_slice(),
        &["rantaiclaw_user".to_string()]
    );
}

/// Build a `ChannelRuntimeContext` around one recording Telegram channel,
/// pointed at `dir` for config reloads. Shared by the allowlist tests below.
fn allowlist_test_ctx(
    dir: &std::path::Path,
    channel: Arc<TelegramRecordingChannel>,
) -> ChannelRuntimeContext {
    let mut channels_by_name: HashMap<String, Arc<dyn Channel>> = HashMap::new();
    channels_by_name.insert("telegram".to_string(), channel);
    ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(DummyProvider),
        default_provider: Arc::new("openrouter".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("startup-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions {
            rantaiclaw_dir: Some(dir.to_path_buf()),
            ..providers::ProviderRuntimeOptions::default()
        },
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    }
}

/// The operator-reported bug: saving a Telegram allowlist from the console
/// used to require restarting the whole managed service — which hosts the
/// gateway, so the save killed the request that made it. The runtime holds
/// every live channel handle and already re-reads config per message, so the
/// new list must reach the channel with no restart at all.
#[tokio::test]
async fn allowlist_edit_reaches_the_live_channel_without_restart() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");

    let write_config = |allowed: Vec<String>| {
        let mut config = crate::config::Config::default();
        config.default_provider = Some("openrouter".to_string());
        // Built through serde from a minimal object, matching how the
        // gateway constructs one (`config_api.rs`) — TelegramConfig has no
        // `Default`, and adding one is a production change this plan does
        // not own.
        config.channels_config.telegram = Some(
            serde_json::from_value(serde_json::json!({
                "bot_token": "111:aaaaaaaaaaaaaaaaaaaaaaaaa",
                "allowed_users": allowed,
            }))
            .expect("build TelegramConfig"),
        );
        let toml = toml::to_string(&config).expect("serialize config");
        std::fs::write(&config_path, toml).expect("write config");
    };

    write_config(vec!["user_a".to_string()]);

    let channel = Arc::new(TelegramRecordingChannel::default());
    let ctx = allowlist_test_ctx(temp.path(), Arc::clone(&channel));

    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("initial apply");

    // Stamp is mtime+len, so the rewrite must be distinguishable.
    std::thread::sleep(std::time::Duration::from_millis(10));
    write_config(vec!["user_a".to_string(), "user_b".to_string()]);

    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("reload apply");

    let applied = channel
        .applied_allowlists
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(
        applied.last().map(Vec::as_slice),
        Some(["user_a".to_string(), "user_b".to_string()].as_slice()),
        "the edited allowlist reached the live channel handle"
    );
}

/// An allowlist change is safety-relevant, so it must apply even when the
/// — usually unrelated — new provider cannot be built. Applying it only on
/// the success path would mean a *tightened* allowlist silently waited on an
/// API key, which is the same shape as the autonomy bug fixed earlier.
#[tokio::test]
async fn allowlist_applies_even_when_the_provider_fails_to_build() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");

    let write_config = |allowed: Vec<String>| {
        let mut config = crate::config::Config::default();
        // No API key for a provider that requires one -> build fails.
        config.default_provider = Some("openai".to_string());
        config.api_key = None;
        // Built through serde from a minimal object, matching how the
        // gateway constructs one (`config_api.rs`) — TelegramConfig has no
        // `Default`, and adding one is a production change this plan does
        // not own.
        config.channels_config.telegram = Some(
            serde_json::from_value(serde_json::json!({
                "bot_token": "111:aaaaaaaaaaaaaaaaaaaaaaaaa",
                "allowed_users": allowed,
            }))
            .expect("build TelegramConfig"),
        );
        let toml = toml::to_string(&config).expect("serialize config");
        std::fs::write(&config_path, toml).expect("write config");
    };

    write_config(vec!["user_a".to_string(), "user_b".to_string()]);

    let channel = Arc::new(TelegramRecordingChannel::default());
    let ctx = allowlist_test_ctx(temp.path(), Arc::clone(&channel));

    // The reload must not fail even though the provider build does.
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("apply despite provider build failure");

    std::thread::sleep(std::time::Duration::from_millis(10));
    // Tighten: remove user_b. This is the direction that must never stall.
    write_config(vec!["user_a".to_string()]);

    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("reload despite provider build failure");

    let applied = channel
        .applied_allowlists
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(
        applied.last().map(Vec::as_slice),
        Some(["user_a".to_string()].as_slice()),
        "tightened allowlist applied even though the provider could not be built"
    );
}

/// Write a config whose provider cannot be built (no key for a provider that
/// requires one), carrying the owner list, guest allowances, temperature and
/// `autonomous_tools` under test. Everything here is on the *security* half
/// of the config, which the failure branch used to drop.
fn write_broken_provider_config(
    config_path: &std::path::Path,
    owners: Vec<String>,
    guest_tools: Vec<String>,
    temperature: f64,
    autonomous_tools: bool,
) {
    let mut config = crate::config::Config::default();
    config.default_provider = Some("openai".to_string());
    config.api_key = None;
    config.default_temperature = temperature;
    config.channels_config.approval_owners = owners;
    config.channels_config.guest_allowed_tools = guest_tools;
    config.channels_config.autonomous_tools = autonomous_tools;
    let toml = toml::to_string(&config).expect("serialize config");
    std::fs::write(config_path, toml).expect("write config");
}

/// The failure branch used to carry forward exactly three fields, so an
/// operator removing a compromised owner in the same edit that left the
/// provider unbuildable got the removal persisted to disk and never applied
/// — and the stamp advanced, so nothing retried it.
#[tokio::test]
async fn owner_removal_applies_even_when_the_provider_fails_to_build() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");
    let channel = Arc::new(TelegramRecordingChannel::default());
    let ctx = allowlist_test_ctx(temp.path(), Arc::clone(&channel));

    write_broken_provider_config(
        &config_path,
        vec![
            "rantaiclaw_operator".to_string(),
            "revoked_user".to_string(),
        ],
        Vec::new(),
        0.0,
        false,
    );
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("apply despite provider build failure");
    assert_eq!(
        routing::live_approval_owners(&ctx).len(),
        2,
        "both owners applied"
    );

    std::thread::sleep(std::time::Duration::from_millis(10));
    write_broken_provider_config(
        &config_path,
        vec!["rantaiclaw_operator".to_string()],
        Vec::new(),
        0.0,
        false,
    );
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("reload despite provider build failure");

    assert_eq!(
        routing::live_approval_owners(&ctx).as_slice(),
        &["rantaiclaw_operator".to_string()],
        "revoking an owner must apply even when the provider cannot be built"
    );
}

/// Same shape for the guest ceiling: it decides what a non-owner may run, so
/// a tightened guest list stalling behind a broken API key is a live gate
/// staying wider than the operator asked for.
#[tokio::test]
async fn guest_gate_applies_when_the_provider_fails_to_build() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");
    let channel = Arc::new(TelegramRecordingChannel::default());
    let ctx = allowlist_test_ctx(temp.path(), Arc::clone(&channel));

    write_broken_provider_config(
        &config_path,
        Vec::new(),
        vec!["web_search".to_string(), "shell".to_string()],
        0.0,
        false,
    );
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("apply despite provider build failure");
    let wide = routing::runtime_defaults_snapshot(&ctx).guest_gate;
    assert!(
        wide.tool_permitted("shell"),
        "seeded with the wider ceiling"
    );

    std::thread::sleep(std::time::Duration::from_millis(10));
    write_broken_provider_config(
        &config_path,
        Vec::new(),
        vec!["web_search".to_string()],
        0.0,
        false,
    );
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("reload despite provider build failure");

    let tightened = routing::runtime_defaults_snapshot(&ctx).guest_gate;
    assert!(
        !tightened.tool_permitted("shell"),
        "tightening the guest ceiling must apply even when the provider cannot be built"
    );
}

/// Regression guard for the inversion itself. `temperature` is not on the
/// exclusion list, so it must survive a provider-build failure. Under the old
/// include-list shape it did not — and neither would any field added later.
#[tokio::test]
async fn a_non_excluded_defaults_field_is_applied_when_the_provider_fails() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");
    let channel = Arc::new(TelegramRecordingChannel::default());
    let ctx = allowlist_test_ctx(temp.path(), Arc::clone(&channel));

    write_broken_provider_config(&config_path, Vec::new(), Vec::new(), 0.25, false);
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("apply despite provider build failure");

    std::thread::sleep(std::time::Duration::from_millis(10));
    write_broken_provider_config(&config_path, Vec::new(), Vec::new(), 0.75, false);
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("reload despite provider build failure");

    let applied = routing::runtime_defaults_snapshot(&ctx);
    assert!(
        (applied.temperature - 0.75).abs() < f64::EPSILON,
        "a field absent from the exclusion list must apply, got {}",
        applied.temperature
    );
    assert_eq!(
        applied.default_provider, "openrouter",
        "the excluded provider field must still be held back"
    );
}

/// `autonomous_tools = false` re-arms the in-chat approval gate. It was read
/// once at startup, so an operator turning the gate back **on** was told
/// "Applied updated channel runtime config from disk" and got nothing.
#[tokio::test]
async fn autonomous_tools_change_is_applied_on_reload() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");
    let channel = Arc::new(TelegramRecordingChannel::default());
    let ctx = allowlist_test_ctx(temp.path(), Arc::clone(&channel));

    write_broken_provider_config(&config_path, Vec::new(), Vec::new(), 0.0, true);
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("apply despite provider build failure");
    assert!(
        routing::runtime_defaults_snapshot(&ctx).autonomous_tools,
        "opted out of gating"
    );

    std::thread::sleep(std::time::Duration::from_millis(10));
    write_broken_provider_config(&config_path, Vec::new(), Vec::new(), 0.0, false);
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("reload despite provider build failure");

    assert!(
        !routing::runtime_defaults_snapshot(&ctx).autonomous_tools,
        "re-arming the approval gate must not need a restart"
    );
}

/// An unstattable config used to return `Ok(())` with nothing logged, which
/// is indistinguishable from "nothing to do". The stamp must NOT advance, so
/// the edit still applies once the file is readable again — the atomic
/// temp-file-and-rename write makes a briefly-absent config real.
#[tokio::test]
async fn unreadable_config_warns_once_and_does_not_advance_the_stamp() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");
    let channel = Arc::new(TelegramRecordingChannel::default());
    let ctx = allowlist_test_ctx(temp.path(), Arc::clone(&channel));

    // No config file on disk yet.
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("an unreadable config is not an error for the caller");
    {
        let slot = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
        assert!(slot.stamp_error_warned, "the failure was reported");
        assert!(
            slot.state.is_none(),
            "nothing was applied and no stamp was recorded"
        );
    }

    // The file appears: the edit must still land.
    write_broken_provider_config(
        &config_path,
        vec!["rantaiclaw_operator".to_string()],
        Vec::new(),
        0.0,
        false,
    );
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("apply once the config is readable");

    assert_eq!(
        routing::live_approval_owners(&ctx).as_slice(),
        &["rantaiclaw_operator".to_string()],
        "the stamp must not have been advanced past an unread config"
    );
    let slot = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        !slot.stamp_error_warned,
        "the latch re-arms so a later outage is reported again"
    );
}

/// The synthesised fallback hands the model a *guessed* autonomy preset that
/// the live gate is not enforcing. It must not be silent.
#[tokio::test]
async fn snapshot_fallback_warns_once() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let channel = Arc::new(TelegramRecordingChannel::default());
    let ctx = allowlist_test_ctx(temp.path(), Arc::clone(&channel));

    assert!(
        !ctx.runtime_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fallback_warned,
        "nothing consulted the snapshot yet"
    );

    let _ = routing::runtime_defaults_snapshot(&ctx);
    assert!(
        ctx.runtime_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fallback_warned,
        "taking the synthesised fallback must be reported"
    );

    // Still exactly one report after repeated per-message consultation.
    let _ = routing::runtime_defaults_snapshot(&ctx);
    assert!(
        ctx.runtime_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fallback_warned
    );
}

/// Two runtimes in one process used to share a global map keyed by config
/// path, so they clobbered each other's applied state. Each context now owns
/// its own.
#[tokio::test]
async fn two_runtimes_do_not_share_applied_config_state() {
    let temp_a = tempfile::TempDir::new().expect("temp dir");
    let temp_b = tempfile::TempDir::new().expect("temp dir");
    let ctx_a = allowlist_test_ctx(temp_a.path(), Arc::new(TelegramRecordingChannel::default()));
    let ctx_b = allowlist_test_ctx(temp_b.path(), Arc::new(TelegramRecordingChannel::default()));

    write_broken_provider_config(
        &temp_a.path().join("config.toml"),
        vec!["owner_a".to_string()],
        Vec::new(),
        0.0,
        false,
    );
    routing::maybe_apply_runtime_config_update(&ctx_a)
        .await
        .expect("apply for runtime A");

    assert_eq!(
        routing::live_approval_owners(&ctx_a).as_slice(),
        &["owner_a".to_string()]
    );
    assert!(
        routing::live_approval_owners(&ctx_b).is_empty(),
        "runtime B must not see runtime A's applied state"
    );
}

/// A config reload whose NEW provider can't be built must still apply the
/// non-provider settings. A safety autonomy downgrade bundled with a broken
/// provider previously never took effect — the reload returned early on the
/// keep-old-provider branch before touching the SecurityPolicy.
#[tokio::test]
async fn maybe_apply_runtime_config_update_applies_autonomy_when_provider_build_fails() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");

    let write_config =
        |provider: &str, level: crate::security::AutonomyLevel, forbidden: Vec<String>| {
            let mut config = crate::config::Config::default();
            config.default_provider = Some(provider.to_string());
            config.autonomy.level = level;
            config.autonomy.workspace_only = false;
            if !forbidden.is_empty() {
                config.autonomy.forbidden_paths = forbidden;
            }
            let toml = toml::to_string(&config).expect("serialize config");
            std::fs::write(&config_path, toml).expect("write config");
        };

    // Initial: a buildable provider at Full autonomy.
    write_config("openrouter", crate::security::AutonomyLevel::Full, vec![]);

    let mut channels_by_name = HashMap::new();
    let channel: Arc<dyn Channel> = Arc::new(TelegramRecordingChannel::default());
    channels_by_name.insert(channel.name().to_string(), channel);

    let ctx = ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(DummyProvider),
        default_provider: Arc::new("openrouter".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("startup-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions {
            rantaiclaw_dir: Some(temp.path().to_path_buf()),
            ..providers::ProviderRuntimeOptions::default()
        },
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    };

    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("initial apply");
    assert_eq!(
        ctx.security.effective_autonomy(),
        crate::security::AutonomyLevel::Full
    );

    // Rewrite: an UNKNOWN provider (build fails) + a safety downgrade to ReadOnly.
    std::thread::sleep(std::time::Duration::from_millis(10));
    write_config(
        "totally-unknown-provider-xyz",
        crate::security::AutonomyLevel::ReadOnly,
        vec![],
    );

    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("reload returns Ok even when the new provider build fails");

    // We took the keep-old-provider branch (the failure is recorded)...
    assert!(
        routing::last_reload_error(&ctx).is_some(),
        "a failed provider build should be recorded"
    );
    // ...yet the safety autonomy downgrade STILL took effect.
    assert_eq!(
        ctx.security.effective_autonomy(),
        crate::security::AutonomyLevel::ReadOnly,
        "autonomy downgrade must apply even when the new provider can't be built"
    );
    // ...and so did the preset the system prompt is rendered from. The gate
    // and the briefing have to move together: `or_insert_with` on this path
    // leaves an existing store entry untouched, so without carrying the
    // autonomy-derived defaults forward the prompt would keep describing
    // the pre-reload preset while the gate enforced ReadOnly.
    assert_eq!(
        routing::runtime_defaults_snapshot(&ctx).autonomy_preset,
        crate::approval::policy_writer::PolicyPreset::Strict,
        "the prompt preset must follow a downgrade even when the provider build fails"
    );

    // A reload must refresh the WHOLE `[autonomy]` section, not just the two
    // fields the old override slots carried. `forbidden_paths` was frozen at
    // whatever was on disk when the daemon started, so this phase fails on
    // pre-fix code. Autonomy is back at Full so the assertion cannot pass
    // vacuously through the read-only short-circuit.
    std::thread::sleep(std::time::Duration::from_millis(10));
    write_config(
        "openrouter",
        crate::security::AutonomyLevel::Full,
        vec!["/newly-forbidden".to_string()],
    );
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("third apply");
    assert_eq!(
        ctx.security.effective_autonomy(),
        crate::security::AutonomyLevel::Full
    );
    assert!(
        !ctx.security.is_path_allowed("/newly-forbidden/secret.txt"),
        "a forbidden path added by a reload must be enforced without a restart"
    );
}

#[tokio::test]
async fn maybe_apply_runtime_config_update_clears_pinned_sender_on_provider_switch() {
    // A per-sender override (set via /model in-channel) pins that sender to a
    // provider. A Web-UI provider switch rewrites config.toml; the reload must
    // CLEAR the override so the pinned sender follows the new default —
    // otherwise the switch never reaches them (the reported bug).
    let temp = tempfile::TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");

    let write_config = |provider: &str, model: &str| {
        let mut config = crate::config::Config::default();
        config.default_provider = Some(provider.to_string());
        config.default_model = Some(model.to_string());
        let toml = toml::to_string(&config).expect("serialize config");
        std::fs::write(&config_path, toml).expect("write config");
    };

    write_config("openrouter", "model-a");

    let mut channels_by_name = HashMap::new();
    let channel: Arc<dyn Channel> = Arc::new(TelegramRecordingChannel::default());
    channels_by_name.insert(channel.name().to_string(), channel);

    let ctx = ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(DummyProvider),
        default_provider: Arc::new("openrouter".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("model-a".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions {
            rantaiclaw_dir: Some(temp.path().to_path_buf()),
            ..providers::ProviderRuntimeOptions::default()
        },
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    };

    // Seed the store from the initial config.
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("initial apply");

    // A sender pins their own provider/model in-channel — differs from the
    // default, so it is stored as an override.
    routing::set_route_selection(
        &ctx,
        "sender-1",
        ChannelRouteSelection {
            provider: "groq".to_string(),
            model: "model-b".to_string(),
        },
    );
    assert_eq!(
        routing::get_route_selection(&ctx, "sender-1").provider,
        "groq",
        "override is active before the switch"
    );

    // Operator switches provider in the Web UI → config.toml changes.
    std::thread::sleep(std::time::Duration::from_millis(10));
    write_config("deepseek", "model-c");
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("reload apply");

    // The pinned sender must now follow the new default (override cleared).
    let route = routing::get_route_selection(&ctx, "sender-1");
    assert_eq!(
        route.provider, "deepseek",
        "pinned sender re-based to the new provider after a Web-UI switch"
    );
    assert_eq!(
        route.model, "model-c",
        "pinned sender re-based to the new model"
    );
}

#[tokio::test]
async fn maybe_apply_runtime_config_update_keeps_provider_and_records_reason_on_build_failure() {
    // Switching to a provider that can't be built (unknown / no usable key)
    // must NOT swap the channel onto a broken provider: keep the working one,
    // advance the stamp (no per-message retry loop), and record the reason.
    let temp = tempfile::TempDir::new().expect("temp dir");
    let config_path = temp.path().join("config.toml");

    let write_config = |provider: &str, model: &str| {
        let mut config = crate::config::Config::default();
        config.default_provider = Some(provider.to_string());
        config.default_model = Some(model.to_string());
        let toml = toml::to_string(&config).expect("serialize config");
        std::fs::write(&config_path, toml).expect("write config");
    };

    write_config("openrouter", "model-a");

    let mut channels_by_name = HashMap::new();
    let channel: Arc<dyn Channel> = Arc::new(TelegramRecordingChannel::default());
    channels_by_name.insert(channel.name().to_string(), channel);

    let ctx = ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(DummyProvider),
        default_provider: Arc::new("openrouter".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("model-a".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions {
            rantaiclaw_dir: Some(temp.path().to_path_buf()),
            ..providers::ProviderRuntimeOptions::default()
        },
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    };

    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("initial apply");
    assert_eq!(
        routing::runtime_defaults_snapshot(&ctx).default_provider,
        "openrouter"
    );

    // Switch to a provider that cannot be built.
    std::thread::sleep(std::time::Duration::from_millis(10));
    write_config("nonexistent-provider-xyz", "whatever");
    // Must return Ok (kept the old provider), not propagate the build error.
    routing::maybe_apply_runtime_config_update(&ctx)
        .await
        .expect("reload keeps old provider on failure");

    assert_eq!(
        routing::runtime_defaults_snapshot(&ctx).default_provider,
        "openrouter",
        "kept the working provider instead of swapping to a broken one"
    );
    assert!(
        routing::last_reload_error(&ctx).is_some(),
        "recorded the reload failure reason for surfacing"
    );
}

// When a provider+tool return the *same* call and result on every
// iteration, the loop-detector (`agent/loop_.rs`, 3 identical repeats)
// intentionally breaks early and force-summarizes with tools disabled,
// rather than running all the way to `max_tool_iterations`. The reply is a
// graceful wrap-up, not "Completed after N tool iterations." and not an
// error. (This asserts the current design, reconciled in plan 017.)
#[tokio::test]
async fn process_channel_message_respects_configured_max_tool_iterations_above_default() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(IterativeToolProvider {
            required_tool_iterations: 11,
        }),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 12,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx,
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-iter-success".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-iter-success".to_string(),
            content: "Loop until done".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    let sent_messages = channel_impl.sent_messages.lock().await;
    assert_eq!(sent_messages.len(), 1);
    assert!(sent_messages[0].starts_with("chat-iter-success:"));
    // Graceful force-summary reply — NOT the old "loop to N then complete"
    // behavior, and NOT a hard error.
    assert!(sent_messages[0].contains("Summary:"));
    assert!(!sent_messages[0].contains("Completed after 11 tool iterations."));
    assert!(!sent_messages[0].contains("⚠️ Error:"));
}

// Exhausting the tool-call budget is no longer a hard, user-facing error.
// The soft-cap (`agent/loop_.rs`) — or the loop-detector, whichever trips
// first for this identical-result fixture — force-summarizes with tools
// disabled so the user gets a best-effort wrap-up (mentioning `/continue`)
// instead of "⚠️ Error: Agent exceeded maximum tool iterations". (Current
// design, reconciled in plan 017.)
#[tokio::test]
async fn process_channel_message_reports_configured_max_tool_iterations_limit() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(IterativeToolProvider {
            required_tool_iterations: 20,
        }),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![Box::new(MockPriceTool)]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 3,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx,
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-iter-fail".to_string(),
            sender: "bob".to_string(),
            reply_target: "chat-iter-fail".to_string(),
            content: "Loop forever".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 2,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    let sent_messages = channel_impl.sent_messages.lock().await;
    assert_eq!(sent_messages.len(), 1);
    assert!(sent_messages[0].starts_with("chat-iter-fail:"));
    // Graceful force-summary reply — NOT the old hard "exceeded maximum
    // tool iterations" error.
    assert!(sent_messages[0].contains("Summary:"));
    assert!(!sent_messages[0].contains("⚠️ Error:"));
}

struct NoopMemory;

#[async_trait::async_trait]
impl Memory for NoopMemory {
    fn name(&self) -> &str {
        "noop"
    }

    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: crate::memory::MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _category: Option<&crate::memory::MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(0)
    }

    async fn health_check(&self) -> bool {
        true
    }
}

struct RecallMemory;

#[async_trait::async_trait]
impl Memory for RecallMemory {
    fn name(&self) -> &str {
        "recall-memory"
    }

    async fn store(
        &self,
        _key: &str,
        _content: &str,
        _category: crate::memory::MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn recall(
        &self,
        _query: &str,
        _limit: usize,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        Ok(vec![crate::memory::MemoryEntry {
            id: "entry-1".to_string(),
            key: "memory_key_1".to_string(),
            content: "Age is 45".to_string(),
            category: crate::memory::MemoryCategory::Core,
            timestamp: "2026-02-20T00:00:00Z".to_string(),
            session_id: None,
            score: Some(0.9),
        }])
    }

    async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
        Ok(None)
    }

    async fn list(
        &self,
        _category: Option<&crate::memory::MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
        Ok(Vec::new())
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(1)
    }

    async fn health_check(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn completion_guard_releases_waiters_on_panic() {
    // The bug: `mark_done()` was the last statement of the worker closure,
    // so a panic anywhere in the message path skipped it. The next message
    // from that sender then waited on a signal that would never come, and
    // after enough of those the dispatch loop stopped draining for EVERY
    // channel with nothing logging a deadlock.
    let completion = Arc::new(supervisor::InFlightTaskCompletion::new());

    let guarded = Arc::clone(&completion);
    let handle = tokio::spawn(async move {
        let _guard = supervisor::CompletionGuard(Arc::clone(&guarded));
        panic!("worker panicked mid-turn");
    });
    assert!(handle.await.is_err(), "the worker task must have panicked");

    // Bounded: a regression here hangs rather than fails, and a hanging test
    // in CI is worse than the bug.
    tokio::time::timeout(std::time::Duration::from_secs(5), completion.wait())
        .await
        .expect("waiters must be released even when the worker panics");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_wait_survives_a_concurrent_mark_done() {
    // `notify_waiters()` stores no permit and `notified()` does not register
    // until first polled, so checking `done` and *then* awaiting left a
    // window where a concurrent `mark_done()` was lost.
    //
    // NOTE: this is a stress test, not a proof. The window is two adjacent
    // statements, so it does not reproduce deterministically; it is here to
    // catch a regression under load. The fix itself rests on Tokio's
    // documented `Notified::enable()` semantics.
    for _ in 0..500 {
        let completion = Arc::new(supervisor::InFlightTaskCompletion::new());
        let marker = Arc::clone(&completion);
        let waiter = Arc::clone(&completion);

        let m = tokio::spawn(async move { marker.mark_done() });
        let w = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), waiter.wait()).await
        });

        m.await.expect("marker task");
        w.await
            .expect("waiter task")
            .expect("wait must not hang when mark_done races it");
    }
}

#[test]
fn json_candidate_mid_line_is_rejected_before_parsing() {
    // The cheap guard that makes the stripper linear: a `{` with prose to its
    // left cannot be line-isolated, so it must be rejected without paying for
    // a parse of the entire remaining message.
    let msg = "the shape is {\"a\": 1} inline\n{\"b\": 2}\n";
    let inline = msg.find('{').expect("inline brace");
    assert!(
        !sanitize::json_candidate_starts_its_line(msg, inline),
        "a brace preceded by prose must be rejected before parsing"
    );

    let isolated = msg[inline + 1..].find('{').expect("isolated brace") + inline + 1;
    assert!(
        sanitize::json_candidate_starts_its_line(msg, isolated),
        "a brace that starts its line must still be considered"
    );
}

#[tokio::test]
async fn channel_error_replies_are_sanitized_before_delivery() {
    // The LLM-error arm delivered its raw error chain verbatim to an
    // arbitrary sender, including a guest, while the sibling failure path
    // sanitized. This drives the real dispatch loop rather than calling the
    // sanitizer directly, so reverting the call site fails the test.
    //
    // Scope note: `sanitize_api_error` scrubs secret-shaped tokens and caps
    // length. It does NOT strip local filesystem paths — the sibling path
    // does not either. Path leakage is a separate pre-existing defect on
    // both arms; this test pins only what this change delivers.
    let secret = "sk-not-a-real-key";
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(FailingProvider {
            error: format!("upstream rejected credential {secret}"),
        }),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 10,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    tx.send(traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "1".to_string(),
        sender: "guest_user".to_string(),
        reply_target: "guest_user".to_string(),
        content: "hello".to_string(),
        channel: "test-channel".to_string(),
        timestamp: 1,
        thread_ts: None,
    })
    .await
    .unwrap();
    drop(tx);

    run_message_dispatch_loop(rx, runtime_ctx, 1).await;

    let sent_messages = channel_impl.sent_messages.lock().await;
    assert_eq!(sent_messages.len(), 1, "the error arm must still reply");
    assert!(
        !sent_messages[0].contains(secret),
        "a secret-shaped token must not reach a chat reply: {}",
        sent_messages[0]
    );
    assert!(
        sent_messages[0].contains("[REDACTED]"),
        "the reply must carry the scrubbed marker: {}",
        sent_messages[0]
    );
}

/// The reported leak, at the runtime level: the same person messaging the bot
/// privately and in a group used to share one history, so private turns were
/// injected verbatim into the prompt for the public chat — and persisted, so
/// it survived restarts.
/// `channel add` configures nothing; it points at `onboard`. That is an
/// informational outcome, and it used to `bail!` — so any script wrapping
/// `rantaiclaw channel add` saw a non-zero exit and treated it as a failure.
#[tokio::test]
async fn channel_add_reports_guidance_without_failing() {
    let config = Config::default();
    let result = admin::handle_command(
        crate::ChannelCommands::Add {
            channel_type: "telegram".to_string(),
        },
        &config,
    )
    .await;

    assert!(result.is_ok(), "guidance must not be reported as a failure");
    let text = admin::channel_add_guidance("telegram");
    assert!(text.contains("telegram"), "names the requested type");
    assert!(text.contains("onboard"), "points at the command that works");
}

/// Same for `channel remove`.
#[tokio::test]
async fn channel_remove_reports_guidance_without_failing() {
    let config = Config::default();
    let result = admin::handle_command(
        crate::ChannelCommands::Remove {
            name: "telegram".to_string(),
        },
        &config,
    )
    .await;

    assert!(result.is_ok(), "guidance must not be reported as a failure");
    let text = admin::channel_remove_guidance("telegram");
    assert!(text.contains("telegram"));
    assert!(text.contains("config.toml"), "says where to make the edit");
}

/// Populate every channel section, so the factory has to build all of them.
///
/// Built through serde from minimal objects, matching how the gateway
/// constructs config (`config_api.rs`); several channel config structs have
/// no `Default`, and adding one is a production change this test does not own.
fn config_with_every_channel() -> Config {
    use serde_json::json;
    let mut config = Config::default();
    let c = &mut config.channels_config;
    c.telegram = serde_json::from_value(json!({
        "bot_token": "111:aaaaaaaaaaaaaaaaaaaaaaaaa", "allowed_users": []
    }))
    .expect("telegram");
    c.discord =
        serde_json::from_value(json!({"bot_token": "t", "allowed_users": []})).expect("discord");
    c.slack = serde_json::from_value(json!({
        "bot_token": "t", "app_token": "a", "channel_id": "C1", "allowed_users": []
    }))
    .expect("slack");
    c.mattermost = serde_json::from_value(json!({
        "url": "https://example.com", "bot_token": "t", "channel_id": "C1", "allowed_users": []
    }))
    .expect("mattermost");
    c.webhook = serde_json::from_value(json!({"port": 8080})).expect("webhook");
    c.imessage = serde_json::from_value(json!({"allowed_contacts": []})).expect("imessage");
    c.matrix = serde_json::from_value(json!({
        "homeserver": "https://example.org", "access_token": "t",
        "room_id": "!r:example.org", "allowed_users": []
    }))
    .expect("matrix");
    c.signal = serde_json::from_value(json!({
        "http_url": "http://localhost:8080", "account": "+15550000000",
        "allowed_from": [], "ignore_attachments": false, "ignore_stories": true
    }))
    .expect("signal");
    c.whatsapp = serde_json::from_value(json!({
        "mode": "cloud", "phone_number_id": "1", "access_token": "t",
        "verify_token": "v", "allowed_numbers": []
    }))
    .expect("whatsapp");
    c.linq = serde_json::from_value(json!({
        "api_token": "k", "from_phone": "+15550000001", "allowed_senders": []
    }))
    .expect("linq");
    c.nextcloud_talk = serde_json::from_value(json!({
        "base_url": "https://example.com", "app_token": "t", "allowed_users": []
    }))
    .expect("nextcloud_talk");
    c.email = serde_json::from_value(json!({
        "imap_host": "imap.example.com", "imap_port": 993, "imap_folder": "INBOX",
        "smtp_host": "smtp.example.com", "smtp_port": 587, "smtp_tls": true,
        "username": "u", "password": "p", "from_address": "bot@example.com",
        "idle_timeout_secs": 60
    }))
    .expect("email");
    c.irc = serde_json::from_value(json!({
        "server": "irc.example.org", "port": 6697, "nickname": "bot",
        "channels": ["#c"], "allowed_users": []
    }))
    .expect("irc");
    c.lark = serde_json::from_value(json!({
        "app_id": "a", "app_secret": "s", "allowed_users": [],
        "use_feishu": false, "receive_mode": "webhook"
    }))
    .expect("lark");
    c.dingtalk = serde_json::from_value(json!({
        "client_id": "a", "client_secret": "s", "allowed_users": []
    }))
    .expect("dingtalk");
    c.qq = serde_json::from_value(json!({
        "app_id": "a", "app_secret": "s", "allowed_users": []
    }))
    .expect("qq");
    config
}

/// The test that would have caught the reported defect: `channels doctor`
/// had no Mattermost branch, so an operator whose Mattermost bot token had
/// expired was told everything was healthy while that channel silently never
/// answered. `MattermostChannel::health_check` had no live caller at all.
///
/// The doctor and the runtime now build from one factory, so this asserts the
/// factory covers every catalog entry that is a real `Channel`.
#[test]
fn every_configured_channel_is_built_by_the_factory() {
    let config = config_with_every_channel();
    let built: Vec<&str> = factory::build_configured_channels(&config)
        .into_iter()
        .map(|(key, _, _)| key)
        .collect();

    for (key, display) in CHANNEL_CATALOG {
        if NON_CHANNEL_CATALOG_KEYS.contains(&key) {
            continue;
        }
        // Feature-gated channels are absent from a build that cannot run them.
        if !channel_is_configured(key, &config) {
            continue;
        }
        assert!(
            built.contains(&key),
            "{display} is configured but the factory does not build it — \
                 this is the drift that lost Mattermost from `channels doctor`"
        );
    }
}

/// The roster is what `channel list` and `status` report. It used to be a
/// separate hand-maintained list documented as the single source of truth,
/// and it disagreed with what was actually constructed.
#[test]
fn roster_covers_exactly_the_catalog() {
    let config = config_with_every_channel();
    let roster = admin::channel_roster(&config);

    assert_eq!(roster.len(), CHANNEL_CATALOG.len());
    for ((key, display), (roster_display, configured)) in CHANNEL_CATALOG.iter().zip(&roster) {
        assert_eq!(display, roster_display, "roster order follows the catalog");
        assert_eq!(
            *configured,
            channel_is_configured(key, &config),
            "{display} disagrees with the catalog's own predicate"
        );
    }
}

/// `GET /api/v1/channels` hand-rolled its own list of `if`s and checked 7 of
/// the 11 channels that existed at the time — matrix, linq, irc and lark were
/// never added, so an operator polling the API was told those channels were
/// not configured when they were. The endpoint derives from the catalog now;
/// this is what fails if it ever goes back to a hand-written list.
///
/// The catalog is the oracle, not a second copy of the implementation's own
/// filter expression — comparing the endpoint against a re-derivation of
/// itself would pass no matter which channels either one missed.
#[test]
fn the_api_channel_list_covers_every_catalog_channel() {
    let config = config_with_every_channel();
    let api = crate::gateway::api_v1::configured_channel_keys(&config);

    for (key, display) in CHANNEL_CATALOG {
        // Feature-gated channels are absent from a build that cannot run them.
        if !channel_is_configured(key, &config) {
            continue;
        }
        assert!(
            api.contains(&key),
            "{display} is configured but `/api/v1/channels` omits it — this is \
             the drift that left the endpoint reporting 7 of 11 channels"
        );
    }
}

/// The completeness test above is satisfied by an endpoint that returns the
/// whole catalog unconditionally. This is the half that makes it mean
/// something: nothing configured, nothing reported.
#[test]
fn the_api_channel_list_reports_nothing_when_nothing_is_configured() {
    let api = crate::gateway::api_v1::configured_channel_keys(&Config::default());
    assert!(
        api.is_empty(),
        "an unconfigured install must report no channels, got: {api:?}"
    );
}

/// Feature-gated channels must be reported as unconfigured in a build that
/// cannot run them, never silently dropped.
#[test]
fn feature_gated_channels_follow_the_build() {
    let config = config_with_every_channel();
    let built: Vec<&str> = factory::build_configured_channels(&config)
        .into_iter()
        .map(|(key, _, _)| key)
        .collect();

    assert_eq!(
        built.contains(&"matrix"),
        cfg!(feature = "channel-matrix"),
        "matrix must be built exactly when its feature is on"
    );
    assert_eq!(
        built.contains(&"lark"),
        cfg!(feature = "channel-lark"),
        "lark must be built exactly when its feature is on"
    );
    assert_eq!(
        channel_is_configured("lark", &config),
        cfg!(feature = "channel-lark"),
        "the roster agrees with the factory about the build"
    );
}

/// The "keep in sync" comment on `channel_supports_announce_delivery` asks
/// for an invariant nothing enforced. The factory can build fifteen channels;
/// cron delivers to four. That gap is deliberate — widening delivery is a
/// capability change, not a refactor — so this pins the advertised set
/// instead of letting it drift silently in either direction.
#[test]
fn announce_delivery_advertises_a_subset_of_what_the_factory_builds() {
    let config = config_with_every_channel();
    let built: Vec<&str> = factory::build_configured_channels(&config)
        .into_iter()
        .map(|(key, _, _)| key)
        .collect();

    let advertised: Vec<&str> = CHANNEL_CATALOG
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| channel_supports_announce_delivery(key))
        .collect();

    assert_eq!(
        advertised,
        vec!["telegram", "discord", "slack", "mattermost"],
        "cron delivery set changed — this is a capability change, not a refactor"
    );
    for key in &advertised {
        assert!(
            built.contains(key),
            "{key} is advertised for cron delivery but the factory cannot build it"
        );
    }
}

#[test]
fn dm_and_group_history_do_not_merge() {
    let dm = conversation_history_key(&traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "1".into(),
        sender: "alice".into(),
        reply_target: "private-chat".into(),
        content: "secret".into(),
        channel: "telegram".into(),
        timestamp: 1,
        thread_ts: None,
    });
    let group = conversation_history_key(&traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "2".into(),
        sender: "alice".into(),
        reply_target: "-1009999".into(),
        content: "public".into(),
        channel: "telegram".into(),
        timestamp: 2,
        thread_ts: None,
    });

    assert_ne!(
        dm, group,
        "one person's DM and a group they share with the bot are not one conversation"
    );
}

/// A forum topic / platform thread is its own conversation, so replies in one
/// topic do not carry another topic's turns.
#[test]
fn threads_resolve_to_their_own_conversation() {
    let base = traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "1".into(),
        sender: "alice".into(),
        reply_target: "chan99".into(),
        content: "hi".into(),
        channel: "discord".into(),
        timestamp: 1,
        thread_ts: None,
    };
    let parent = conversation_history_key(&base);
    let threaded = conversation_history_key(&traits::ChannelMessage {
        thread_ts: Some("thread42".into()),
        ..base
    });

    assert_ne!(parent, threaded);
}

/// History and `/model` routing must share one key. Leaving them different
/// reintroduces the same class in a quieter form: a pin set in one chat
/// following the person into every other chat they are in.
#[test]
fn route_override_key_follows_the_conversation_not_the_person() {
    let chat_a = conversation_history_key(&traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "1".into(),
        sender: "alice".into(),
        reply_target: "chat-a".into(),
        content: "/model".into(),
        channel: "telegram".into(),
        timestamp: 1,
        thread_ts: None,
    });
    let chat_b = conversation_history_key(&traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "2".into(),
        sender: "alice".into(),
        reply_target: "chat-b".into(),
        content: "hi".into(),
        channel: "telegram".into(),
        timestamp: 2,
        thread_ts: None,
    });

    assert_ne!(
        chat_a, chat_b,
        "a /model pin must not follow the sender into another chat"
    );
}

/// A role the normalizer does not expect is dropped from the rebuilt turn
/// list. Nothing writes one today; this pins that the loss is reported rather
/// than silent, since it would be permanent after the next compaction.
#[test]
fn non_standard_role_is_dropped_without_corrupting_the_pairing() {
    let turns = vec![
        ChatMessage::user("q1"),
        ChatMessage {
            role: "tool".to_string(),
            ..ChatMessage::assistant("tool output")
        },
        ChatMessage::assistant("a1"),
    ];

    let normalized = normalize_cached_channel_turns(turns);

    assert_eq!(
        normalized.len(),
        2,
        "the unexpected role is not smuggled into the pairing"
    );
    assert_eq!(normalized[0].role, "user");
    assert_eq!(normalized[1].role, "assistant");
    assert_eq!(normalized[1].content, "a1");
}

#[tokio::test]
async fn message_dispatch_processes_messages_in_parallel() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(SlowProvider {
            delay: Duration::from_millis(250),
        }),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 10,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(4);
    tx.send(traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "1".to_string(),
        sender: "alice".to_string(),
        reply_target: "alice".to_string(),
        content: "hello".to_string(),
        channel: "test-channel".to_string(),
        timestamp: 1,
        thread_ts: None,
    })
    .await
    .unwrap();
    tx.send(traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "2".to_string(),
        sender: "bob".to_string(),
        reply_target: "bob".to_string(),
        content: "world".to_string(),
        channel: "test-channel".to_string(),
        timestamp: 2,
        thread_ts: None,
    })
    .await
    .unwrap();
    drop(tx);

    let started = Instant::now();
    run_message_dispatch_loop(rx, runtime_ctx, 2).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(430),
        "expected parallel dispatch (<430ms), got {:?}",
        elapsed
    );

    let sent_messages = channel_impl.sent_messages.lock().await;
    assert_eq!(sent_messages.len(), 2);
}

#[tokio::test]
async fn message_dispatch_interrupts_in_flight_telegram_request_and_preserves_context() {
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let provider_impl = Arc::new(DelayedHistoryCaptureProvider {
        delay: Duration::from_millis(250),
        calls: std::sync::Mutex::new(Vec::new()),
    });

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: provider_impl.clone(),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 10,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: true,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(8);
    let send_task = tokio::spawn(async move {
        tx.send(traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "forwarded content".to_string(),
            channel: "telegram".to_string(),
            timestamp: 1,
            thread_ts: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        tx.send(traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-2".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "summarize this".to_string(),
            channel: "telegram".to_string(),
            timestamp: 2,
            thread_ts: None,
        })
        .await
        .unwrap();
    });

    run_message_dispatch_loop(rx, runtime_ctx, 4).await;
    send_task.await.unwrap();

    let sent_messages = channel_impl.sent_messages.lock().await;
    assert_eq!(sent_messages.len(), 1);
    assert!(sent_messages[0].starts_with("chat-1:"));
    assert!(sent_messages[0].contains("response-2"));
    drop(sent_messages);

    let calls = provider_impl
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(calls.len(), 2);
    let second_call = &calls[1];
    assert!(second_call
        .iter()
        .any(|(role, content)| { role == "user" && content.contains("forwarded content") }));
    assert!(second_call
        .iter()
        .any(|(role, content)| { role == "user" && content.contains("summarize this") }));
    assert!(
        !second_call.iter().any(|(role, _)| role == "assistant"),
        "cancelled turn should not persist an assistant response"
    );
}

#[tokio::test]
async fn message_dispatch_interrupt_scope_is_same_sender_same_chat() {
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(SlowProvider {
            delay: Duration::from_millis(180),
        }),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 10,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: true,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(8);
    let send_task = tokio::spawn(async move {
        tx.send(traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-a".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "first chat".to_string(),
            channel: "telegram".to_string(),
            timestamp: 1,
            thread_ts: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        tx.send(traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-b".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-2".to_string(),
            content: "second chat".to_string(),
            channel: "telegram".to_string(),
            timestamp: 2,
            thread_ts: None,
        })
        .await
        .unwrap();
    });

    run_message_dispatch_loop(rx, runtime_ctx, 4).await;
    send_task.await.unwrap();

    let sent_messages = channel_impl.sent_messages.lock().await;
    assert_eq!(sent_messages.len(), 2);
    assert!(sent_messages.iter().any(|msg| msg.starts_with("chat-1:")));
    assert!(sent_messages.iter().any(|msg| msg.starts_with("chat-2:")));
}

#[tokio::test]
async fn process_channel_message_cancels_scoped_typing_task() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: Arc::new(SlowProvider {
            delay: Duration::from_millis(20),
        }),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 10,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx,
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "typing-msg".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-typing".to_string(),
            content: "hello".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    let starts = channel_impl.start_typing_calls.load(Ordering::SeqCst);
    let stops = channel_impl.stop_typing_calls.load(Ordering::SeqCst);
    assert_eq!(starts, 1, "start_typing should be called once");
    assert_eq!(stops, 1, "stop_typing should be called once");
}

#[test]
fn prompt_contains_all_sections() {
    let ws = make_workspace();
    let tools = vec![("shell", "Run commands"), ("file_read", "Read files")];
    let prompt = build_system_prompt(ws.path(), "test-model", &tools, &[], None, None);

    // Section headers
    assert!(prompt.contains("## Tools"), "missing Tools section");
    assert!(prompt.contains("## Safety"), "missing Safety section");
    assert!(prompt.contains("## Workspace"), "missing Workspace section");
    assert!(
        prompt.contains("## Project Context"),
        "missing Project Context"
    );
    assert!(
        prompt.contains("## Current Date & Time"),
        "missing Date/Time"
    );
    assert!(prompt.contains("## Runtime"), "missing Runtime section");
}

#[test]
fn prompt_injects_tools() {
    let ws = make_workspace();
    let tools = vec![
        ("shell", "Run commands"),
        ("memory_recall", "Search memory"),
    ];
    let prompt = build_system_prompt(ws.path(), "gpt-4o", &tools, &[], None, None);

    assert!(prompt.contains("**shell**"));
    assert!(prompt.contains("Run commands"));
    assert!(prompt.contains("**memory_recall**"));
}

#[test]
fn prompt_includes_single_tool_protocol_block_after_append() {
    let ws = make_workspace();
    let tools = vec![("shell", "Run commands")];
    let mut prompt = build_system_prompt(ws.path(), "gpt-4o", &tools, &[], None, None);

    assert!(
        !prompt.contains("## Tool Use Protocol"),
        "build_system_prompt should not emit protocol block directly"
    );

    prompt.push_str(&build_tool_instructions(&[]));

    assert_eq!(
        prompt.matches("## Tool Use Protocol").count(),
        1,
        "protocol block should appear exactly once in the final prompt"
    );
}

#[test]
fn prompt_injects_safety() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

    assert!(prompt.contains("Do not exfiltrate private data"));
    assert!(prompt.contains("Do not run destructive commands"));
    assert!(prompt.contains("Prefer `trash` over `rm`"));
}

#[test]
fn prompt_injects_workspace_files() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

    assert!(prompt.contains("### SOUL.md"), "missing SOUL.md header");
    assert!(prompt.contains("Be helpful"), "missing SOUL content");
    assert!(prompt.contains("### IDENTITY.md"), "missing IDENTITY.md");
    assert!(
        prompt.contains("Name: RantaiClaw"),
        "missing IDENTITY content"
    );
    assert!(prompt.contains("### USER.md"), "missing USER.md");
    assert!(prompt.contains("### AGENTS.md"), "missing AGENTS.md");
    assert!(prompt.contains("### TOOLS.md"), "missing TOOLS.md");
    // HEARTBEAT.md is intentionally excluded from channel prompts — it's only
    // relevant to the heartbeat worker and causes LLMs to emit spurious
    // "HEARTBEAT_OK" acknowledgments in channel conversations.
    assert!(
        !prompt.contains("### HEARTBEAT.md"),
        "HEARTBEAT.md should not be in channel prompt"
    );
    assert!(prompt.contains("### MEMORY.md"), "missing MEMORY.md");
    assert!(prompt.contains("User likes Rust"), "missing MEMORY content");
}

#[test]
fn prompt_missing_file_markers() {
    let tmp = TempDir::new().unwrap();
    // Empty workspace — no files at all
    let prompt = build_system_prompt(tmp.path(), "model", &[], &[], None, None);

    assert!(prompt.contains("[File not found: SOUL.md]"));
    assert!(prompt.contains("[File not found: AGENTS.md]"));
    assert!(prompt.contains("[File not found: IDENTITY.md]"));
}

#[test]
fn prompt_bootstrap_only_if_exists() {
    let ws = make_workspace();
    // No BOOTSTRAP.md — should not appear
    let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);
    assert!(
        !prompt.contains("### BOOTSTRAP.md"),
        "BOOTSTRAP.md should not appear when missing"
    );

    // Create BOOTSTRAP.md — should appear
    std::fs::write(ws.path().join("BOOTSTRAP.md"), "# Bootstrap\nFirst run.").unwrap();
    let prompt2 = build_system_prompt(ws.path(), "model", &[], &[], None, None);
    assert!(
        prompt2.contains("### BOOTSTRAP.md"),
        "BOOTSTRAP.md should appear when present"
    );
    assert!(prompt2.contains("First run"));
}

#[test]
fn prompt_no_daily_memory_injection() {
    let ws = make_workspace();
    let memory_dir = ws.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    std::fs::write(
        memory_dir.join(format!("{today}.md")),
        "# Daily\nSome note.",
    )
    .unwrap();

    let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

    // Daily notes should NOT be in the system prompt (on-demand via tools)
    assert!(
        !prompt.contains("Daily Notes"),
        "daily notes should not be auto-injected"
    );
    assert!(
        !prompt.contains("Some note"),
        "daily content should not be in prompt"
    );
}

#[test]
fn prompt_runtime_metadata() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "claude-sonnet-4", &[], &[], None, None);

    assert!(prompt.contains("Model: claude-sonnet-4"));
    assert!(prompt.contains(&format!("OS: {}", std::env::consts::OS)));
    assert!(prompt.contains("Host:"));
}

#[test]
fn prompt_skills_include_instructions_and_tools() {
    let ws = make_workspace();
    let skills = vec![crate::skills::Skill {
        name: "code-review".into(),
        description: "Review code for bugs".into(),
        version: "1.0.0".into(),
        author: None,
        tags: vec![],
        tools: vec![crate::skills::SkillTool {
            name: "lint".into(),
            description: "Run static checks".into(),
            kind: "shell".into(),
            command: "cargo clippy".into(),
            args: HashMap::new(),
        }],
        prompts: vec!["Always run cargo test before final response.".into()],
        location: None,
        requires: crate::skills::SkillRequires::default(),
        install_recipes: Vec::new(),
        remote: false,
        origin: None,
    }];

    let prompt = build_system_prompt(ws.path(), "model", &[], &skills, None, None);

    assert!(prompt.contains("<available_skills>"), "missing skills XML");
    assert!(prompt.contains("<name>code-review</name>"));
    assert!(prompt.contains("<description>Review code for bugs</description>"));
    assert!(prompt.contains("SKILL.md</location>"));
    assert!(prompt.contains("<instructions>"));
    assert!(
        prompt.contains("<instruction>Always run cargo test before final response.</instruction>")
    );
    assert!(prompt.contains("<tools>"));
    assert!(prompt.contains("<name>lint</name>"));
    assert!(prompt.contains("<kind>shell</kind>"));
    assert!(!prompt.contains("loaded on demand"));
}

#[test]
fn prompt_skills_compact_mode_omits_instructions_and_tools() {
    let ws = make_workspace();
    let skills = vec![crate::skills::Skill {
        name: "code-review".into(),
        description: "Review code for bugs".into(),
        version: "1.0.0".into(),
        author: None,
        tags: vec![],
        tools: vec![crate::skills::SkillTool {
            name: "lint".into(),
            description: "Run static checks".into(),
            kind: "shell".into(),
            command: "cargo clippy".into(),
            args: HashMap::new(),
        }],
        prompts: vec!["Always run cargo test before final response.".into()],
        location: None,
        requires: crate::skills::SkillRequires::default(),
        install_recipes: Vec::new(),
        remote: false,
        origin: None,
    }];

    let prompt = build_system_prompt_with_mode(
        ws.path(),
        "model",
        &[],
        &skills,
        None,
        None,
        false,
        crate::config::SkillsPromptInjectionMode::Compact,
    );

    assert!(prompt.contains("<available_skills>"), "missing skills XML");
    assert!(prompt.contains("<name>code-review</name>"));
    assert!(prompt.contains("<location>skills/code-review/SKILL.md</location>"));
    assert!(prompt.contains("loaded on demand"));
    assert!(!prompt.contains("<instructions>"));
    assert!(
        !prompt.contains("<instruction>Always run cargo test before final response.</instruction>")
    );
    assert!(!prompt.contains("<tools>"));
}

#[test]
fn prompt_skills_escape_reserved_xml_chars() {
    let ws = make_workspace();
    let skills = vec![crate::skills::Skill {
        name: "code<review>&".into(),
        description: "Review \"unsafe\" and 'risky' bits".into(),
        version: "1.0.0".into(),
        author: None,
        tags: vec![],
        tools: vec![crate::skills::SkillTool {
            name: "run\"linter\"".into(),
            description: "Run <lint> & report".into(),
            kind: "shell&exec".into(),
            command: "cargo clippy".into(),
            args: HashMap::new(),
        }],
        prompts: vec!["Use <tool_call> and & keep output \"safe\"".into()],
        location: None,
        requires: crate::skills::SkillRequires::default(),
        install_recipes: Vec::new(),
        remote: false,
        origin: None,
    }];

    let prompt = build_system_prompt(ws.path(), "model", &[], &skills, None, None);

    assert!(prompt.contains("<name>code&lt;review&gt;&amp;</name>"));
    assert!(prompt.contains(
        "<description>Review &quot;unsafe&quot; and &apos;risky&apos; bits</description>"
    ));
    assert!(prompt.contains("<name>run&quot;linter&quot;</name>"));
    assert!(prompt.contains("<description>Run &lt;lint&gt; &amp; report</description>"));
    assert!(prompt.contains("<kind>shell&amp;exec</kind>"));
    assert!(prompt.contains(
        "<instruction>Use &lt;tool_call&gt; and &amp; keep output &quot;safe&quot;</instruction>"
    ));
}

#[test]
fn prompt_truncation() {
    let ws = make_workspace();
    // Write a file larger than BOOTSTRAP_MAX_CHARS
    let big_content = "x".repeat(BOOTSTRAP_MAX_CHARS + 1000);
    std::fs::write(ws.path().join("AGENTS.md"), &big_content).unwrap();

    let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

    assert!(
        prompt.contains("truncated at"),
        "large files should be truncated"
    );
    assert!(
        !prompt.contains(&big_content),
        "full content should not appear"
    );
}

#[test]
fn prompt_empty_files_skipped() {
    let ws = make_workspace();
    std::fs::write(ws.path().join("TOOLS.md"), "").unwrap();

    let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

    // Empty file should not produce a header
    assert!(
        !prompt.contains("### TOOLS.md"),
        "empty files should be skipped"
    );
}

#[test]
fn channel_log_truncation_is_utf8_safe_for_multibyte_text() {
    let msg = "Hello from RantaiClaw 🌍. Current status is healthy, and café-style UTF-8 text stays safe in logs.";

    // Reproduces the production crash path where channel logs truncate at 80 chars.
    let result = std::panic::catch_unwind(|| crate::util::truncate_with_ellipsis(msg, 80));
    assert!(
        result.is_ok(),
        "truncate_with_ellipsis should never panic on UTF-8"
    );

    let truncated = result.unwrap();
    assert!(!truncated.is_empty());
    assert!(truncated.is_char_boundary(truncated.len()));
}

#[test]
fn prompt_contains_channel_capabilities() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

    assert!(
        prompt.contains("## Channel Capabilities"),
        "missing Channel Capabilities section"
    );
    assert!(
        prompt.contains("running as a messaging bot"),
        "missing channel context"
    );
    assert!(
        prompt.contains("NEVER repeat, describe, or echo credentials"),
        "missing security instruction"
    );
}

#[test]
fn prompt_workspace_path() {
    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

    assert!(prompt.contains(&format!("Working directory: `{}`", ws.path().display())));
}

#[test]
fn conversation_memory_key_uses_message_id() {
    let msg = traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "msg_abc123".into(),
        sender: "U123".into(),
        reply_target: "C456".into(),
        content: "hello".into(),
        channel: "slack".into(),
        timestamp: 1,
        thread_ts: None,
    };

    assert_eq!(conversation_memory_key(&msg), "slack_U123_msg_abc123");
}

#[test]
fn conversation_memory_key_is_unique_per_message() {
    let msg1 = traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "msg_1".into(),
        sender: "U123".into(),
        reply_target: "C456".into(),
        content: "first".into(),
        channel: "slack".into(),
        timestamp: 1,
        thread_ts: None,
    };
    let msg2 = traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "msg_2".into(),
        sender: "U123".into(),
        reply_target: "C456".into(),
        content: "second".into(),
        channel: "slack".into(),
        timestamp: 2,
        thread_ts: None,
    };

    assert_ne!(
        conversation_memory_key(&msg1),
        conversation_memory_key(&msg2)
    );
}

#[tokio::test]
async fn autosave_keys_preserve_multiple_conversation_facts() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();

    let msg1 = traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "msg_1".into(),
        sender: "U123".into(),
        reply_target: "C456".into(),
        content: "I'm Paul".into(),
        channel: "slack".into(),
        timestamp: 1,
        thread_ts: None,
    };
    let msg2 = traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "msg_2".into(),
        sender: "U123".into(),
        reply_target: "C456".into(),
        content: "I'm 45".into(),
        channel: "slack".into(),
        timestamp: 2,
        thread_ts: None,
    };

    mem.store(
        &conversation_memory_key(&msg1),
        &msg1.content,
        MemoryCategory::Conversation,
        None,
    )
    .await
    .unwrap();
    mem.store(
        &conversation_memory_key(&msg2),
        &msg2.content,
        MemoryCategory::Conversation,
        None,
    )
    .await
    .unwrap();

    assert_eq!(mem.count().await.unwrap(), 2);

    let recalled = mem.recall("45", 5, None).await.unwrap();
    assert!(recalled.iter().any(|entry| entry.content.contains("45")));
}

#[tokio::test]
async fn build_memory_context_includes_recalled_entries() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    mem.store("age_fact", "Age is 45", MemoryCategory::Core, None)
        .await
        .unwrap();

    let context = build_memory_context(&mem, "age", 0.0, None).await;
    assert!(context.contains("[Memory context]"));
    assert!(context.contains("Age is 45"));
}

#[tokio::test]
async fn build_memory_context_surfaces_conversation_scoped_entry() {
    let tmp = TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    // Stored under a specific conversation scope, it must be recalled when
    // that conversation asks. Curated category: `conversation` entries are
    // auto-save transcript rows and are excluded from injection outright.
    mem.store(
        "scoped_fact",
        "Project ships Friday",
        MemoryCategory::Core,
        Some("telegram:u1"),
    )
    .await
    .unwrap();

    let context = build_memory_context(&mem, "ship", 0.0, Some("telegram:u1")).await;
    assert!(context.contains("Project ships Friday"));
}

#[tokio::test]
async fn process_channel_message_restores_per_sender_history_on_follow_ups() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let provider_impl = Arc::new(HistoryCaptureProvider::default());

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: provider_impl.clone(),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx.clone(),
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-a".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "hello".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    process_channel_message(
        runtime_ctx,
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-b".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-1".to_string(),
            content: "follow up".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 2,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    let calls = provider_impl
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].len(), 2);
    assert_eq!(calls[0][0].0, "system");
    assert_eq!(calls[0][1].0, "user");
    assert_eq!(calls[1].len(), 4);
    assert_eq!(calls[1][0].0, "system");
    assert_eq!(calls[1][1].0, "user");
    assert_eq!(calls[1][2].0, "assistant");
    assert_eq!(calls[1][3].0, "user");
    assert!(calls[1][1].1.contains("hello"));
    assert!(calls[1][2].1.contains("response-1"));
    assert!(calls[1][3].1.contains("follow up"));
}

#[tokio::test]
async fn process_channel_message_enriches_current_turn_without_persisting_context() {
    let channel_impl = Arc::new(RecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let provider_impl = Arc::new(HistoryCaptureProvider::default());
    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: provider_impl.clone(),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(RecallMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx.clone(),
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "msg-ctx-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-ctx".to_string(),
            content: "hello".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    let calls = provider_impl
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].len(), 2);
    assert_eq!(calls[0][1].0, "user");
    assert!(calls[0][1].1.contains("[Memory context]"));
    assert!(calls[0][1].1.contains("Age is 45"));
    assert!(calls[0][1].1.contains("hello"));

    let histories = runtime_ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let turns = histories
        .get(&conversation::ConversationKey::new("test-channel", "chat-ctx").resolve())
        .expect("history should be stored for sender");
    assert_eq!(turns[0].role, "user");
    assert_eq!(turns[0].content, "hello");
    assert!(!turns[0].content.contains("[Memory context]"));
}

#[tokio::test]
async fn process_channel_message_telegram_keeps_system_instruction_at_top_only() {
    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();

    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let provider_impl = Arc::new(HistoryCaptureProvider::default());
    let mut histories = HashMap::new();
    histories.insert(
        conversation::ConversationKey::new("telegram", "chat-telegram").resolve(),
        vec![
            ChatMessage::assistant("stale assistant"),
            ChatMessage::user("earlier user question"),
            ChatMessage::assistant("earlier assistant reply"),
        ],
    );

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: provider_impl.clone(),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(histories)),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(std::env::temp_dir()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx.clone(),
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "tg-msg-1".to_string(),
            sender: "alice".to_string(),
            reply_target: "chat-telegram".to_string(),
            content: "hello".to_string(),
            channel: "telegram".to_string(),
            timestamp: 1,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    let calls = provider_impl
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].len(), 4);

    let roles = calls[0]
        .iter()
        .map(|(role, _)| role.as_str())
        .collect::<Vec<_>>();
    assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
    assert!(
        calls[0][0]
            .1
            .contains("When responding on Telegram, include media markers"),
        "telegram delivery instruction should live in the system prompt"
    );
    assert!(!calls[0].iter().skip(1).any(|(role, _)| role == "system"));
}

/// The channel dispatcher shares `build_memory_context` with the agent and
/// the CLI loop, and it has the same shape that broke them: it auto-saves
/// the inbound message *before* recalling, so the store holds a verbatim
/// copy of the question by the time the context block is built.
///
/// This drives the real dispatcher against a real SQLite store — no stub
/// memory — and reads the prompt off the provider. Without the self-echo
/// drop the block is the question quoted back, and the curated fact never
/// reaches the model.
#[tokio::test]
async fn channel_turn_recalls_facts_not_the_question_it_was_asked() {
    // Long enough to clear AUTOSAVE_MIN_MESSAGE_CHARS, or nothing is saved
    // and there is no echo to reproduce.
    let question = "when is the deployment window for this service";
    assert!(question.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS);

    let tmp = tempfile::TempDir::new().unwrap();
    let mem = crate::memory::SqliteMemory::new(tmp.path()).unwrap();
    mem.store(
        "deploy_window",
        "The deployment window for this service is Friday afternoons",
        crate::memory::MemoryCategory::Core,
        None,
    )
    .await
    .unwrap();

    let channel_impl = Arc::new(TelegramRecordingChannel::default());
    let channel: Arc<dyn Channel> = channel_impl.clone();
    let mut channels_by_name = HashMap::new();
    channels_by_name.insert(channel.name().to_string(), channel);

    let provider_impl = Arc::new(HistoryCaptureProvider::default());
    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(routing::RuntimeConfigSlot::default())),
        channels_by_name: Arc::new(channels_by_name),
        provider: provider_impl.clone(),
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(mem),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("test-model".to_string()),
        temperature: 0.0,
        // Both of these matter: auto-save writes the echo, and the real
        // default threshold is what the echo's ranking pushed facts under.
        auto_save_memory: true,
        max_tool_iterations: 5,
        min_relevance_score: 0.4,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        provider_cache: Arc::new(Mutex::new(HashMap::new())),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: None,
        api_url: None,
        reliability: Arc::new(crate::config::ReliabilityConfig::default()),
        provider_runtime_options: providers::ProviderRuntimeOptions::default(),
        workspace_dir: Arc::new(tmp.path().to_path_buf()),
        message_timeout_secs: CHANNEL_MESSAGE_TIMEOUT_SECS,
        interrupt_on_new_message: false,
        multimodal: crate::config::MultimodalConfig::default(),
        security: Arc::new(crate::security::SecurityPolicy::default()),
        channel_approval: None,
        approval_owners: Arc::new(Vec::new()),
        tool_approvals: Arc::new(crate::security::PendingApprovals::default()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            Vec::<String>::new(),
            &[],
            &[],
        )),
    });

    process_channel_message(
        runtime_ctx,
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: "tg-mem-1".to_string(),
            sender: "rantaiclaw_user".to_string(),
            reply_target: "chat-telegram".to_string(),
            content: question.to_string(),
            channel: "telegram".to_string(),
            timestamp: 1,
            thread_ts: None,
        },
        CancellationToken::new(),
    )
    .await;

    let calls = provider_impl
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let user_turn = calls[0]
        .iter()
        .find(|(role, _)| role == "user")
        .map(|(_, content)| content.clone())
        .expect("a user turn should reach the provider");

    let block = user_turn
        .split_once(question)
        .map(|(before, _)| before.to_string())
        .unwrap_or_default();

    assert!(
        block.contains("deployment window for this service is Friday"),
        "the curated fact never reached the prompt:\n{user_turn}"
    );
    assert!(
        !block.contains(question),
        "the question was injected back as its own context:\n{user_turn}"
    );
}

#[test]
fn extract_tool_context_summary_collects_alias_and_native_tool_calls() {
    let history = vec![
        ChatMessage::system("sys"),
        ChatMessage::assistant(
            r#"<toolcall>
{"name":"shell","arguments":{"command":"date"}}
</toolcall>"#,
        ),
        ChatMessage::assistant(
            r#"{"content":null,"tool_calls":[{"id":"1","name":"web_search","arguments":"{}"}]}"#,
        ),
    ];

    let summary = extract_tool_context_summary(&history, 1);
    assert_eq!(summary, "[Used tools: shell, web_search]");
}

#[test]
fn extract_tool_context_summary_collects_prompt_mode_tool_result_names() {
    let history = vec![
        ChatMessage::system("sys"),
        ChatMessage::assistant("Using markdown tool call fence"),
        ChatMessage::user(
            r#"[Tool results]
<tool_result name="http_request">
{"status":200}
</tool_result>
<tool_result name="shell">
Mon Feb 20
</tool_result>"#,
        ),
    ];

    let summary = extract_tool_context_summary(&history, 1);
    assert_eq!(summary, "[Used tools: http_request, shell]");
}

#[test]
fn extract_tool_context_summary_respects_start_index() {
    let history = vec![
        ChatMessage::assistant(
            r#"<tool_call>
{"name":"stale_tool","arguments":{}}
</tool_call>"#,
        ),
        ChatMessage::assistant(
            r#"<tool_call>
{"name":"fresh_tool","arguments":{}}
</tool_call>"#,
        ),
    ];

    let summary = extract_tool_context_summary(&history, 1);
    assert_eq!(summary, "[Used tools: fresh_tool]");
}

#[test]
fn clean_delivered_reply_strips_leading_tool_annotation() {
    let out = clean_delivered_reply("[Used tools: cron_list]\nAll set.");
    assert_eq!(out, "All set.");
}

#[test]
fn clean_delivered_reply_annotation_only_becomes_fallback() {
    let out = clean_delivered_reply("[Used tools: cron_list, manage_permissions]");
    assert_eq!(out, CHANNEL_EMPTY_REPLY_FALLBACK);
}

#[test]
fn clean_delivered_reply_empty_becomes_fallback() {
    assert_eq!(clean_delivered_reply("   "), CHANNEL_EMPTY_REPLY_FALLBACK);
}

#[test]
fn clean_delivered_reply_passes_through_normal_text() {
    assert_eq!(
        clean_delivered_reply("Here is your answer."),
        "Here is your answer."
    );
}

// ── AIEOS Identity Tests (Issue #168) ─────────────────────────

#[test]
fn aieos_identity_from_file() {
    use crate::config::IdentityConfig;
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let identity_path = tmp.path().join("aieos_identity.json");

    // Write AIEOS identity file
    let aieos_json = r#"{
            "identity": {
                "names": {"first": "Nova", "nickname": "Nov"},
                "bio": "A helpful AI assistant.",
                "origin": "Silicon Valley"
            },
            "psychology": {
                "mbti": "INTJ",
                "moral_compass": ["Be helpful", "Do no harm"]
            },
            "linguistics": {
                "style": "concise",
                "formality": "casual"
            }
        }"#;
    std::fs::write(&identity_path, aieos_json).unwrap();

    // Create identity config pointing to the file
    let config = IdentityConfig {
        format: "aieos".into(),
        aieos_path: Some("aieos_identity.json".into()),
        aieos_inline: None,
    };

    let prompt = build_system_prompt(tmp.path(), "model", &[], &[], Some(&config), None);

    // Should contain AIEOS sections
    assert!(prompt.contains("## Identity"));
    assert!(prompt.contains("**Name:** Nova"));
    assert!(prompt.contains("**Nickname:** Nov"));
    assert!(prompt.contains("**Bio:** A helpful AI assistant."));
    assert!(prompt.contains("**Origin:** Silicon Valley"));

    assert!(prompt.contains("## Personality"));
    assert!(prompt.contains("**MBTI:** INTJ"));
    assert!(prompt.contains("**Moral Compass:**"));
    assert!(prompt.contains("- Be helpful"));

    assert!(prompt.contains("## Communication Style"));
    assert!(prompt.contains("**Style:** concise"));
    assert!(prompt.contains("**Formality Level:** casual"));

    // Should NOT contain OpenClaw bootstrap file headers
    assert!(!prompt.contains("### SOUL.md"));
    assert!(!prompt.contains("### IDENTITY.md"));
    assert!(!prompt.contains("[File not found"));
}

#[test]
fn aieos_identity_from_inline() {
    use crate::config::IdentityConfig;

    let config = IdentityConfig {
        format: "aieos".into(),
        aieos_path: None,
        aieos_inline: Some(r#"{"identity":{"names":{"first":"Claw"}}}"#.into()),
    };

    let prompt = build_system_prompt(
        std::env::temp_dir().as_path(),
        "model",
        &[],
        &[],
        Some(&config),
        None,
    );

    assert!(prompt.contains("**Name:** Claw"));
    assert!(prompt.contains("## Identity"));
}

#[test]
fn aieos_fallback_to_openclaw_on_parse_error() {
    use crate::config::IdentityConfig;

    let config = IdentityConfig {
        format: "aieos".into(),
        aieos_path: Some("nonexistent.json".into()),
        aieos_inline: None,
    };

    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "model", &[], &[], Some(&config), None);

    // Should fall back to OpenClaw format when AIEOS file is not found
    // (Error is logged to stderr with filename, not included in prompt)
    assert!(prompt.contains("### SOUL.md"));
}

#[test]
fn aieos_empty_uses_openclaw() {
    use crate::config::IdentityConfig;

    // Format is "aieos" but neither path nor inline is set
    let config = IdentityConfig {
        format: "aieos".into(),
        aieos_path: None,
        aieos_inline: None,
    };

    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "model", &[], &[], Some(&config), None);

    // Should use OpenClaw format (not configured for AIEOS)
    assert!(prompt.contains("### SOUL.md"));
    assert!(prompt.contains("Be helpful"));
}

#[test]
fn openclaw_format_uses_bootstrap_files() {
    use crate::config::IdentityConfig;

    let config = IdentityConfig {
        format: "openclaw".into(),
        aieos_path: Some("identity.json".into()),
        aieos_inline: None,
    };

    let ws = make_workspace();
    let prompt = build_system_prompt(ws.path(), "model", &[], &[], Some(&config), None);

    // Should use OpenClaw format even if aieos_path is set
    assert!(prompt.contains("### SOUL.md"));
    assert!(prompt.contains("Be helpful"));
    assert!(!prompt.contains("## Identity"));
}

#[test]
fn none_identity_config_uses_openclaw() {
    let ws = make_workspace();
    // Pass None for identity config
    let prompt = build_system_prompt(ws.path(), "model", &[], &[], None, None);

    // Should use OpenClaw format
    assert!(prompt.contains("### SOUL.md"));
    assert!(prompt.contains("Be helpful"));
}

#[test]
fn classify_health_ok_true() {
    let state = admin::classify_health_result(&Ok(true));
    assert_eq!(state, admin::ChannelHealthState::Healthy);
}

#[test]
fn classify_health_ok_false() {
    let state = admin::classify_health_result(&Ok(false));
    assert_eq!(state, admin::ChannelHealthState::Unhealthy);
}

#[tokio::test]
async fn classify_health_timeout() {
    let result = tokio::time::timeout(Duration::from_millis(1), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        true
    })
    .await;
    let state = admin::classify_health_result(&result);
    assert_eq!(state, admin::ChannelHealthState::Timeout);
}

struct AlwaysFailChannel {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

struct BlockUntilClosedChannel {
    name: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Channel for AlwaysFailChannel {
    fn name(&self) -> &str {
        self.name
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(
        &self,
        _tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("listen boom")
    }
}

#[async_trait::async_trait]
impl Channel for BlockUntilClosedChannel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tx.closed().await;
        Ok(())
    }
}

/// A channel whose listener stays healthy forever while the platform behind it
/// does not answer — an expired bot token, a revoked webhook, a workspace the
/// bot was removed from. This is the shape the heartbeat used to report as OK.
struct LiveListenerDeadPlatformChannel {
    name: String,
    probes: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Channel for LiveListenerDeadPlatformChannel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        tx.closed().await;
        Ok(())
    }

    async fn health_check(&self) -> bool {
        self.probes.fetch_add(1, Ordering::SeqCst);
        false
    }
}

/// The defect: the heartbeat called `mark_component_ok` unconditionally, so a
/// channel whose listener task was alive reported healthy no matter what the
/// platform said. `health_check` existed on sixteen channels and had exactly
/// one caller — a one-shot CLI command.
#[tokio::test]
async fn a_channel_whose_platform_stops_answering_is_reported_unhealthy() {
    let probes = Arc::new(AtomicUsize::new(0));
    let channel_name = format!("test-supervised-deadplatform-{}", uuid::Uuid::new_v4());
    let component = format!("channel:{channel_name}");
    let channel: Arc<dyn Channel> = Arc::new(LiveListenerDeadPlatformChannel {
        name: channel_name,
        probes: Arc::clone(&probes),
    });

    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let shutdown = CancellationToken::new();
    let handle = supervisor::spawn_supervised_listener_with_health_interval(
        channel,
        tx,
        1,
        1,
        Duration::from_millis(20),
        shutdown.clone(),
    );

    // The threshold is deliberate: one failed probe must NOT flip the status.
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        crate::health::snapshot_json()["components"][&component]["status"],
        "ok",
        "a single failed probe must not flap the status"
    );

    // Past the threshold it must.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        probes.load(Ordering::SeqCst) >= usize::try_from(CHANNEL_HEALTH_FAILURE_THRESHOLD).unwrap(),
        "the supervisor must actually call health_check, got {} probes",
        probes.load(Ordering::SeqCst)
    );
    let snapshot = crate::health::snapshot_json();
    assert_ne!(
        snapshot["components"][&component]["status"], "ok",
        "a channel whose platform stops answering must stop reporting healthy: {}",
        snapshot["components"][&component]
    );

    shutdown.cancel();
    drop(rx);
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
}

#[tokio::test]
async fn supervised_listener_marks_error_and_restarts_on_failures() {
    let calls = Arc::new(AtomicUsize::new(0));
    // UUID-suffixed like its two neighbours. `crate::health` is a process-wide
    // registry, so a fixed component name collides with any other test that
    // registers the same one — and the collision surfaces as a confusing
    // assertion on somebody else's restart count, not as a name clash.
    let name: &'static str =
        Box::leak(format!("test-supervised-fail-{}", uuid::Uuid::new_v4()).into_boxed_str());
    let channel: Arc<dyn Channel> = Arc::new(AlwaysFailChannel {
        name,
        calls: Arc::clone(&calls),
    });

    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let handle = supervisor::spawn_supervised_listener(channel, tx, 1, 1, CancellationToken::new());

    tokio::time::sleep(Duration::from_millis(80)).await;
    drop(rx);
    handle.abort();
    let _ = handle.await;

    let snapshot = crate::health::snapshot_json();
    let component = &snapshot["components"][format!("channel:{name}")];
    assert_eq!(component["status"], "error");
    assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
    assert!(component["last_error"]
        .as_str()
        .unwrap_or("")
        .contains("listen boom"));
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn supervised_listener_refreshes_health_while_running() {
    let calls = Arc::new(AtomicUsize::new(0));
    let channel_name = format!("test-supervised-heartbeat-{}", uuid::Uuid::new_v4());
    let component_name = format!("channel:{channel_name}");
    let channel: Arc<dyn Channel> = Arc::new(BlockUntilClosedChannel {
        name: channel_name,
        calls: Arc::clone(&calls),
    });

    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let handle = supervisor::spawn_supervised_listener_with_health_interval(
        channel,
        tx,
        1,
        1,
        Duration::from_millis(20),
        CancellationToken::new(),
    );

    tokio::time::sleep(Duration::from_millis(35)).await;
    let first_last_ok = crate::health::snapshot_json()["components"][&component_name]["last_ok"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(!first_last_ok.is_empty());

    tokio::time::sleep(Duration::from_millis(70)).await;
    let second_last_ok = crate::health::snapshot_json()["components"][&component_name]["last_ok"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let first = chrono::DateTime::parse_from_rfc3339(&first_last_ok)
        .expect("last_ok should be valid RFC3339");
    let second = chrono::DateTime::parse_from_rfc3339(&second_last_ok)
        .expect("last_ok should be valid RFC3339");
    assert!(second > first, "expected periodic health heartbeat refresh");

    drop(rx);
    let join = tokio::time::timeout(Duration::from_secs(1), handle).await;
    assert!(join.is_ok(), "listener should stop after channel shutdown");
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn supervised_listener_stops_on_shutdown_cancellation() {
    // Regression guard for the in-place channel restart path: cancelling
    // the shutdown token must stop the listener even while the message
    // bus is still open (rx alive) AND the channel ignores the token —
    // `BlockUntilClosedChannel` parks on `tx.closed()` and never reads
    // `_cancel`, so only the supervisor's `shutdown.cancelled()` backstop
    // can unstick it. Without it the TUI could not restart channels.
    let calls = Arc::new(AtomicUsize::new(0));
    let channel_name = format!("test-supervised-shutdown-{}", uuid::Uuid::new_v4());
    let channel: Arc<dyn Channel> = Arc::new(BlockUntilClosedChannel {
        name: channel_name,
        calls: Arc::clone(&calls),
    });

    // Keep `_rx` alive so the bus never closes on its own — this proves
    // cancellation, not bus teardown, is what stops the listener.
    let (tx, _rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(1);
    let shutdown = CancellationToken::new();
    let handle = supervisor::spawn_supervised_listener(channel, tx, 1, 60, shutdown.clone());

    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "listener should have entered listen() before cancellation"
    );

    shutdown.cancel();
    let join = tokio::time::timeout(Duration::from_secs(1), handle).await;
    assert!(
        join.is_ok(),
        "listener should stop promptly when shutdown is cancelled"
    );
}

#[test]
fn maybe_restart_daemon_systemd_args_regression() {
    assert_eq!(
        SYSTEMD_STATUS_ARGS,
        ["--user", "is-active", "rantaiclaw.service"]
    );
    assert_eq!(
        SYSTEMD_RESTART_ARGS,
        ["--user", "restart", "rantaiclaw.service"]
    );
}

#[test]
fn maybe_restart_daemon_openrc_args_regression() {
    assert_eq!(OPENRC_STATUS_ARGS, ["rantaiclaw", "status"]);
    assert_eq!(OPENRC_RESTART_ARGS, ["rantaiclaw", "restart"]);
}

/// Plan 121's closing invariant: the module's public **function** surface is
/// exactly the symbols that were reachable from outside `src/channels/` before
/// the decomposition started. Ten modules now exist where one did; without this
/// a later extraction can widen the API by accident, one `pub use` at a time.
///
/// Channel types (`pub use telegram::TelegramChannel`, …) and the `Channel` /
/// `SendMessage` traits are deliberately not counted: they were public before
/// and are the module's type surface, not its call surface.
#[test]
fn channels_module_public_surface_is_the_documented_ten() {
    let src = include_str!("mod.rs");
    let production = src
        .split("\n#[cfg(test)]")
        .next()
        .expect("source has a production half");

    let mut public: Vec<String> = Vec::new();
    for line in production.lines() {
        let line = line.trim_end();
        if let Some(rest) = line.strip_prefix("pub async fn ") {
            public.push(rest.split(['(', '<']).next().unwrap_or(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("pub fn ") {
            public.push(rest.split(['(', '<']).next().unwrap_or(rest).to_string());
        }
    }
    // Re-exports of functions from the extracted modules.
    for reexport in [
        "doctor_channels",
        "build_system_prompt",
        "build_system_prompt_with_mode",
        "channel_supports_announce_delivery",
    ] {
        if production.contains(reexport) {
            public.push(reexport.to_string());
        }
    }
    public.sort();
    public.dedup();

    let expected = vec![
        "build_system_prompt".to_string(),
        "build_system_prompt_with_mode".to_string(),
        "channel_supports_announce_delivery".to_string(),
        "doctor_channels".to_string(),
        "start_channels".to_string(),
        "start_channels_with_cancellation".to_string(),
    ];
    assert_eq!(
        public, expected,
        "the public function surface changed. Adding one is a decision, not a \
         side effect of an extraction — update this list deliberately."
    );

    // The remaining external symbols are `pub(crate)` because `main.rs` compiles
    // this tree as its own crate: `handle_command`, `channel_roster`,
    // `announce_daemon_reload`, `reload_managed_daemon`, `build_configured_channels`.
    for crate_level in [
        "announce_daemon_reload",
        "channel_roster",
        "handle_command",
        "reload_managed_daemon",
        "build_configured_channels",
    ] {
        assert!(
            production.contains(crate_level),
            "{crate_level} is one of the ten external symbols and must stay reachable"
        );
    }
}

/// MEM-SCOPE-SENDER, surfaced by plan 118 when it fixed the same leak in
/// conversation history and left this one recorded.
///
/// The memory scope was built from `msg.sender` while history used
/// `msg.reply_target`, so one person's private DM and every group they share
/// with the bot were one memory scope — a detail stored in private could be
/// recalled into a public group.
#[test]
fn memory_scope_does_not_merge_a_dm_into_a_group() {
    let dm = traits::ChannelMessage {
        sender_aliases: Vec::new(),
        id: "1".into(),
        sender: "user_a".into(),
        reply_target: "private-chat".into(),
        content: "secret".into(),
        channel: "telegram".into(),
        timestamp: 1,
        thread_ts: None,
    };
    let group = traits::ChannelMessage {
        reply_target: "-1009999".into(),
        id: "2".into(),
        content: "public".into(),
        ..dm.clone()
    };

    assert_ne!(
        conversation_memory_scope(&dm),
        conversation_memory_scope(&group),
        "the same person's DM and a group they share with the bot are not one \
         memory scope — a private detail must not be recalled into a group"
    );

    // And it is the *same* scope history uses: keying them differently is what
    // produced this leak in the first place.
    assert_eq!(
        conversation_memory_scope(&dm),
        conversation_history_key(&dm)
    );
}
