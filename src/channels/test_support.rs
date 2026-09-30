//! Fixtures the dispatch tests share: the recording channels, the stub
//! providers and tools, and the helpers that build a dispatch context and drive
//! a guest turn.
//!
//! Two test modules use them, `mod_tests.rs` for the dispatch core and
//! `guest_privacy_tests.rs` for the guest privacy invariant. They lived
//! private to `mod_tests.rs` until the second module needed them.

use super::dispatch::*;
use super::*;
use crate::memory::Memory;
use crate::observability::NoopObserver;
use crate::providers::{ChatMessage, Provider};
use crate::tools::{Tool, ToolResult};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub(super) struct RecordingChannel {
    pub(super) sent_messages: tokio::sync::Mutex<Vec<String>>,
    pub(super) start_typing_calls: AtomicUsize,
    pub(super) stop_typing_calls: AtomicUsize,
}

#[derive(Default)]
pub(super) struct TelegramRecordingChannel {
    pub(super) sent_messages: tokio::sync::Mutex<Vec<String>>,
    /// Every `apply_allowed_senders` call, in order. A `std::sync::Mutex`
    /// because the trait method is sync.
    pub(super) applied_allowlists: std::sync::Mutex<Vec<Vec<String>>>,
}

#[async_trait::async_trait]
impl Channel for TelegramRecordingChannel {
    // Mirrors the real channel: the instructions come from the channel
    // impl now, not from a `match` on its name, so a stub that claims the
    // name must also claim the capability.
    fn delivery_instructions(&self, workspace: &std::path::Path) -> Option<String> {
        Some(crate::channels::telegram::telegram_delivery_instructions(
            workspace,
        ))
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

    // Exact-or-wildcard match against the most recently applied list — the
    // same rule the real single-form channels (Lark, Discord, Slack) use —
    // so a dispatch-level test can exercise a real revoke/re-add cycle
    // without a live transport. Unrestricted until something is actually
    // applied: unlike a real channel, this fake has no boot-time list to
    // fall back on, and most tests using it never touch allowlists at all.
    fn is_sender_still_allowed(&self, msg: &traits::ChannelMessage) -> bool {
        self.applied_allowlists
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .is_none_or(|list| list.iter().any(|u| u == "*" || u == &msg.sender))
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

    async fn start_typing(&self, _recipient: &str, _thread_ts: Option<&str>) -> anyhow::Result<()> {
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

    async fn start_typing(&self, _recipient: &str, _thread_ts: Option<&str>) -> anyhow::Result<()> {
        self.start_typing_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        self.stop_typing_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct HistoryCaptureProvider {
    pub(super) calls: std::sync::Mutex<Vec<Vec<(String, String)>>>,
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

pub(super) struct NoopMemory;

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

/// A context for dispatch tests driven by a real channel parser: the given
/// channels, one provider cached as the default, nothing persisted.
pub(super) fn dispatch_ctx(
    channels: Vec<Arc<dyn Channel>>,
    provider: Arc<dyn Provider>,
    runtime_config: routing::RuntimeConfigSlot,
) -> Arc<ChannelRuntimeContext> {
    let channels_by_name: HashMap<String, Arc<dyn Channel>> = channels
        .into_iter()
        .map(|channel| (channel.name().to_string(), channel))
        .collect();
    let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    provider_cache_seed.insert("test-provider".to_string(), Arc::clone(&provider));

    Arc::new(ChannelRuntimeContext {
        runtime_config: Arc::new(Mutex::new(runtime_config)),
        channels_by_name: Arc::new(channels_by_name),
        provider,
        default_provider: Arc::new("test-provider".to_string()),
        memory: Arc::new(NoopMemory),
        tools_registry: Arc::new(vec![]),
        observer: Arc::new(NoopObserver),
        system_prompt: Arc::new("test-system-prompt".to_string()),
        guest_system_prompt: Arc::new("test-system-prompt".to_string()),
        model: Arc::new("default-model".to_string()),
        temperature: 0.0,
        auto_save_memory: false,
        max_tool_iterations: 5,
        min_relevance_score: 0.0,
        conversation_histories: Arc::new(Mutex::new(HashMap::new())),
        history_store: None,
        ledger: None,
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
        guest_gate: Arc::new(crate::approval::GuestGate::new(&[], &[])),
    })
}

/// The part of `prompt` from the tool-use protocol on.
pub(super) fn tool_protocol_block(prompt: &str) -> &str {
    prompt
        .split_once("## Tool Use Protocol")
        .map_or("", |(_, block)| block)
}

/// Answers every turn with one fixed reply and keeps the system prompt each
/// turn started from, so a test can pin both what the model was told and what
/// the channel was handed.
pub(super) struct ReplyAndPromptProvider {
    pub(super) reply: String,
    pub(super) system_prompts: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for ReplyAndPromptProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(self.reply.clone())
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone())
            .unwrap_or_default();
        self.system_prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(system);
        Ok(self.reply.clone())
    }
}

pub(super) const GUEST_SENDER: &str = "rantaiclaw_guest";

pub(super) const OWNER_SENDER: &str = "rantaiclaw_owner";

/// A tool that does nothing. Only its name matters to the tests around which
/// tools a turn is handed.
pub(super) struct NamedStubTool(pub(super) &'static str);

#[async_trait::async_trait]
impl Tool for NamedStubTool {
    fn name(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "stub tool"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: "stub ran".to_string(),
            error: None,
        })
    }
}

/// A provider with native tool calling that keeps the tool specs each request
/// carried (`None` when the request carried no tool specs at all).
#[derive(Default)]
pub(super) struct NativeSpecRecorder {
    pub(super) requests: std::sync::Mutex<Vec<Option<Vec<crate::tools::ToolSpec>>>>,
    /// The system prompt each request started from.
    pub(super) system_prompts: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for NativeSpecRecorder {
    fn supports_native_tools(&self) -> bool {
        true
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("ok".to_string())
    }

    async fn chat(
        &self,
        request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.tools.map(<[crate::tools::ToolSpec]>::to_vec));
        self.system_prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(
                request
                    .messages
                    .iter()
                    .find(|m| m.role == "system")
                    .map(|m| m.content.clone())
                    .unwrap_or_default(),
            );
        Ok(crate::providers::ChatResponse {
            usage: None,
            text: Some("ok".to_string()),
            tool_calls: Vec::new(),
        })
    }
}

/// A runtime-defaults slot seeded with `preset` and `guest_gate`, so a turn
/// reads them from the reloaded state the way a daemon that has applied its
/// config does.
pub(super) fn seeded_defaults_slot(
    preset: crate::approval::policy_writer::PolicyPreset,
    guest_gate: crate::approval::GuestGate,
) -> routing::RuntimeConfigSlot {
    routing::RuntimeConfigSlot {
        state: Some(routing::RuntimeConfigState {
            defaults: ChannelRuntimeDefaults {
                default_provider: "test-provider".to_string(),
                model: "default-model".to_string(),
                temperature: 0.0,
                api_key: None,
                api_url: None,
                reliability: crate::config::ReliabilityConfig::default(),
                approval_owners: Arc::new(vec![OWNER_SENDER.to_string()]),
                guest_gate: Arc::new(guest_gate),
                allowed_commands: Arc::new(Vec::new()),
                autonomy_level: crate::security::AutonomyLevel::Supervised,
                autonomy_preset: preset,
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
    }
}

pub(super) fn gate_of(tools: &[&str]) -> crate::approval::GuestGate {
    let tools: Vec<String> = tools.iter().map(|t| (*t).to_string()).collect();
    crate::approval::GuestGate::new(&tools, &[])
}

/// A dispatch context whose start-up gate lists `startup_gate` and whose
/// reloaded defaults list `reloaded_gate` under `preset`, over a registry of
/// stub tools named `registry`. `guest_base` is the guest prompt as built at
/// start-up.
pub(super) fn guest_turn_context(
    provider: Arc<dyn Provider>,
    registry: &[&'static str],
    startup_gate: &[&str],
    reloaded_gate: &[&str],
    preset: crate::approval::policy_writer::PolicyPreset,
    guest_base: &str,
) -> Arc<ChannelRuntimeContext> {
    let channel: Arc<dyn Channel> = Arc::new(RecordingChannel::default());
    let mut ctx = dispatch_ctx(
        vec![channel],
        provider,
        seeded_defaults_slot(preset, gate_of(reloaded_gate)),
    );
    {
        let inner = Arc::get_mut(&mut ctx).expect("the context is not shared yet");
        inner.tools_registry = Arc::new(
            registry
                .iter()
                .map(|name| Box::new(NamedStubTool(name)) as Box<dyn Tool>)
                .collect(),
        );
        inner.approval_owners = Arc::new(vec![OWNER_SENDER.to_string()]);
        inner.guest_gate = Arc::new(gate_of(startup_gate));
        inner.guest_system_prompt = Arc::new(guest_base.to_string());
    }
    ctx
}

/// Replaces the guest ceiling of the reloaded defaults, the way a config edit
/// does between two messages.
pub(super) fn reload_guest_gate(ctx: &ChannelRuntimeContext, tools: &[&str]) {
    let mut slot = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
    slot.state
        .as_mut()
        .expect("the slot is seeded")
        .defaults
        .guest_gate = Arc::new(gate_of(tools));
}

pub(super) async fn send_guest_message(ctx: &Arc<ChannelRuntimeContext>, sender: &str, id: &str) {
    process_channel_message(
        Arc::clone(ctx),
        traits::ChannelMessage {
            sender_aliases: Vec::new(),
            id: id.to_string(),
            sender: sender.to_string(),
            reply_target: "chat-guest-turn".to_string(),
            content: "hello".to_string(),
            channel: "test-channel".to_string(),
            timestamp: 1,
            thread_ts: None,
            reply_anchor: None,
            is_direct: true,
        },
        CancellationToken::new(),
    )
    .await;
}

pub(super) fn prompts_seen_by(provider: &ReplyAndPromptProvider) -> Vec<String> {
    provider
        .system_prompts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The guest prompt as the runtime builds it at start-up for a guest allowed
/// `tools`: the same builder call, with the descriptions of those tools.
pub(super) fn startup_guest_prompt(workspace: &std::path::Path, tools: &[(&str, &str)]) -> String {
    build_system_prompt_with_mode(
        workspace,
        "model",
        tools,
        &[],
        None,
        None,
        false,
        crate::config::SkillsPromptInjectionMode::Full,
        true,
    )
}
