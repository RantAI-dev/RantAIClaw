//! Channel subsystem for messaging platform integrations.
//!
//! This module provides the multi-channel messaging infrastructure that connects
//! RantaiClaw to external platforms. Each channel implements the [`Channel`] trait
//! defined in [`traits`], which provides a uniform interface for sending messages,
//! listening for incoming messages, health checking, and typing indicators.
//!
//! Channels are instantiated by [`start_channels`] based on the runtime configuration.
//! The subsystem manages per-sender conversation history, concurrent message processing
//! with configurable parallelism, and exponential-backoff reconnection for resilience.
//!
//! # Extension
//!
//! To add a new channel, implement [`Channel`] in a new submodule and wire it into
//! [`start_channels`]. See `AGENTS.md` §7.2 for the full change playbook.

pub mod approval_relay;
pub mod auto_start_state;
pub mod cli;
pub mod conversation;
pub mod dingtalk;
pub mod discord;
pub mod email_channel;
pub mod format;
mod history_store;
pub mod imessage;
pub mod irc;
#[cfg(feature = "channel-lark")]
pub mod lark;
pub mod linq;
#[cfg(feature = "channel-matrix")]
pub mod matrix;
pub mod mattermost;
pub mod nextcloud_talk;
pub mod pairing;
pub mod qq;
pub mod qr_terminal;
pub mod signal;
pub mod slack;
pub mod telegram;
pub mod traits;
pub mod whatsapp;
#[cfg(feature = "whatsapp-web")]
pub mod whatsapp_storage;
#[cfg(feature = "whatsapp-web")]
pub mod whatsapp_web;

pub use cli::CliChannel;
pub use dingtalk::DingTalkChannel;
pub use discord::DiscordChannel;
pub use email_channel::EmailChannel;
pub use imessage::IMessageChannel;
pub use irc::IrcChannel;
#[cfg(feature = "channel-lark")]
pub use lark::LarkChannel;
pub use linq::LinqChannel;
#[cfg(feature = "channel-matrix")]
pub use matrix::MatrixChannel;
pub use mattermost::MattermostChannel;
pub use nextcloud_talk::NextcloudTalkChannel;
pub use qq::QQChannel;
pub use signal::SignalChannel;
pub use slack::SlackChannel;
pub use telegram::TelegramChannel;
pub use traits::{Channel, SendMessage};
pub use whatsapp::WhatsAppChannel;
#[cfg(feature = "whatsapp-web")]
pub use whatsapp_web::WhatsAppWebChannel;

use crate::agent::loop_::{build_tool_instructions, run_tool_call_loop};
use crate::config::Config;
use crate::memory::{self, Memory};
use crate::observability::{self, Observer};
use crate::providers::{self, ChatMessage, Provider};
use crate::runtime;
use crate::security::SecurityPolicy;
use crate::tools::{self, Tool};
use crate::util::truncate_with_ellipsis;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio_util::sync::CancellationToken;

/// Per-sender conversation history for channel messages.
type ConversationHistoryMap = Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>;
/// Maximum history messages to keep per sender.
const MAX_CHANNEL_HISTORY: usize = 50;
/// Minimum user-message length (in chars) for auto-save to memory.
/// Messages shorter than this (e.g. "ok", "thanks") are not stored,
/// reducing noise in memory recall.
const AUTOSAVE_MIN_MESSAGE_CHARS: usize = 20;

/// Maximum characters per injected workspace file (matches `OpenClaw` default).
const BOOTSTRAP_MAX_CHARS: usize = 20_000;

const DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS: u64 = 2;
const DEFAULT_CHANNEL_MAX_BACKOFF_SECS: u64 = 60;
const MIN_CHANNEL_MESSAGE_TIMEOUT_SECS: u64 = 30;
/// Default timeout for processing a single channel message (LLM + tools).
/// Used as fallback when not configured in channels_config.message_timeout_secs.
const CHANNEL_MESSAGE_TIMEOUT_SECS: u64 = 300;
/// Cap timeout scaling so large max_tool_iterations values do not create unbounded waits.
const CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP: u64 = 4;
const CHANNEL_PARALLELISM_PER_CHANNEL: usize = 4;
const CHANNEL_MIN_IN_FLIGHT_MESSAGES: usize = 8;
const CHANNEL_MAX_IN_FLIGHT_MESSAGES: usize = 64;
const CHANNEL_TYPING_REFRESH_INTERVAL_SECS: u64 = 4;

/// Recorded in place of an assistant turn when a reply could not be delivered,
/// or when the turn ended in a timeout, an error, or a cancellation.
///
/// Without it the user turn appended at the start of the turn stays unpaired,
/// and `normalize_cached_channel_turns` merges consecutive user turns into one
/// blob — so the model sees the retried question concatenated onto the failed
/// one with no marker that the first attempt died.
const UNDELIVERED_TURN_MARKER: &str = "(the previous reply was not delivered)";
const TIMED_OUT_TURN_MARKER: &str = "(the previous attempt timed out)";
const FAILED_TURN_MARKER: &str = "(the previous attempt failed)";

/// Backstop for waiting on a previous in-flight turn to signal completion.
///
/// `CompletionGuard` releases the signal even on a panic, so this should never
/// fire. It exists because the wait sits on the path that drains the shared
/// message bus for **all** channels: if it ever hangs, every platform goes quiet
/// with nothing logged. Generous enough not to cut a legitimately slow turn
/// short — the turn budget bounds that separately.
const IN_FLIGHT_COMPLETION_WAIT_TIMEOUT_SECS: u64 = 120;
const IN_FLIGHT_COMPLETION_WAIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(IN_FLIGHT_COMPLETION_WAIT_TIMEOUT_SECS);
const CHANNEL_HEALTH_HEARTBEAT_SECS: u64 = 30;

/// How long an unanswered in-chat approval waits before auto-denying.
///
/// Shared by the shell and tool registries so the two cannot drift, and read by
/// `approval_relay::auto_deny_line` so the prompt states this value rather than
/// a literal. The prompt used to promise "Auto-deny in 5 min" while the shell
/// registry behind it had no deadline at all.
const CHANNEL_APPROVAL_DEADLINE_SECS: u64 = 300;
const CHANNEL_APPROVAL_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(CHANNEL_APPROVAL_DEADLINE_SECS);
const MODEL_CACHE_PREVIEW_LIMIT: usize = 10;
const MEMORY_CONTEXT_MAX_ENTRIES: usize = 4;
const MEMORY_CONTEXT_ENTRY_MAX_CHARS: usize = 800;
const MEMORY_CONTEXT_MAX_CHARS: usize = 4_000;
const CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES: usize = 12;
const CHANNEL_HISTORY_COMPACT_CONTENT_CHARS: usize = 600;

type ProviderCacheMap = Arc<Mutex<HashMap<String, Arc<dyn Provider>>>>;
type RouteSelectionMap = Arc<Mutex<HashMap<String, ChannelRouteSelection>>>;

fn effective_channel_message_timeout_secs(configured: u64) -> u64 {
    configured.max(MIN_CHANNEL_MESSAGE_TIMEOUT_SECS)
}

fn channel_message_timeout_budget_secs(
    message_timeout_secs: u64,
    max_tool_iterations: usize,
) -> u64 {
    let iterations = max_tool_iterations.max(1) as u64;
    let scale = iterations.min(CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP);
    message_timeout_secs.saturating_mul(scale)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChannelRouteSelection {
    provider: String,
    model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChannelRuntimeCommand {
    ShowProviders,
    SetProvider(String),
    ShowModel,
    SetModel(String),
}

#[derive(Debug, Clone)]
struct ChannelRuntimeDefaults {
    default_provider: String,
    model: String,
    temperature: f64,
    api_key: Option<String>,
    api_url: Option<String>,
    reliability: crate::config::ReliabilityConfig,
    /// Senders authorized to approve over a channel
    /// (`[channels_config] approval_owners`). Reloaded from disk so owner
    /// changes apply without a `channels run` restart. `Arc` keeps `.clone()`
    /// cheap.
    approval_owners: Arc<Vec<String>>,
    /// Per-role capability ceiling for non-owners. Rebuilt from config on
    /// reload so guest tool/command allowances apply without a restart.
    guest_gate: Arc<crate::approval::GuestGate>,
    /// Autonomy shell-command basenames (`[autonomy] allowed_commands`). Synced
    /// into the live `SecurityPolicy` runtime allowlist on reload so
    /// owner-added commands take effect without a restart.
    allowed_commands: Arc<Vec<String>>,
    /// Autonomy level (`[autonomy] level`). Hot-swapped into the live
    /// `SecurityPolicy` on reload so `rantaiclaw autonomy <preset>` (e.g.
    /// Off → Full) applies without a `channels run`/daemon restart.
    autonomy_level: crate::security::AutonomyLevel,
    /// Active approval preset, refreshed on reload. The channel system prompt
    /// is built once at startup (it reads bootstrap files and skills off disk),
    /// so without carrying this the prompt kept describing whatever preset was
    /// active when the daemon started — the gate moved, the briefing did not.
    autonomy_preset: crate::approval::policy_writer::PolicyPreset,
    /// Per-channel sender allowlists, keyed by the same lowercase channel name
    /// used by `channels_by_name`. Reloaded from disk so a console or CLI
    /// allowlist edit reaches a running listener without restarting the daemon —
    /// which, since the daemon hosts the gateway, used to kill the request that
    /// made the edit.
    allowlists: Arc<HashMap<String, Vec<String>>>,
    /// Per-message behaviour knobs that used to be read from the boot-time
    /// `ctx`, so editing them in `config.toml` did nothing until a restart even
    /// though the reload reported success. They carry no gate semantics — the
    /// timeout bounds a turn, the iteration cap bounds a tool loop — so they
    /// live here purely to close the "reload said applied, nothing changed" gap.
    message_timeout_secs: u64,
    max_tool_iterations: usize,
    auto_save_memory: bool,
    min_relevance_score: f64,
    /// `[channels_config] autonomous_tools`. Reloaded so an operator can re-arm
    /// the in-chat approval gate without a restart. This is the security-relevant
    /// direction: `false` means "gate tools", and before this it was read once at
    /// startup, so turning the gate back **on** was reported as applied and did
    /// nothing. The `ApprovalManager` itself is now always constructed at boot,
    /// so flipping this flag costs nothing per message.
    autonomous_tools: bool,
    /// Per-channel `mention_only`, carried only so reload can detect an edit it
    /// cannot apply and say so. See `channel_mention_only`.
    mention_only: Arc<HashMap<String, bool>>,
}

/// Per-channel sender allowlists, keyed by `Channel::name()`.
///
/// The field each channel stores its allowlist in differs (`allowed_users`,
/// `allowed_from`, `allowed_numbers`, `allowed_senders`, `allowed_contacts`), so
/// this mirrors the field choices in `pairing::apply_pairing` exactly rather than
/// picking different ones.
///
/// It is a second copy of that mapping, which is duplication this subsystem has
/// too much of already. Consolidating it belongs with the other cross-file
/// allowlist work (`plans/129-…`), which owns `pairing.rs`; doing it here would
/// put two plans in one file. Until then: **change both or neither.**
/// Per-channel `mention_only`, keyed like `channel_allowlists`.
///
/// Only the three channels whose config carries the flag appear. Unlike the
/// allowlists this is **not** applied on reload: `mention_only` is passed into
/// the channel constructors and lives inside the channel objects, so applying it
/// live needs a `Channel` trait method, which is a cross-file change this plan
/// does not own. It is tracked here purely so a reload can *tell the operator*
/// that their edit needs a restart instead of reporting success and doing
/// nothing.
fn channel_mention_only(cc: &crate::config::ChannelsConfig) -> HashMap<String, bool> {
    let mut out = HashMap::new();
    if let Some(c) = cc.telegram.as_ref() {
        out.insert("telegram".to_string(), c.mention_only);
    }
    if let Some(c) = cc.discord.as_ref() {
        out.insert("discord".to_string(), c.mention_only);
    }
    if let Some(c) = cc.mattermost.as_ref() {
        out.insert("mattermost".to_string(), c.mention_only.unwrap_or(false));
    }
    out
}

fn channel_allowlists(cc: &crate::config::ChannelsConfig) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    let mut put = |name: &str, list: Option<&Vec<String>>| {
        if let Some(list) = list {
            out.insert(name.to_string(), list.clone());
        }
    };
    put("telegram", cc.telegram.as_ref().map(|c| &c.allowed_users));
    put("discord", cc.discord.as_ref().map(|c| &c.allowed_users));
    put("slack", cc.slack.as_ref().map(|c| &c.allowed_users));
    put(
        "mattermost",
        cc.mattermost.as_ref().map(|c| &c.allowed_users),
    );
    put("matrix", cc.matrix.as_ref().map(|c| &c.allowed_users));
    put("irc", cc.irc.as_ref().map(|c| &c.allowed_users));
    put("lark", cc.lark.as_ref().map(|c| &c.allowed_users));
    put("dingtalk", cc.dingtalk.as_ref().map(|c| &c.allowed_users));
    put("qq", cc.qq.as_ref().map(|c| &c.allowed_users));
    put(
        "nextcloud_talk",
        cc.nextcloud_talk.as_ref().map(|c| &c.allowed_users),
    );
    put("signal", cc.signal.as_ref().map(|c| &c.allowed_from));
    put("whatsapp", cc.whatsapp.as_ref().map(|c| &c.allowed_numbers));
    put("linq", cc.linq.as_ref().map(|c| &c.allowed_senders));
    put(
        "imessage",
        cc.imessage.as_ref().map(|c| &c.allowed_contacts),
    );
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfigFileStamp {
    modified: SystemTime,
    len: u64,
}

/// What one channel runtime knows about its config file: the applied state, plus
/// warn-once latches for the two paths that used to fail in total silence.
///
/// The latches live here rather than in a `static` so they cannot couple one
/// test to another — which is the defect that removing the global store fixed.
#[derive(Default)]
struct RuntimeConfigSlot {
    state: Option<RuntimeConfigState>,
    /// Set once `runtime_defaults_snapshot` has reported taking its synthesised
    /// fallback. That fallback hands the model a *guessed* autonomy preset the
    /// gate is not enforcing, so it must be visible — but it is consulted per
    /// message, so it must not be visible once per message.
    fallback_warned: bool,
    /// Set once an unreadable/unstattable config file has been reported. Cleared
    /// on the next successful stat so a later outage is reported again.
    stamp_error_warned: bool,
}

#[derive(Debug, Clone)]
struct RuntimeConfigState {
    defaults: ChannelRuntimeDefaults,
    last_applied_stamp: Option<ConfigFileStamp>,
    /// Reason the most recent reload could not apply the new provider (e.g. no
    /// usable API key). `Some` means the runtime kept the previous provider.
    last_reload_error: Option<String>,
}

/// The most recent reload failure reason for `config_path`, if the runtime kept
/// the previous provider instead of swapping to a broken one. Exposed so an
/// operator surface can report why a channel didn't follow a provider switch.
#[cfg_attr(not(test), allow(dead_code))]
fn last_reload_error(ctx: &ChannelRuntimeContext) -> Option<String> {
    ctx.runtime_config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .state
        .as_ref()
        .and_then(|s| s.last_reload_error.clone())
}

const SYSTEMD_STATUS_ARGS: [&str; 3] = ["--user", "is-active", "rantaiclaw.service"];
const SYSTEMD_RESTART_ARGS: [&str; 3] = ["--user", "restart", "rantaiclaw.service"];
const OPENRC_STATUS_ARGS: [&str; 2] = ["rantaiclaw", "status"];
const OPENRC_RESTART_ARGS: [&str; 2] = ["rantaiclaw", "restart"];

#[derive(Clone)]
struct ChannelRuntimeContext {
    /// Reloaded config state for *this* runtime's config file.
    ///
    /// Was a process-global `HashMap<PathBuf, _>` keyed by config path. Entries
    /// were inserted and never removed, a gateway and a channel runtime in one
    /// process shared and clobbered each other's entry, and every test that
    /// touched it was order-dependent on every other. One context owns one
    /// state; `None` means nothing has been loaded yet, which is the same
    /// condition the old "no entry for this path" fallback keyed on.
    runtime_config: Arc<Mutex<RuntimeConfigSlot>>,
    channels_by_name: Arc<HashMap<String, Arc<dyn Channel>>>,
    provider: Arc<dyn Provider>,
    default_provider: Arc<String>,
    memory: Arc<dyn Memory>,
    tools_registry: Arc<Vec<Box<dyn Tool>>>,
    observer: Arc<dyn Observer>,
    system_prompt: Arc<String>,
    model: Arc<String>,
    temperature: f64,
    auto_save_memory: bool,
    max_tool_iterations: usize,
    min_relevance_score: f64,
    conversation_histories: ConversationHistoryMap,
    /// Durable backing for `conversation_histories`. `Some` persists each
    /// in-memory mutation to `brain.db` (and seeds the map at startup) so
    /// conversation threads survive daemon restarts. `None` means persistence
    /// is disabled (non-sqlite memory backends, or an open failure) and history
    /// stays in-memory only, exactly as before.
    history_store: Option<Arc<history_store::ChannelHistoryStore>>,
    provider_cache: ProviderCacheMap,
    route_overrides: RouteSelectionMap,
    api_key: Option<String>,
    api_url: Option<String>,
    reliability: Arc<crate::config::ReliabilityConfig>,
    provider_runtime_options: providers::ProviderRuntimeOptions,
    workspace_dir: Arc<PathBuf>,
    message_timeout_secs: u64,
    interrupt_on_new_message: bool,
    multimodal: crate::config::MultimodalConfig,
    /// Shared security policy. Carries the runtime allowlist + bound
    /// `PendingApprovals` registry. Read by the approval-reply parser
    /// before each inbound message is routed to the agent.
    security: Arc<crate::security::SecurityPolicy>,
    /// Per-tool approval gate for polling channels. `Some` (default) means
    /// tools that need approval at the current autonomy level are denied —
    /// polling channels do NOT run tools unattended. `None` only when
    /// `[channels_config] autonomous_tools = true`, restoring the
    /// run-everything behaviour. The shared session-allowlist stays empty
    /// (channels never grant interactive approval), so it's safe to share
    /// one manager across senders.
    channel_approval: Option<Arc<crate::approval::ApprovalManager>>,
    /// Senders authorized to APPROVE tool calls over a channel
    /// (`[channels_config] approval_owners`). Empty ⇒ nobody can approve, so the
    /// in-chat relay is never offered and approval-required tools auto-deny.
    /// Shared with the dispatch loop's reply parser.
    approval_owners: Arc<Vec<String>>,
    /// Dedicated registry for in-chat whole-tool approvals (Layer A). Separate
    /// from the shell allowlist `PendingApprovals` on `security`. The per-message
    /// [`ChatRelayApprovalBackend`] registers + awaits here; the dispatch loop's
    /// `try_handle_tool_reply` resolves it when an owner replies.
    tool_approvals: Arc<crate::security::PendingApprovals>,
    /// Per-role capability ceiling applied to non-owner ("guest") senders. The
    /// ceiling is role-based (same for every guest), so it's built once from
    /// config; a turn uses it only when the sender isn't an owner. Owners get
    /// the full toolset.
    guest_gate: Arc<crate::approval::GuestGate>,
}

#[derive(Clone)]
struct InFlightSenderTaskState {
    task_id: u64,
    cancellation: CancellationToken,
    completion: Arc<InFlightTaskCompletion>,
}

struct InFlightTaskCompletion {
    done: AtomicBool,
    notify: tokio::sync::Notify,
}

impl InFlightTaskCompletion {
    fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        // Register the waiter BEFORE re-checking `done`.
        //
        // `notify_waiters()` stores no permit — it only wakes waiters that are
        // already registered — and `notified()` does not register until first
        // polled. Checking `done` and then awaiting left a window where a
        // `mark_done()` on another worker landed between the two and was lost,
        // parking this sender's next message forever.
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.done.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

/// Marks an [`InFlightTaskCompletion`] done on drop, including on unwind.
///
/// `mark_done()` used to be the last statement of the worker closure, so a panic
/// anywhere in the message path — provider, tool loop, renderer, a channel's
/// `send` — skipped it. The next message from that sender then waited on a
/// signal that would never come, and the worker's semaphore permit was never
/// released. After enough of those the dispatch loop stops draining its queue
/// and **every** channel goes quiet, with nothing logging a deadlock because the
/// task never finishes.
struct CompletionGuard(Arc<InFlightTaskCompletion>);

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.0.mark_done();
    }
}

fn conversation_memory_key(msg: &traits::ChannelMessage) -> String {
    format!("{}_{}_{}", msg.channel, msg.sender, msg.id)
}

/// The conversation this message belongs to, for history and `/model` routing.
///
/// Keyed by the **chat**, not the person. It used to be `channel_sender`, so one
/// person's private DM, every group the bot shared with them and every forum
/// topic collapsed into a single thread — turns from a private conversation were
/// injected verbatim into the prompt when that same person next spoke in a
/// public group, and persisted to `brain.db` so it survived restarts.
///
/// `reply_target` is the chat id on every channel that has one (Telegram
/// `chat_id[:thread_id]`, Discord/Slack `channel_id`), and it is stable per
/// conversation — checked against Telegram, Discord, Slack and Matrix before
/// this was adopted. Matrix sets `reply_target` to the sender, but a Matrix
/// channel is pinned to one configured room, so there is only ever one
/// conversation there and nothing merges.
///
/// Route overrides use this same value, so a `/model` pin follows the
/// conversation rather than following the person into every chat they are in.
fn conversation_history_key(msg: &traits::ChannelMessage) -> String {
    conversation::ConversationKey::new(&msg.channel, &msg.reply_target)
        .in_thread(msg.thread_ts.as_deref())
        .resolve()
}

fn interruption_scope_key(msg: &traits::ChannelMessage) -> String {
    format!("{}_{}_{}", msg.channel, msg.reply_target, msg.sender)
}

fn channel_delivery_instructions(channel_name: &str) -> Option<&'static str> {
    match channel_name {
        "telegram" => Some(
            "When responding on Telegram, include media markers for files or URLs that should be sent as attachments. Use one marker per attachment with this exact syntax: [IMAGE:<path-or-url>], [DOCUMENT:<path-or-url>], [VIDEO:<path-or-url>], [AUDIO:<path-or-url>], or [VOICE:<path-or-url>]. Keep normal user-facing text outside markers and never wrap markers in code fences.",
        ),
        _ => None,
    }
}

/// Appended to a channel system prompt when the sender is an approval owner
/// (`can_approve` is true for them). Without it, a cautious model self-refuses
/// owner-only tools (e.g. `manage_permissions`, `issue_pairing_code`) because
/// their descriptions say "owner-only" and nothing tells the model the sender
/// IS an owner — even though the runtime has already authorized them. This does
/// NOT widen any permission: the runtime gate stays the sole enforcer; it only
/// stops the model from falsely declining an already-authorized request.
const CHANNEL_OWNER_CONTEXT: &str = "The person you are talking to is a verified OWNER of this bot: \
the runtime has already authorized them for owner-privileged actions. When they ask you to use an \
owner-only tool (for example manage_permissions or issue_pairing_code), use it on their behalf — do \
NOT refuse on the grounds that the tool is owner-only.";

/// Announce-capable channels — the set `deliver_if_configured`
/// (`src/cron/scheduler.rs`) can push a scheduled agent job's output to. Keep in
/// sync with that match.
pub(crate) fn channel_supports_announce_delivery(channel_name: &str) -> bool {
    matches!(
        channel_name,
        "telegram" | "discord" | "slack" | "mattermost"
    )
}

/// Guidance so the agent, when the user asks for a scheduled/recurring message or
/// reminder, creates a `cron_add` agent job whose `delivery` routes the output
/// back to THIS chat — and nowhere else. Only emitted for channels the scheduler
/// can actually deliver to.
fn channel_cron_delivery_instructions(channel_name: &str, reply_target: &str) -> Option<String> {
    if !channel_supports_announce_delivery(channel_name) {
        return None;
    }
    Some(format!(
        "You are talking to this user on the '{channel_name}' channel (their delivery \
address is '{reply_target}'). When they ask you to send them a message, reminder, or \
report on a schedule (e.g. \"message me every morning\"), create it with the cron_add \
tool as an agent job and set delivery to route the output back to THEM here: \
delivery = {{ \"mode\": \"announce\", \"channel\": \"{channel_name}\", \"to\": \"{reply_target}\" }}. \
The scheduled output is delivered only to this chat — it does not appear anywhere else. \
Do not ask the user for their chat id; use the address above."
    ))
}

fn build_channel_system_prompt(
    base_prompt: &str,
    channel_name: &str,
    reply_target: &str,
    is_owner: bool,
) -> String {
    let mut prompt = if let Some(instructions) = channel_delivery_instructions(channel_name) {
        if base_prompt.is_empty() {
            instructions.to_string()
        } else {
            format!("{base_prompt}\n\n{instructions}")
        }
    } else {
        base_prompt.to_string()
    };

    if let Some(cron) = channel_cron_delivery_instructions(channel_name, reply_target) {
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str(&cron);
    }

    if is_owner {
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str(CHANNEL_OWNER_CONTEXT);
    }

    prompt
}

fn normalize_cached_channel_turns(turns: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut normalized = Vec::with_capacity(turns.len());
    let mut expecting_user = true;

    for turn in turns {
        match (expecting_user, turn.role.as_str()) {
            (true, "user") => {
                normalized.push(turn);
                expecting_user = false;
            }
            (false, "assistant") => {
                normalized.push(turn);
                expecting_user = true;
            }
            // Interrupted channel turns can produce consecutive user messages
            // (no assistant persisted yet). Merge instead of dropping.
            (false, "user") | (true, "assistant") => {
                if let Some(last_turn) = normalized.last_mut() {
                    if !turn.content.is_empty() {
                        if !last_turn.content.is_empty() {
                            last_turn.content.push_str("\n\n");
                        }
                        last_turn.content.push_str(&turn.content);
                    }
                }
            }
            // Any other role (`system`, `tool`, …). Nothing writes one to this
            // store today, so this is a trap rather than a live bug — but the
            // store is a general message vector, and a silent drop here would be
            // permanent after the next compaction. Say what was lost.
            (_, role) => {
                tracing::debug!(
                    role = %role,
                    "dropping cached channel turn with an unexpected role"
                );
            }
        }
    }

    normalized
}

fn supports_runtime_model_switch(channel_name: &str) -> bool {
    matches!(channel_name, "telegram" | "discord")
}

fn parse_runtime_command(channel_name: &str, content: &str) -> Option<ChannelRuntimeCommand> {
    if !supports_runtime_model_switch(channel_name) {
        return None;
    }

    let trimmed = content.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let command_token = parts.next()?;
    let base_command = command_token
        .split('@')
        .next()
        .unwrap_or(command_token)
        .to_ascii_lowercase();

    match base_command.as_str() {
        "/models" => {
            if let Some(provider) = parts.next() {
                Some(ChannelRuntimeCommand::SetProvider(
                    provider.trim().to_string(),
                ))
            } else {
                Some(ChannelRuntimeCommand::ShowProviders)
            }
        }
        "/model" => {
            let model = parts.collect::<Vec<_>>().join(" ").trim().to_string();
            if model.is_empty() {
                Some(ChannelRuntimeCommand::ShowModel)
            } else {
                Some(ChannelRuntimeCommand::SetModel(model))
            }
        }
        _ => None,
    }
}

fn resolve_provider_alias(name: &str) -> Option<String> {
    let candidate = name.trim();
    if candidate.is_empty() {
        return None;
    }

    let providers_list = providers::list_providers();
    for provider in providers_list {
        if provider.name.eq_ignore_ascii_case(candidate)
            || provider
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(candidate))
        {
            return Some(provider.name.to_string());
        }
    }

    None
}

fn resolved_default_provider(config: &Config) -> String {
    config
        .default_provider
        .clone()
        .unwrap_or_else(|| "openrouter".to_string())
}

fn resolved_default_model(config: &Config) -> String {
    config
        .default_model
        .clone()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string())
}

fn runtime_defaults_from_config(config: &Config) -> ChannelRuntimeDefaults {
    ChannelRuntimeDefaults {
        default_provider: resolved_default_provider(config),
        model: resolved_default_model(config),
        temperature: config.default_temperature,
        api_key: config.api_key.clone(),
        api_url: config.api_url.clone(),
        reliability: config.reliability.clone(),
        approval_owners: Arc::new(config.channels_config.approval_owners.clone()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            config.autonomy.auto_approve.clone(),
            &config.channels_config.guest_allowed_tools,
            &config.channels_config.guest_allowed_commands,
        )),
        allowed_commands: Arc::new(config.autonomy.allowed_commands.clone()),
        autonomy_level: config.autonomy.level,
        autonomy_preset: crate::approval::policy_writer::preset_for_autonomy(&config.autonomy),
        allowlists: Arc::new(channel_allowlists(&config.channels_config)),
        message_timeout_secs: effective_channel_message_timeout_secs(
            config.channels_config.message_timeout_secs,
        ),
        max_tool_iterations: config.agent.max_tool_iterations,
        auto_save_memory: config.memory.auto_save,
        min_relevance_score: config.memory.min_relevance_score,
        autonomous_tools: config.channels_config.autonomous_tools,
        mention_only: Arc::new(channel_mention_only(&config.channels_config)),
    }
}

fn runtime_config_path(ctx: &ChannelRuntimeContext) -> Option<PathBuf> {
    ctx.provider_runtime_options
        .rantaiclaw_dir
        .as_ref()
        .map(|dir| dir.join("config.toml"))
}

fn runtime_defaults_snapshot(ctx: &ChannelRuntimeContext) -> ChannelRuntimeDefaults {
    {
        let mut slot = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = slot.state.as_ref() {
            return state.defaults.clone();
        }
        // No state loaded: everything below is synthesised. In production
        // `start_channels` always seeds this, so reaching here means the runtime
        // is answering messages against a *guessed* autonomy preset that the
        // live gate is not enforcing. Say so — but once, since this is consulted
        // per message.
        if !slot.fallback_warned {
            slot.fallback_warned = true;
            tracing::warn!(
                "Channel runtime has no loaded config state; falling back to \
                 boot-time context values and a guessed autonomy preset. The \
                 enforced policy is whatever SecurityPolicy holds, not this."
            );
        }
    }

    // Fallback only when nothing has been loaded for this runtime. It is seeded
    // at startup in `start_channels`, so in production the snapshot above is
    // authoritative; this mirrors the startup `ctx` fields for the ad-hoc/test
    // path.
    ChannelRuntimeDefaults {
        default_provider: ctx.default_provider.as_str().to_string(),
        model: ctx.model.as_str().to_string(),
        temperature: ctx.temperature,
        api_key: ctx.api_key.clone(),
        api_url: ctx.api_url.clone(),
        reliability: (*ctx.reliability).clone(),
        approval_owners: Arc::clone(&ctx.approval_owners),
        guest_gate: Arc::clone(&ctx.guest_gate),
        allowed_commands: Arc::new(Vec::new()),
        // Empty on the fallback path: this branch has no config to read
        // allowlists from, and an empty map means "apply nothing", so every
        // channel keeps the list it was constructed with. Inventing entries
        // here would let a fallback *widen* a gate, which is the opposite of
        // what a fallback should be able to do.
        allowlists: Arc::new(HashMap::new()),
        // Behaviour knobs mirror the boot-time `ctx`, which is exactly what this
        // fallback is for. Unlike the gate-bearing fields above these carry no
        // authority, so mirroring them cannot widen anything.
        message_timeout_secs: ctx.message_timeout_secs,
        max_tool_iterations: ctx.max_tool_iterations,
        auto_save_memory: ctx.auto_save_memory,
        min_relevance_score: ctx.min_relevance_score,
        // The fallback must not be the permissive answer: `false` keeps the gate
        // armed. A path with no config to read may not decide that tools run
        // unattended.
        autonomous_tools: false,
        // Empty: with no config to compare against, the reload has nothing to
        // report a divergence from.
        mention_only: Arc::new(HashMap::new()),
        autonomy_level: ctx.security.effective_autonomy(),
        // Fallback path only (the store has no entry — ad-hoc/tests). The
        // live policy carries the enforced level but not `always_ask`, which
        // is what separates Manual from Smart, so Supervised resolves to the
        // stricter of the two. Production goes through
        // `runtime_defaults_from_config`, which has the full config and
        // resolves the preset exactly.
        autonomy_preset: match ctx.security.effective_autonomy() {
            crate::security::AutonomyLevel::ReadOnly => {
                crate::approval::policy_writer::PolicyPreset::Strict
            }
            crate::security::AutonomyLevel::Full => {
                crate::approval::policy_writer::PolicyPreset::Off
            }
            crate::security::AutonomyLevel::Supervised => {
                crate::approval::policy_writer::PolicyPreset::Manual
            }
        },
    }
}

/// Current approval owners from the live runtime-defaults store (or the startup
/// `ctx` fallback). Mirrors `runtime_defaults_snapshot` so `/approve` / `/allow`
/// reply authorization tracks owner changes without a `channels run` restart.
fn live_approval_owners(ctx: &ChannelRuntimeContext) -> Arc<Vec<String>> {
    runtime_defaults_snapshot(ctx).approval_owners
}

/// Stat the config file for change detection.
///
/// Returns `Result` rather than `Option` because both failures used to be
/// swallowed by `.ok()?` and the caller then returned success with nothing
/// logged — the atomic temp-file-and-rename write this project uses makes a
/// briefly-absent config a real occurrence, and an operator whose edit never
/// applied had no way to find out why.
async fn config_file_stamp(path: &Path) -> Result<ConfigFileStamp> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("Failed to stat {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("No modification time for {}", path.display()))?;
    Ok(ConfigFileStamp {
        modified,
        len: metadata.len(),
    })
}

fn decrypt_optional_secret_for_runtime_reload(
    store: &crate::security::SecretStore,
    value: &mut Option<String>,
    field_name: &str,
) -> Result<()> {
    if let Some(raw) = value.clone() {
        if crate::security::SecretStore::is_encrypted(&raw) {
            *value = Some(
                store
                    .decrypt(&raw)
                    .with_context(|| format!("Failed to decrypt {field_name}"))?,
            );
        }
    }
    Ok(())
}

async fn load_runtime_defaults_from_config_file(
    path: &Path,
) -> Result<(ChannelRuntimeDefaults, crate::config::AutonomyConfig)> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let mut parsed: Config =
        toml::from_str(&contents).with_context(|| format!("Failed to parse {}", path.display()))?;
    parsed.config_path = path.to_path_buf();

    if let Some(rantaiclaw_dir) = path.parent() {
        let store = crate::security::SecretStore::new(rantaiclaw_dir, parsed.secrets.encrypt);
        decrypt_optional_secret_for_runtime_reload(&store, &mut parsed.api_key, "config.api_key")?;
    }

    parsed.apply_env_overrides();
    // Hand back the whole `[autonomy]` section, not just the two fields the
    // couriers used to carry: `apply_config` refreshes all eight at once.
    let autonomy = parsed.autonomy.clone();
    Ok((runtime_defaults_from_config(&parsed), autonomy))
}

async fn maybe_apply_runtime_config_update(ctx: &ChannelRuntimeContext) -> Result<()> {
    let Some(config_path) = runtime_config_path(ctx) else {
        return Ok(());
    };

    let stamp = match config_file_stamp(&config_path).await {
        Ok(stamp) => {
            // Recovered: re-arm the latch so a later outage is reported again.
            let mut slot = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
            slot.stamp_error_warned = false;
            stamp
        }
        Err(err) => {
            // Cannot tell whether the config changed, so nothing is applied.
            // This used to return `Ok(())` silently, which is indistinguishable
            // from "nothing to do" — an operator whose edit never landed saw no
            // reason at all. The stamp is deliberately NOT advanced, so a later
            // successful read still applies the edit.
            let mut slot = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
            if !slot.stamp_error_warned {
                slot.stamp_error_warned = true;
                tracing::warn!(
                    path = %config_path.display(),
                    "Cannot stat the channel config file, so config changes are not being \
                     applied: {err:#}"
                );
            }
            return Ok(());
        }
    };

    {
        let slot = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = slot.state.as_ref() {
            if state.last_applied_stamp == Some(stamp) {
                return Ok(());
            }
        }
    }

    let (next_defaults, next_autonomy) =
        load_runtime_defaults_from_config_file(&config_path).await?;
    // Snapshot the currently-applied defaults BEFORE we overwrite the store, so we
    // can tell whether the operator actually changed the provider/model.
    let prev_defaults = runtime_defaults_snapshot(ctx);

    // Apply the non-provider settings FIRST, before trying to build the new
    // provider. They don't depend on the provider, and a safety-motivated
    // autonomy downgrade (or command-allowlist change) must take effect even when
    // the — often unrelated — new provider can't be built. Applying them only on
    // the success path meant `rantaiclaw autonomy off` bundled with a broken
    // provider silently never applied.
    //
    // Swap the whole config half in one write. This previously patched only two
    // of the eight `[autonomy]` fields, via per-field override slots; the other
    // six — forbidden_paths, workspace_only, block_high_risk_commands,
    // require_approval_for_medium_risk, and the two budgets — stayed frozen at
    // whatever was on disk when the daemon started. Operator grants in
    // `runtime_allowlist` (`/allow <cmd> --persist`), the rate-limit window and
    // the approval registry are process state and are deliberately untouched.
    ctx.security.apply_config(&next_autonomy);

    // `mention_only` is constructor-injected into the channel objects, so this
    // reload cannot apply it. Saying nothing would repeat the bug this plan
    // exists to fix — "Applied updated channel runtime config from disk" while
    // the edit did nothing — so name the channel and state that a restart is
    // required. Compared against the previously *applied* snapshot, so the
    // warning fires once per edit rather than on every reload.
    for (name, next_value) in next_defaults.mention_only.iter() {
        let previous = prev_defaults.mention_only.get(name.as_str());
        if previous.is_some_and(|prev| prev != next_value) {
            tracing::warn!(
                channel = %name,
                mention_only = *next_value,
                "mention_only changed on disk but cannot be applied to a running \
                 channel — restart the channel runtime for it to take effect"
            );
        }
    }

    // Push per-channel allowlists into the live channel handles, for the same
    // reason the autonomy swap above happens here: an allowlist change is
    // safety-relevant, so it must apply even when the — usually unrelated — new
    // provider cannot be built. Doing it on the success path only would mean a
    // tightened allowlist silently waited on an API key.
    //
    // Channels that hold their allowlist as a plain `Vec` inherit the no-op
    // default on `Channel::apply_allowed_senders` and keep their boot-time list.
    for (name, allowed) in next_defaults.allowlists.iter() {
        if let Some(channel) = ctx.channels_by_name.get(name.as_str()) {
            channel.apply_allowed_senders(allowed);
        }
    }

    let next_default_provider = match providers::create_resilient_provider_with_options(
        &next_defaults.default_provider,
        next_defaults.api_key.as_deref(),
        next_defaults.api_url.as_deref(),
        &next_defaults.reliability,
        &ctx.provider_runtime_options,
    ) {
        Ok(p) => p,
        Err(err) => {
            // Can't build the new provider (e.g. no usable API key). Keep the
            // working provider + previously-applied defaults; advance the stamp so
            // we don't rebuild-and-fail on every message; record the reason so an
            // operator surface can report it. The operator's fix is itself a config
            // write, which changes the stamp and re-triggers this reload.
            let reason = format!("provider '{}': {err}", next_defaults.default_provider);
            tracing::warn!(
                provider = %next_defaults.default_provider,
                "Config reload kept the previous provider — could not build the new one: {err}"
            );
            let mut guard = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
            let entry = guard.state.get_or_insert_with(|| RuntimeConfigState {
                defaults: prev_defaults.clone(),
                last_applied_stamp: None,
                last_reload_error: None,
            });
            // Keeping the old provider must not also keep the old *policy*.
            //
            // This applies the whole reloaded config and then puts back only the
            // fields that genuinely depend on the provider we failed to build.
            // The inversion is the point: an include-list freezes every field
            // added to `ChannelRuntimeDefaults` in future unless someone
            // remembers to extend it, and forgetting is silent. It had already
            // happened — `approval_owners`, `guest_gate` and `allowlists` were
            // dropped here, so removing a compromised owner in the same edit
            // that left the provider unbuildable persisted the removal to disk
            // and never applied it, and the stamp advanced so nothing retried.
            //
            // With the exclusion list, a new field applies by default and
            // freezing one is a deliberate act that has to be written down here.
            let mut applied = next_defaults.clone();
            applied.default_provider = entry.defaults.default_provider.clone();
            applied.model = entry.defaults.model.clone();
            applied.api_key = entry.defaults.api_key.clone();
            applied.api_url = entry.defaults.api_url.clone();
            applied.reliability = entry.defaults.reliability.clone();
            entry.defaults = applied;
            entry.last_applied_stamp = Some(stamp);
            entry.last_reload_error = Some(reason);
            return Ok(());
        }
    };
    let next_default_provider: Arc<dyn Provider> = Arc::from(next_default_provider);

    if let Err(err) = next_default_provider.warmup().await {
        tracing::warn!(
            provider = %next_defaults.default_provider,
            "Provider warmup failed after config reload: {err}"
        );
    }

    {
        let mut cache = ctx.provider_cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.clear();
        cache.insert(
            next_defaults.default_provider.clone(),
            Arc::clone(&next_default_provider),
        );
    }

    // (autonomy level + allowed_commands were already applied above, before the
    // provider build, so they take effect even on the keep-old-provider branch.)
    {
        let mut guard = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
        guard.state = Some(RuntimeConfigState {
            defaults: next_defaults.clone(),
            last_applied_stamp: Some(stamp),
            last_reload_error: None,
        });
    }

    // If the operator changed the provider or default model (Web-UI switch or a
    // direct config edit), clear per-sender route overrides so senders pinned by
    // an in-chat `/model` / `/models` re-base to the new default — the operator
    // switch wins. Only clear on an actual change, never on unrelated reloads.
    if prev_defaults.default_provider != next_defaults.default_provider
        || prev_defaults.model != next_defaults.model
    {
        let mut routes = ctx
            .route_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !routes.is_empty() {
            tracing::info!(
                cleared = routes.len(),
                provider = %next_defaults.default_provider,
                model = %next_defaults.model,
                "Cleared per-sender route overrides after a provider/model change"
            );
            routes.clear();
        }
    }

    tracing::info!(
        path = %config_path.display(),
        provider = %next_defaults.default_provider,
        model = %next_defaults.model,
        temperature = next_defaults.temperature,
        "Applied updated channel runtime config from disk"
    );

    Ok(())
}

fn default_route_selection(ctx: &ChannelRuntimeContext) -> ChannelRouteSelection {
    let defaults = runtime_defaults_snapshot(ctx);
    ChannelRouteSelection {
        provider: defaults.default_provider,
        model: defaults.model,
    }
}

/// Look up a sender's pinned route, falling back to the current defaults.
///
/// **Lock-order invariant: the runtime-config store is acquired BEFORE
/// `route_overrides`, never while holding it.**
///
/// `default_route_selection` reaches the global config store via
/// `runtime_defaults_snapshot`. Written as one expression, the `route_overrides`
/// guard is a temporary that lives to the end of the statement — so the fallback
/// ran while still holding it, taking the two locks in the opposite order from
/// `set_route_selection` directly below. Both are `std::sync::Mutex` held inside
/// async tasks, so a cycle would wedge the entire dispatch loop with no error
/// and no recovery short of a restart. Binding the lookup drops the guard first.
fn get_route_selection(ctx: &ChannelRuntimeContext, sender_key: &str) -> ChannelRouteSelection {
    let existing = {
        ctx.route_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sender_key)
            .cloned()
    };
    existing.unwrap_or_else(|| default_route_selection(ctx))
}

fn set_route_selection(ctx: &ChannelRuntimeContext, sender_key: &str, next: ChannelRouteSelection) {
    let default_route = default_route_selection(ctx);
    let mut routes = ctx
        .route_overrides
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if next == default_route {
        routes.remove(sender_key);
    } else {
        routes.insert(sender_key.to_string(), next);
    }
}

fn clear_sender_history(ctx: &ChannelRuntimeContext, sender_key: &str) {
    ctx.conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(sender_key);

    // Persistence must never break message handling: log and ignore errors.
    if let Some(store) = ctx.history_store.as_ref() {
        if let Err(e) = store.delete(sender_key) {
            tracing::warn!("failed to delete persisted channel history for {sender_key}: {e}");
        }
    }
}

fn compact_sender_history(ctx: &ChannelRuntimeContext, sender_key: &str) -> bool {
    let mut histories = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let Some(turns) = histories.get_mut(sender_key) else {
        return false;
    };

    if turns.is_empty() {
        return false;
    }

    let keep_from = turns
        .len()
        .saturating_sub(CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES);
    let mut compacted = normalize_cached_channel_turns(turns[keep_from..].to_vec());

    for turn in &mut compacted {
        if turn.content.chars().count() > CHANNEL_HISTORY_COMPACT_CONTENT_CHARS {
            turn.content =
                truncate_with_ellipsis(&turn.content, CHANNEL_HISTORY_COMPACT_CONTENT_CHARS);
        }
    }

    if compacted.is_empty() {
        turns.clear();
        // Persist the now-empty state (save with [] deletes the row).
        let snapshot: Vec<ChatMessage> = Vec::new();
        drop(histories);
        persist_sender_turns(ctx, sender_key, &snapshot);
        return false;
    }

    *turns = compacted;
    let snapshot = turns.clone();
    drop(histories);
    persist_sender_turns(ctx, sender_key, &snapshot);
    true
}

/// Write-through helper: persist the current turns for a sender to the durable
/// store, if persistence is enabled. Errors are logged and ignored — durability
/// must never break live message handling.
fn persist_sender_turns(ctx: &ChannelRuntimeContext, sender_key: &str, turns: &[ChatMessage]) {
    if let Some(store) = ctx.history_store.as_ref() {
        if let Err(e) = store.save(sender_key, turns) {
            tracing::warn!("failed to persist channel history for {sender_key}: {e}");
        }
    }
}

fn append_sender_turn(ctx: &ChannelRuntimeContext, sender_key: &str, turn: ChatMessage) {
    let mut histories = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let turns = histories.entry(sender_key.to_string()).or_default();
    turns.push(turn);
    while turns.len() > MAX_CHANNEL_HISTORY {
        turns.remove(0);
    }
    let snapshot = turns.clone();
    drop(histories);
    persist_sender_turns(ctx, sender_key, &snapshot);
}

fn is_context_window_overflow_error(err: &anyhow::Error) -> bool {
    let lower = err.to_string().to_lowercase();
    [
        "exceeds the context window",
        "context window of this model",
        "maximum context length",
        "context length exceeded",
        "too many tokens",
        "token limit exceeded",
        "prompt is too long",
        "input is too long",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
}

/// Top model IDs for a provider, for the in-channel `/model` reply.
///
/// Resolves through the shared catalog rather than re-reading
/// `models_cache.json` here. This module used to carry its own copy of the
/// cache path, the deserialization structs and the lookup — a fourth reader of
/// one file with a fourth copy of the rules, which is the duplication that let
/// the catalog surfaces drift apart in the first place.
fn load_cached_model_preview(workspace_dir: &Path, provider_name: &str) -> Vec<String> {
    crate::onboard::wizard::provider_model_catalog(workspace_dir, provider_name)
        .models
        .into_iter()
        .take(MODEL_CACHE_PREVIEW_LIMIT)
        .collect()
}

async fn get_or_create_provider(
    ctx: &ChannelRuntimeContext,
    provider_name: &str,
) -> anyhow::Result<Arc<dyn Provider>> {
    if let Some(existing) = ctx
        .provider_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(provider_name)
        .cloned()
    {
        return Ok(existing);
    }

    if provider_name == ctx.default_provider.as_str() {
        return Ok(Arc::clone(&ctx.provider));
    }

    let defaults = runtime_defaults_snapshot(ctx);
    let api_url = if provider_name == defaults.default_provider.as_str() {
        defaults.api_url.as_deref()
    } else {
        None
    };

    let provider = create_resilient_provider_nonblocking(
        provider_name,
        ctx.api_key.clone(),
        api_url.map(ToString::to_string),
        ctx.reliability.as_ref().clone(),
        ctx.provider_runtime_options.clone(),
    )
    .await?;
    let provider: Arc<dyn Provider> = Arc::from(provider);

    if let Err(err) = provider.warmup().await {
        tracing::warn!(provider = provider_name, "Provider warmup failed: {err}");
    }

    let mut cache = ctx.provider_cache.lock().unwrap_or_else(|e| e.into_inner());
    let cached = cache
        .entry(provider_name.to_string())
        .or_insert_with(|| Arc::clone(&provider));
    Ok(Arc::clone(cached))
}

async fn create_resilient_provider_nonblocking(
    provider_name: &str,
    api_key: Option<String>,
    api_url: Option<String>,
    reliability: crate::config::ReliabilityConfig,
    provider_runtime_options: providers::ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    let provider_name = provider_name.to_string();
    tokio::task::spawn_blocking(move || {
        providers::create_resilient_provider_with_options(
            &provider_name,
            api_key.as_deref(),
            api_url.as_deref(),
            &reliability,
            &provider_runtime_options,
        )
    })
    .await
    .context("failed to join provider initialization task")?
}

fn build_models_help_response(current: &ChannelRouteSelection, workspace_dir: &Path) -> String {
    let mut response = String::new();
    let _ = writeln!(
        response,
        "Current provider: `{}`\nCurrent model: `{}`",
        current.provider, current.model
    );
    response.push_str("\nSwitch model with `/model <model-id>`.\n");

    let cached_models = load_cached_model_preview(workspace_dir, &current.provider);
    if cached_models.is_empty() {
        let _ = writeln!(
            response,
            "\nNo cached model list found for `{}`. Ask the operator to run `rantaiclaw models refresh --provider {}`.",
            current.provider, current.provider
        );
    } else {
        let _ = writeln!(
            response,
            "\nCached model IDs (top {}):",
            cached_models.len()
        );
        for model in cached_models {
            let _ = writeln!(response, "- `{model}`");
        }
    }

    response
}

fn build_providers_help_response(current: &ChannelRouteSelection) -> String {
    let mut response = String::new();
    let _ = writeln!(
        response,
        "Current provider: `{}`\nCurrent model: `{}`",
        current.provider, current.model
    );
    response.push_str("\nSwitch provider with `/models <provider>`.\n");
    response.push_str("Switch model with `/model <model-id>`.\n\n");
    response.push_str("Available providers:\n");
    for provider in providers::list_providers() {
        if provider.aliases.is_empty() {
            let _ = writeln!(response, "- {}", provider.name);
        } else {
            let _ = writeln!(
                response,
                "- {} (aliases: {})",
                provider.name,
                provider.aliases.join(", ")
            );
        }
    }
    response
}

async fn handle_runtime_command_if_needed(
    ctx: &ChannelRuntimeContext,
    msg: &traits::ChannelMessage,
    target_channel: Option<&Arc<dyn Channel>>,
) -> bool {
    let Some(command) = parse_runtime_command(&msg.channel, &msg.content) else {
        return false;
    };

    let Some(channel) = target_channel else {
        return true;
    };

    let sender_key = conversation_history_key(msg);
    let mut current = get_route_selection(ctx, &sender_key);

    let response = match command {
        ChannelRuntimeCommand::ShowProviders => build_providers_help_response(&current),
        ChannelRuntimeCommand::SetProvider(raw_provider) => {
            match resolve_provider_alias(&raw_provider) {
                Some(provider_name) => match get_or_create_provider(ctx, &provider_name).await {
                    Ok(_) => {
                        if provider_name != current.provider {
                            current.provider = provider_name.clone();
                            set_route_selection(ctx, &sender_key, current.clone());
                            clear_sender_history(ctx, &sender_key);
                        }

                        format!(
                            "Provider switched to `{provider_name}` for this sender session. Current model is `{}`.\nUse `/model <model-id>` to set a provider-compatible model.",
                            current.model
                        )
                    }
                    Err(err) => {
                        let safe_err = providers::sanitize_api_error(&err.to_string());
                        format!(
                            "Failed to initialize provider `{provider_name}`. Route unchanged.\nDetails: {safe_err}"
                        )
                    }
                },
                None => format!(
                    "Unknown provider `{raw_provider}`. Use `/models` to list valid providers."
                ),
            }
        }
        ChannelRuntimeCommand::ShowModel => {
            build_models_help_response(&current, ctx.workspace_dir.as_path())
        }
        ChannelRuntimeCommand::SetModel(raw_model) => {
            let model = raw_model.trim().trim_matches('`').to_string();
            if model.is_empty() {
                "Model ID cannot be empty. Use `/model <model-id>`.".to_string()
            } else {
                current.model = model.clone();
                set_route_selection(ctx, &sender_key, current.clone());
                clear_sender_history(ctx, &sender_key);

                format!(
                    "Model switched to `{model}` for provider `{}` in this sender session.",
                    current.provider
                )
            }
        }
    };

    if let Err(err) = channel
        .send(&SendMessage::new(response, &msg.reply_target).in_thread(msg.thread_ts.clone()))
        .await
    {
        tracing::warn!(
            "Failed to send runtime command response on {}: {err}",
            channel.name()
        );
    }

    true
}

async fn build_memory_context(
    mem: &dyn Memory,
    user_msg: &str,
    min_relevance_score: f64,
    conversation_id: Option<&str>,
) -> String {
    // The shared builder now owns these rules. This was the only one of the
    // three that bounded its output; the agent loader and the CLI loop have
    // been moved onto it rather than the other way round.
    crate::memory::build_memory_context(
        mem,
        user_msg,
        min_relevance_score,
        conversation_id,
        crate::memory::MemoryContextLimits {
            max_entries: MEMORY_CONTEXT_MAX_ENTRIES,
            max_entry_chars: MEMORY_CONTEXT_ENTRY_MAX_CHARS,
            max_total_chars: MEMORY_CONTEXT_MAX_CHARS,
        },
    )
    .await
    // Channels reach a remote user over a transport with no event stream, so
    // there is nowhere to surface the recalled keys; only the block is used.
    .block
}

/// Extract a compact summary of tool interactions from history messages added
/// during `run_tool_call_loop`. Scans assistant messages for `<tool_call>` tags
/// or native tool-call JSON to collect tool names used.
/// Returns an empty string when no tools were invoked.
fn extract_tool_context_summary(history: &[ChatMessage], start_index: usize) -> String {
    fn push_unique_tool_name(tool_names: &mut Vec<String>, name: &str) {
        let candidate = name.trim();
        if candidate.is_empty() {
            return;
        }
        if !tool_names.iter().any(|existing| existing == candidate) {
            tool_names.push(candidate.to_string());
        }
    }

    fn collect_tool_names_from_tool_call_tags(content: &str, tool_names: &mut Vec<String>) {
        const TAG_PAIRS: [(&str, &str); 4] = [
            ("<tool_call>", "</tool_call>"),
            ("<toolcall>", "</toolcall>"),
            ("<tool-call>", "</tool-call>"),
            ("<invoke>", "</invoke>"),
        ];

        for (open_tag, close_tag) in TAG_PAIRS {
            for segment in content.split(open_tag) {
                if let Some(json_end) = segment.find(close_tag) {
                    let json_str = segment[..json_end].trim();
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                        if let Some(name) = val.get("name").and_then(|n| n.as_str()) {
                            push_unique_tool_name(tool_names, name);
                        }
                    }
                }
            }
        }
    }

    fn collect_tool_names_from_native_json(content: &str, tool_names: &mut Vec<String>) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(content) {
            if let Some(calls) = val.get("tool_calls").and_then(|c| c.as_array()) {
                for call in calls {
                    let name = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .or_else(|| call.get("name").and_then(|n| n.as_str()));
                    if let Some(name) = name {
                        push_unique_tool_name(tool_names, name);
                    }
                }
            }
        }
    }

    fn collect_tool_names_from_tool_results(content: &str, tool_names: &mut Vec<String>) {
        let marker = "<tool_result name=\"";
        let mut remaining = content;
        while let Some(start) = remaining.find(marker) {
            let name_start = start + marker.len();
            let after_name_start = &remaining[name_start..];
            if let Some(name_end) = after_name_start.find('"') {
                let name = &after_name_start[..name_end];
                push_unique_tool_name(tool_names, name);
                remaining = &after_name_start[name_end + 1..];
            } else {
                break;
            }
        }
    }

    let mut tool_names: Vec<String> = Vec::new();

    for msg in history.iter().skip(start_index) {
        match msg.role.as_str() {
            "assistant" => {
                collect_tool_names_from_tool_call_tags(&msg.content, &mut tool_names);
                collect_tool_names_from_native_json(&msg.content, &mut tool_names);
            }
            "user" => {
                // Prompt-mode tool calls are always followed by [Tool results] entries
                // containing `<tool_result name="...">` tags with canonical tool names.
                collect_tool_names_from_tool_results(&msg.content, &mut tool_names);
            }
            _ => {}
        }
    }

    if tool_names.is_empty() {
        return String::new();
    }

    format!("[Used tools: {}]", tool_names.join(", "))
}

/// Shown when the model finishes a turn (often after tool calls) without any
/// final answer text, so the user never receives an empty or annotation-only
/// bubble.
const CHANNEL_EMPTY_REPLY_FALLBACK: &str =
    "I worked on that but don't have a final answer to show — want me to try again?";

/// Make a reply safe to deliver to a human: strip a leading internal
/// `[Used tools: …]` annotation (that belongs in history, not the chat) and
/// substitute a graceful message when nothing meaningful remains. The tool
/// summary is still recorded separately in conversation history.
fn clean_delivered_reply(text: &str) -> String {
    let mut s = text.trim_start();
    if s.starts_with("[Used tools:") {
        s = match s.find('\n') {
            Some(nl) => s[nl + 1..].trim_start(),
            None => "",
        };
    }
    let s = s.trim();
    if s.is_empty() {
        CHANNEL_EMPTY_REPLY_FALLBACK.to_string()
    } else {
        s.to_string()
    }
}

fn sanitize_channel_response(response: &str, tools: &[Box<dyn Tool>]) -> String {
    let known_tool_names: HashSet<String> = tools
        .iter()
        .map(|tool| tool.name().to_ascii_lowercase())
        .collect();
    strip_isolated_tool_json_artifacts(response, &known_tool_names)
}

fn is_tool_call_payload(value: &serde_json::Value, known_tool_names: &HashSet<String>) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };

    let (name, has_args) =
        if let Some(function) = object.get("function").and_then(|f| f.as_object()) {
            (
                function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .or_else(|| object.get("name").and_then(|v| v.as_str())),
                function.contains_key("arguments")
                    || function.contains_key("parameters")
                    || object.contains_key("arguments")
                    || object.contains_key("parameters"),
            )
        } else {
            (
                object.get("name").and_then(|v| v.as_str()),
                object.contains_key("arguments") || object.contains_key("parameters"),
            )
        };

    let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
        return false;
    };

    has_args && known_tool_names.contains(&name.to_ascii_lowercase())
}

fn is_tool_result_payload(
    object: &serde_json::Map<String, serde_json::Value>,
    saw_tool_call_payload: bool,
) -> bool {
    if !saw_tool_call_payload || !object.contains_key("result") {
        return false;
    }

    object.keys().all(|key| {
        matches!(
            key.as_str(),
            "result" | "id" | "tool_call_id" | "name" | "tool"
        )
    })
}

fn sanitize_tool_json_value(
    value: &serde_json::Value,
    known_tool_names: &HashSet<String>,
    saw_tool_call_payload: bool,
) -> Option<(String, bool)> {
    if is_tool_call_payload(value, known_tool_names) {
        return Some((String::new(), true));
    }

    if let Some(array) = value.as_array() {
        if !array.is_empty()
            && array
                .iter()
                .all(|item| is_tool_call_payload(item, known_tool_names))
        {
            return Some((String::new(), true));
        }
        return None;
    }

    let object = value.as_object()?;

    if let Some(tool_calls) = object.get("tool_calls").and_then(|value| value.as_array()) {
        if !tool_calls.is_empty()
            && tool_calls
                .iter()
                .all(|call| is_tool_call_payload(call, known_tool_names))
        {
            let content = object
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            return Some((content, true));
        }
    }

    if is_tool_result_payload(object, saw_tool_call_payload) {
        return Some((String::new(), false));
    }

    None
}

/// Whether anything but whitespace precedes `start` on its line.
///
/// The cheap half of [`is_line_isolated_json_segment`], split out because it
/// needs only `start` — so it can reject a candidate *before* paying for a
/// parse. A `{` in the middle of prose is the common case, and the loop used to
/// parse the entire remaining message for each one, then advance a single char
/// and do it again: O(braces x bytes) on every delivered reply, for a purely
/// cosmetic strip.
fn json_candidate_starts_its_line(message: &str, start: usize) -> bool {
    let line_start = message[..start].rfind('\n').map_or(0, |idx| idx + 1);
    message[line_start..start].trim().is_empty()
}

fn is_line_isolated_json_segment(message: &str, start: usize, end: usize) -> bool {
    let line_end = message[end..]
        .find('\n')
        .map_or(message.len(), |idx| end + idx);

    json_candidate_starts_its_line(message, start) && message[end..line_end].trim().is_empty()
}

fn strip_isolated_tool_json_artifacts(message: &str, known_tool_names: &HashSet<String>) -> String {
    let mut cleaned = String::with_capacity(message.len());
    let mut cursor = 0usize;
    let mut saw_tool_call_payload = false;

    while cursor < message.len() {
        let Some(rel_start) = message[cursor..].find(['{', '[']) else {
            cleaned.push_str(&message[cursor..]);
            break;
        };

        let start = cursor + rel_start;
        cleaned.push_str(&message[cursor..start]);

        // Reject before parsing when the candidate cannot be line-isolated
        // anyway. This is the whole performance fix: the parse below reads the
        // entire remaining message, and without this guard every `{` in prose
        // paid for one.
        let mut stream = if json_candidate_starts_its_line(message, start) {
            Some(
                serde_json::Deserializer::from_str(&message[start..])
                    .into_iter::<serde_json::Value>(),
            )
        } else {
            None
        };

        if let Some(Ok(value)) = stream.as_mut().and_then(|s| s.next()) {
            let stream = stream.as_ref().expect("checked above");
            let consumed = stream.byte_offset();
            if consumed > 0 {
                let end = start + consumed;
                if is_line_isolated_json_segment(message, start, end) {
                    if let Some((replacement, marks_tool_call)) =
                        sanitize_tool_json_value(&value, known_tool_names, saw_tool_call_payload)
                    {
                        if marks_tool_call {
                            saw_tool_call_payload = true;
                        }
                        if !replacement.trim().is_empty() {
                            cleaned.push_str(replacement.trim());
                        }
                        cursor = end;
                        continue;
                    }
                }
            }
        }

        let Some(ch) = message[start..].chars().next() else {
            break;
        };
        cleaned.push(ch);
        cursor = start + ch.len_utf8();
    }

    let mut result = cleaned.replace("\r\n", "\n");
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result.trim().to_string()
}

/// Outcome of trying to claim the per-channel single-runner lock.
enum ChannelLock {
    /// Lock held — keep the `File` alive for the listener's lifetime.
    Acquired(std::fs::File),
    /// Another live process already runs this channel — skip the listener.
    HeldByOther,
    /// Lock infrastructure unavailable (no data dir / IO error) — fail open
    /// and run anyway; the guard is best-effort, not a hard gate.
    Unavailable,
}

/// Claim an exclusive advisory lock for `channel` under the shared data dir
/// (`<data>/locks/channel-<name>.lock`). The lock is global (the WhatsApp
/// session and Telegram bot token are shared resources), so only one process
/// runs a given channel at a time. Released automatically on drop / exit.
fn acquire_channel_lock(channel: &str) -> ChannelLock {
    use fs2::FileExt;
    let Some(dirs) = directories::ProjectDirs::from("", "", "rantaiclaw") else {
        return ChannelLock::Unavailable;
    };
    let lock_dir = dirs.data_dir().join("locks");
    if std::fs::create_dir_all(&lock_dir).is_err() {
        return ChannelLock::Unavailable;
    }
    let lock_path = lock_dir.join(format!("channel-{channel}.lock"));
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
    else {
        return ChannelLock::Unavailable;
    };
    match file.try_lock_exclusive() {
        Ok(()) => ChannelLock::Acquired(file),
        Err(_) => ChannelLock::HeldByOther,
    }
}

fn spawn_supervised_listener(
    ch: Arc<dyn Channel>,
    tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    spawn_supervised_listener_with_health_interval(
        ch,
        tx,
        initial_backoff_secs,
        max_backoff_secs,
        Duration::from_secs(CHANNEL_HEALTH_HEARTBEAT_SECS),
        shutdown,
    )
}

fn spawn_supervised_listener_with_health_interval(
    ch: Arc<dyn Channel>,
    tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    health_interval: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let health_interval = if health_interval.is_zero() {
        Duration::from_secs(1)
    } else {
        health_interval
    };

    tokio::spawn(async move {
        // Single-runner guard: one OS process per channel. Hold an advisory
        // flock for the listener's lifetime. If another live process already
        // holds it (e.g. a daemon while a TUI also auto-starts channels), skip
        // this listener — running both causes duplicate/contradictory replies
        // (WhatsApp) or `409 Conflict` poll flapping (Telegram). Lock releases
        // on drop / process exit, so a crashed holder never blocks restart.
        let _channel_lock = match acquire_channel_lock(ch.name()) {
            ChannelLock::Acquired(lock) => Some(lock),
            ChannelLock::Unavailable => {
                tracing::debug!(
                    "channel {}: lock unavailable; running without single-runner guard",
                    ch.name()
                );
                None
            }
            ChannelLock::HeldByOther => {
                tracing::warn!(
                    "channel {} already running in another process; skipping this listener",
                    ch.name()
                );
                return;
            }
        };

        let component = format!("channel:{}", ch.name());
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);

        'supervise: loop {
            crate::health::mark_component_ok(&component);
            let mut health = tokio::time::interval(health_interval);
            health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let result = {
                // Pass the shared shutdown token so a well-behaved channel
                // (e.g. Telegram) aborts its long-poll cleanly. The
                // `shutdown.cancelled()` select arm is a backstop for
                // channels that ignore the token: breaking the loop drops
                // the pinned listen future, cancelling its in-flight work.
                let listen_future = ch.listen(tx.clone(), shutdown.clone());
                tokio::pin!(listen_future);

                loop {
                    tokio::select! {
                        () = shutdown.cancelled() => break 'supervise,
                        _ = health.tick() => {
                            crate::health::mark_component_ok(&component);
                        }
                        result = &mut listen_future => break result,
                    }
                }
            };

            if tx.is_closed() || shutdown.is_cancelled() {
                break;
            }

            match result {
                Ok(()) => {
                    tracing::warn!("Channel {} exited unexpectedly; restarting", ch.name());
                    crate::health::mark_component_error(&component, "listener exited unexpectedly");
                    // Clean exit — reset backoff since the listener ran successfully
                    backoff = initial_backoff_secs.max(1);
                }
                Err(e) => {
                    tracing::error!("Channel {} error: {e}; restarting", ch.name());
                    crate::health::mark_component_error(&component, e.to_string());
                }
            }

            crate::health::bump_component_restart(&component);
            // Cancellable backoff: a restart/shutdown request must not wait
            // out a long backoff window before the listener stops.
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(Duration::from_secs(backoff)) => {}
            }
            // Double backoff AFTER sleeping so first error uses initial_backoff
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

fn compute_max_in_flight_messages(channel_count: usize) -> usize {
    channel_count
        .saturating_mul(CHANNEL_PARALLELISM_PER_CHANNEL)
        .clamp(
            CHANNEL_MIN_IN_FLIGHT_MESSAGES,
            CHANNEL_MAX_IN_FLIGHT_MESSAGES,
        )
}

fn log_worker_join_result(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::error!("Channel message worker crashed: {error}");
    }
}

fn spawn_scoped_typing_task(
    channel: Arc<dyn Channel>,
    recipient: String,
    cancellation_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let stop_signal = cancellation_token;
    let refresh_interval = Duration::from_secs(CHANNEL_TYPING_REFRESH_INTERVAL_SECS);
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(refresh_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                () = stop_signal.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(e) = channel.start_typing(&recipient).await {
                        tracing::debug!("Failed to start typing on {}: {e}", channel.name());
                    }
                }
            }
        }

        if let Err(e) = channel.stop_typing(&recipient).await {
            tracing::debug!("Failed to stop typing on {}: {e}", channel.name());
        }
    });

    handle
}

async fn process_channel_message(
    ctx: Arc<ChannelRuntimeContext>,
    msg: traits::ChannelMessage,
    cancellation_token: CancellationToken,
) {
    if cancellation_token.is_cancelled() {
        return;
    }

    // Pre-v0.6.7 used `println!` here, which leaks into the TUI's
    // alt-screen and corrupts rendering when channels are auto-started
    // alongside `rantaiclaw` (a v0.6.6 tester saw an inbound Telegram line
    // — "[telegram] from <sender>: ..." — printed straight into the local
    // chat surface). Tracing routes to the log file in TUI
    // mode and to whatever subscriber daemon mode installs — operator
    // can `RUST_LOG=info` + tail the log file.
    tracing::info!(
        channel = %msg.channel,
        sender = %msg.sender,
        "channel message received: {}",
        truncate_with_ellipsis(&msg.content, 80)
    );

    let target_channel = ctx.channels_by_name.get(&msg.channel).cloned();
    if let Err(err) = maybe_apply_runtime_config_update(ctx.as_ref()).await {
        tracing::warn!("Failed to apply runtime config update: {err}");
    }
    if handle_runtime_command_if_needed(ctx.as_ref(), &msg, target_channel.as_ref()).await {
        return;
    }

    let history_key = conversation_history_key(&msg);
    let route = get_route_selection(ctx.as_ref(), &history_key);
    let runtime_defaults = runtime_defaults_snapshot(ctx.as_ref());
    let active_provider = match get_or_create_provider(ctx.as_ref(), &route.provider).await {
        Ok(provider) => provider,
        Err(err) => {
            let safe_err = providers::sanitize_api_error(&err.to_string());
            let message = format!(
                "⚠️ Failed to initialize provider `{}`. Please run `/models` to choose another provider.\nDetails: {safe_err}",
                route.provider
            );
            if let Some(channel) = target_channel.as_ref() {
                let _ = channel
                    .send(
                        &SendMessage::new(message, &msg.reply_target)
                            .in_thread(msg.thread_ts.clone()),
                    )
                    .await;
            }
            return;
        }
    };
    // Conversation scope for layered memory: one scope per chat/thread on this
    // surface (channel:sender[:thread]), the same identity used for history
    // keying. Stores and recalls are scoped to it so one chat's memory doesn't
    // bleed into another's, while shared/global memory still backfills.
    let conversation_scope = conversation::ConversationKey::new(&msg.channel, &msg.sender)
        .in_thread(msg.thread_ts.as_deref())
        .resolve();

    if runtime_defaults.auto_save_memory
        && msg.content.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS
    {
        let autosave_key = conversation_memory_key(&msg);
        // Raw inbound text, stored unread and re-injected into later prompts as
        // established context. Screen it the same way an agent-initiated write
        // is screened — this is the path where untrusted content actually
        // arrives, and nobody reviews it in between.
        match crate::memory::sanitize_memory_content(&msg.content) {
            Ok(sanitized) => {
                if !sanitized.notes.is_empty() {
                    tracing::debug!(
                        notes = %sanitized.notes.join("; "),
                        "adjusted an auto-saved message before storing"
                    );
                }
                let _ = ctx
                    .memory
                    .store(
                        &autosave_key,
                        &sanitized.content,
                        crate::memory::MemoryCategory::Conversation,
                        Some(conversation_scope.as_str()),
                    )
                    .await;
            }
            Err(reason) => {
                tracing::warn!("skipped auto-saving a message: {reason}");
            }
        }
    }

    tracing::info!("processing channel message");
    let started_at = Instant::now();

    let had_prior_history = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&history_key)
        .is_some_and(|turns| !turns.is_empty());

    // Preserve user turn before the LLM call so interrupted requests keep context.
    append_sender_turn(ctx.as_ref(), &history_key, ChatMessage::user(&msg.content));

    // Build history from per-sender conversation cache.
    let prior_turns_raw = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&history_key)
        .cloned()
        .unwrap_or_default();
    let mut prior_turns = normalize_cached_channel_turns(prior_turns_raw);

    // Only enrich with memory context when there is no prior conversation
    // history. Follow-up turns already include context from previous messages.
    if !had_prior_history {
        let memory_context = build_memory_context(
            ctx.memory.as_ref(),
            &msg.content,
            runtime_defaults.min_relevance_score,
            Some(conversation_scope.as_str()),
        )
        .await;
        if let Some(last_turn) = prior_turns.last_mut() {
            if last_turn.role == "user" && !memory_context.is_empty() {
                last_turn.content = format!("{memory_context}{}", msg.content);
            }
        }
    }

    // Owner status drives both the prompt (tell the model the sender is an
    // owner so it doesn't self-refuse owner-only tools) and the capability
    // ceiling below. Compute once so the two never disagree.
    let sender_is_owner = crate::approval::can_approve_any(
        &runtime_defaults.approval_owners,
        msg.sender_identities(),
    );
    // `ctx.system_prompt` is built once at channel start — it reads bootstrap
    // files and skills off disk, so rebuilding it per message is not free. The
    // approval policy can change under a running daemon, though, and the safety
    // section is pure in-memory work, so re-render just that part against the
    // preset carried on the reloaded defaults. Without this the gate followed a
    // config change while the briefing kept describing the boot-time preset.
    let base_prompt = crate::agent::prompt::replace_safety_section(
        ctx.system_prompt.as_str(),
        &crate::agent::prompt::render_safety_section(
            // `SafetySection` matches `Channel { .. }` and never reads the
            // payload, and the real value is only known where the provider is
            // built (channel startup). If the section ever starts branching on
            // it, this call site has to thread it through instead.
            crate::agent::prompt::PromptSurface::Channel {
                native_tools: false,
            },
            Some(runtime_defaults.autonomy_preset),
            ctx.tools_registry.as_ref(),
            &[],
        ),
    );
    let system_prompt = build_channel_system_prompt(
        &base_prompt,
        &msg.channel,
        &msg.reply_target,
        sender_is_owner,
    );
    let mut history = vec![ChatMessage::system(system_prompt)];
    history.extend(prior_turns);
    let use_streaming = target_channel
        .as_ref()
        .is_some_and(|ch| ch.supports_draft_updates());

    let (delta_tx, delta_rx) = if use_streaming {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let draft_message_id = if use_streaming {
        if let Some(channel) = target_channel.as_ref() {
            match channel
                .send_draft(
                    &SendMessage::new("...", &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::debug!("Failed to send draft on {}: {e}", channel.name());
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let draft_updater = if let (Some(mut rx), Some(draft_id_ref), Some(channel_ref)) = (
        delta_rx,
        draft_message_id.as_deref(),
        target_channel.as_ref(),
    ) {
        let channel = Arc::clone(channel_ref);
        let reply_target = msg.reply_target.clone();
        let draft_id = draft_id_ref.to_string();
        Some(tokio::spawn(async move {
            let mut accumulated = String::new();
            while let Some(delta) = rx.recv().await {
                accumulated.push_str(&delta);
                if let Err(e) = channel
                    .update_draft(&reply_target, &draft_id, &accumulated)
                    .await
                {
                    tracing::debug!("Draft update failed: {e}");
                }
            }
        }))
    } else {
        None
    };

    let typing_cancellation = target_channel.as_ref().map(|_| CancellationToken::new());
    let typing_task = match (target_channel.as_ref(), typing_cancellation.as_ref()) {
        (Some(channel), Some(token)) => Some(spawn_scoped_typing_task(
            Arc::clone(channel),
            msg.reply_target.clone(),
            token.clone(),
        )),
        _ => None,
    };

    // Record history length before tool loop so we can extract tool context after.
    let history_len_before_tools = history.len();

    enum LlmExecutionResult {
        Completed(Result<Result<String, anyhow::Error>, tokio::time::error::Elapsed>),
        Cancelled,
    }

    // In-chat owner approval (Layer A): only when tool-gating is active AND an
    // owner is configured AND we can post back to the chat. Otherwise the loop
    // keeps the auto-deny default — channels never gain approval power silently.
    // `autonomous_tools = true` opts out of gating; anything else keeps it armed.
    // Read from the reloaded defaults, not from boot, so re-arming applies live.
    let tool_gate = if runtime_defaults.autonomous_tools {
        None
    } else {
        ctx.channel_approval.as_deref()
    };
    let chat_relay_backend = if tool_gate.is_some() && !runtime_defaults.approval_owners.is_empty()
    {
        target_channel.as_ref().map(|chan| {
            approval_relay::ChatRelayApprovalBackend::new(
                Arc::clone(&ctx.tool_approvals),
                Arc::clone(chan),
                msg.reply_target.clone(),
                msg.thread_ts.clone(),
                msg.channel.clone(),
            )
        })
    } else {
        None
    };
    let chat_relay_backend_ref = chat_relay_backend
        .as_ref()
        .map(|b| b as &dyn crate::approval::ApprovalBackend);

    // Per-role capability ceiling: owners (senders in approval_owners) get the
    // full toolset; everyone else runs under the guest gate (safe tools +
    // guest_allowed_tools, shell limited to guest_allowed_commands).
    let guest_gate_ref = if sender_is_owner {
        None
    } else {
        Some(runtime_defaults.guest_gate.as_ref())
    };

    let timeout_budget_secs = channel_message_timeout_budget_secs(
        runtime_defaults.message_timeout_secs,
        runtime_defaults.max_tool_iterations,
    );
    let llm_result = tokio::select! {
        () = cancellation_token.cancelled() => LlmExecutionResult::Cancelled,
        result = tokio::time::timeout(
            Duration::from_secs(timeout_budget_secs),
            run_tool_call_loop(
                active_provider.as_ref(),
                &mut history,
                ctx.tools_registry.as_ref(),
                ctx.observer.as_ref(),
                route.provider.as_str(),
                route.model.as_str(),
                runtime_defaults.temperature,
                true,
                tool_gate,
                msg.channel.as_str(),
                // Origin chat → `cron_add` delivery safety net (announce channels).
                Some(msg.reply_target.as_str()),
                chat_relay_backend_ref,
                guest_gate_ref,
                &ctx.multimodal,
                runtime_defaults.max_tool_iterations,
                Some(cancellation_token.clone()),
                delta_tx,
                None,
            ),
        ) => LlmExecutionResult::Completed(result),
    };

    if let Some(handle) = draft_updater {
        let _ = handle.await;
    }

    if let Some(token) = typing_cancellation.as_ref() {
        token.cancel();
    }
    if let Some(handle) = typing_task {
        log_worker_join_result(handle.await);
    }

    match llm_result {
        LlmExecutionResult::Cancelled => {
            tracing::info!(
                channel = %msg.channel,
                sender = %msg.sender,
                "Cancelled in-flight channel request due to newer message"
            );
            if let (Some(channel), Some(draft_id)) =
                (target_channel.as_ref(), draft_message_id.as_deref())
            {
                if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                    tracing::debug!("Failed to cancel draft on {}: {err}", channel.name());
                }
            }
        }
        LlmExecutionResult::Completed(Ok(Ok(response))) => {
            let sanitized_response =
                sanitize_channel_response(&response, ctx.tools_registry.as_ref());
            let delivered_response = if sanitized_response.is_empty() && !response.trim().is_empty()
            {
                "I encountered malformed tool-call output and could not produce a safe reply. Please try again.".to_string()
            } else {
                sanitized_response
            };

            // Extract condensed tool-use context from the history messages
            // added during run_tool_call_loop, so the LLM retains awareness
            // of what it did on subsequent turns.
            let tool_summary = extract_tool_context_summary(&history, history_len_before_tools);
            let history_response = if tool_summary.is_empty() {
                delivered_response.clone()
            } else {
                format!("{tool_summary}\n{delivered_response}")
            };

            // Deliver the model's answer only: history keeps the tool summary,
            // but the user must never receive a bare `[Used tools: …]` line or an
            // empty bubble (e.g. when the model ends a turn after tool calls
            // without final text).
            let delivered_response = clean_delivered_reply(&delivered_response);
            tracing::info!(
                ms = started_at.elapsed().as_millis() as u64,
                "channel reply: {}",
                truncate_with_ellipsis(&delivered_response, 80)
            );

            // Deliver FIRST, record after. The append used to run before the
            // send, so a failed delivery left the model believing it had
            // answered — on the next turn it would reference a reply the user
            // never received.
            let delivered = if let Some(channel) = target_channel.as_ref() {
                if let Some(ref draft_id) = draft_message_id {
                    match channel
                        .finalize_draft(&msg.reply_target, draft_id, &delivered_response)
                        .await
                    {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!("Failed to finalize draft: {e}; sending as new message");
                            channel
                                .send(
                                    &SendMessage::new(&delivered_response, &msg.reply_target)
                                        .in_thread(msg.thread_ts.clone()),
                                )
                                .await
                                .is_ok()
                        }
                    }
                } else {
                    match channel
                        .send(
                            &SendMessage::new(&delivered_response, &msg.reply_target)
                                .in_thread(msg.thread_ts.clone()),
                        )
                        .await
                    {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::error!(channel = %channel.name(), "failed to reply: {e}");
                            false
                        }
                    }
                }
            } else {
                // No channel in the runtime map. Nothing was delivered, but that
                // is a routing problem with its own finding; preserve the
                // existing recording behaviour rather than changing an unrelated
                // path from inside this fix.
                true
            };

            append_sender_turn(
                ctx.as_ref(),
                &history_key,
                if delivered {
                    ChatMessage::assistant(&history_response)
                } else {
                    ChatMessage::assistant(UNDELIVERED_TURN_MARKER)
                },
            );
        }
        LlmExecutionResult::Completed(Ok(Err(e))) => {
            if crate::agent::loop_::is_tool_loop_cancelled(&e) || cancellation_token.is_cancelled()
            {
                tracing::info!(
                    channel = %msg.channel,
                    sender = %msg.sender,
                    "Cancelled in-flight channel request due to newer message"
                );
                if let (Some(channel), Some(draft_id)) =
                    (target_channel.as_ref(), draft_message_id.as_deref())
                {
                    if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                        tracing::debug!("Failed to cancel draft on {}: {err}", channel.name());
                    }
                }
                return;
            }

            if is_context_window_overflow_error(&e) {
                let compacted = compact_sender_history(ctx.as_ref(), &history_key);
                let error_text = if compacted {
                    "⚠️ Context window exceeded for this conversation. I compacted recent history and kept the latest context. Please resend your last message."
                } else {
                    "⚠️ Context window exceeded for this conversation. Please resend your last message."
                };
                tracing::warn!(
                    target: "channels",
                    channel = %msg.channel,
                    sender = %msg.sender,
                    elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                    compacted,
                    "context window exceeded"
                );
                if let Some(channel) = target_channel.as_ref() {
                    if let Some(ref draft_id) = draft_message_id {
                        let _ = channel
                            .finalize_draft(&msg.reply_target, draft_id, error_text)
                            .await;
                    } else {
                        let _ = channel
                            .send(
                                &SendMessage::new(error_text, &msg.reply_target)
                                    .in_thread(msg.thread_ts.clone()),
                            )
                            .await;
                    }
                }
                return;
            }

            tracing::error!(
                target: "channels",
                channel = %msg.channel,
                sender = %msg.sender,
                elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                "LLM error: {e:#}"
            );
            // Sanitize before it reaches a chat. Not every error on this arm
            // comes from the provider (tool execution, filesystem, transport),
            // so the raw chain can carry local absolute paths, internal URLs and
            // response fragments — delivered verbatim to an arbitrary sender,
            // including a guest, and unbounded in length. The sibling failure
            // path already does this; this one did not.
            //
            // The unredacted error stays in the `tracing` record above, where
            // the operator can still see it.
            // Pair the user turn appended at the start of this turn, so the
            // next question is not merged onto the failed one.
            append_sender_turn(
                ctx.as_ref(),
                &history_key,
                ChatMessage::assistant(FAILED_TURN_MARKER),
            );
            let safe_err = providers::sanitize_api_error(&format!("{e:#}"));
            let reply = format!("⚠️ Error: {safe_err}");
            if let Some(channel) = target_channel.as_ref() {
                if let Some(ref draft_id) = draft_message_id {
                    let _ = channel
                        .finalize_draft(&msg.reply_target, draft_id, &reply)
                        .await;
                } else {
                    let _ = channel
                        .send(
                            &SendMessage::new(reply, &msg.reply_target)
                                .in_thread(msg.thread_ts.clone()),
                        )
                        .await;
                }
            }
        }
        LlmExecutionResult::Completed(Err(_)) => {
            let timeout_msg = format!(
                "LLM response timed out after {}s (base={}s, max_tool_iterations={})",
                timeout_budget_secs,
                runtime_defaults.message_timeout_secs,
                runtime_defaults.max_tool_iterations
            );
            tracing::error!(
                target: "channels",
                channel = %msg.channel,
                sender = %msg.sender,
                elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                "{timeout_msg}"
            );
            append_sender_turn(
                ctx.as_ref(),
                &history_key,
                ChatMessage::assistant(TIMED_OUT_TURN_MARKER),
            );
            if let Some(channel) = target_channel.as_ref() {
                let error_text =
                    "⚠️ Request timed out while waiting for the model. Please try again.";
                if let Some(ref draft_id) = draft_message_id {
                    let _ = channel
                        .finalize_draft(&msg.reply_target, draft_id, error_text)
                        .await;
                } else {
                    let _ = channel
                        .send(
                            &SendMessage::new(error_text, &msg.reply_target)
                                .in_thread(msg.thread_ts.clone()),
                        )
                        .await;
                }
            }
        }
    }
}

async fn run_message_dispatch_loop(
    mut rx: tokio::sync::mpsc::Receiver<traits::ChannelMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    max_in_flight_messages: usize,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_in_flight_messages));
    let mut workers = tokio::task::JoinSet::new();
    let in_flight_by_sender = Arc::new(tokio::sync::Mutex::new(HashMap::<
        String,
        InFlightSenderTaskState,
    >::new()));
    let task_sequence = Arc::new(AtomicU64::new(1));

    while let Some(msg) = rx.recv().await {
        // Intercept approval replies before the message reaches the agent.
        // Try the whole-tool relay first (`/approve X`, `/deny X` — Layer A),
        // then the shell allowlist relay (`/allow X`, `y X`, … — Layer B). Both
        // are stateless: they consult only their pending registry and return an
        // acknowledgement if the text was a recognised reply, else `None` so
        // normal chat falls through. Owner authority is enforced inside each.
        // Refresh runtime config from disk first so reply authorization reads
        // the LIVE owner list (mirrors the per-message path) — owner changes
        // apply without a `channels run` restart.
        if let Err(err) = maybe_apply_runtime_config_update(ctx.as_ref()).await {
            tracing::warn!("Failed to apply runtime config update: {err}");
        }
        let live_owners = live_approval_owners(ctx.as_ref());
        // Authorize the reply against ANY of the sender's identity forms (parity
        // with the capability gate), so an owner recorded under a different form
        // than the one the runtime resolved `sender` to can still approve. The
        // relay uses this identity only for the owner check, so handing it a
        // matching form is equivalent and keeps the relay signatures single-form.
        let approver = msg
            .sender_identities()
            .find(|id| crate::approval::can_approve(&live_owners, id))
            .unwrap_or(msg.sender.as_str());
        // The chat this reply arrived in. Resolution used to consult neither the
        // request id nor the origin the request already carried, so an approval
        // posted into one chat could be answered from another.
        let approval_reply = approval_relay::try_handle_tool_reply(
            &msg.content,
            ctx.tool_approvals.as_ref(),
            approver,
            &live_owners,
            &msg.channel,
            &msg.reply_target,
        )
        .or_else(|| {
            approval_relay::try_handle_reply(
                &msg.content,
                ctx.security.as_ref(),
                approver,
                &live_owners,
                &msg.channel,
                &msg.reply_target,
            )
        });
        if let Some(reply) = approval_reply {
            if let Some(channel) = ctx.channels_by_name.get(&msg.channel) {
                let ack = traits::SendMessage::new(reply, msg.reply_target.clone())
                    .in_thread(msg.thread_ts.clone());
                if let Err(e) = channel.send(&ack).await {
                    tracing::warn!(
                        target: "approval_relay",
                        channel = %msg.channel,
                        error = %e,
                        "failed to deliver approval ack"
                    );
                }
            }
            continue;
        }

        let permit = match Arc::clone(&semaphore).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };

        let worker_ctx = Arc::clone(&ctx);
        let in_flight = Arc::clone(&in_flight_by_sender);
        let task_sequence = Arc::clone(&task_sequence);
        workers.spawn(async move {
            let _permit = permit;
            let interrupt_enabled =
                worker_ctx.interrupt_on_new_message && msg.channel == "telegram";
            let sender_scope_key = interruption_scope_key(&msg);
            let cancellation_token = CancellationToken::new();
            let completion = Arc::new(InFlightTaskCompletion::new());
            let task_id = task_sequence.fetch_add(1, Ordering::Relaxed);

            // Releases waiters on EVERY exit path, including a panic. Held for
            // the rest of the closure; see `CompletionGuard`.
            let _completion_guard = CompletionGuard(Arc::clone(&completion));

            if interrupt_enabled {
                let previous = {
                    let mut active = in_flight.lock().await;
                    active.insert(
                        sender_scope_key.clone(),
                        InFlightSenderTaskState {
                            task_id,
                            cancellation: cancellation_token.clone(),
                            completion: Arc::clone(&completion),
                        },
                    )
                };

                if let Some(previous) = previous {
                    tracing::info!(
                        channel = %msg.channel,
                        sender = %msg.sender,
                        "Interrupting previous in-flight request for sender"
                    );
                    previous.cancellation.cancel();
                    // Bounded: the guard above makes a lost signal far less
                    // likely, but this wait is on the path that stops the whole
                    // dispatch loop draining, so it must not be able to hang.
                    // Two overlapping turns for one sender is strictly better
                    // than a channel that never answers again.
                    if tokio::time::timeout(
                        IN_FLIGHT_COMPLETION_WAIT_TIMEOUT,
                        previous.completion.wait(),
                    )
                    .await
                    .is_err()
                    {
                        tracing::warn!(
                            channel = %msg.channel,
                            sender = %msg.sender,
                            timeout_secs = IN_FLIGHT_COMPLETION_WAIT_TIMEOUT.as_secs(),
                            "previous in-flight request did not signal completion; proceeding anyway"
                        );
                    }
                }
            }

            process_channel_message(worker_ctx, msg, cancellation_token).await;

            if interrupt_enabled {
                let mut active = in_flight.lock().await;
                if active
                    .get(&sender_scope_key)
                    .is_some_and(|state| state.task_id == task_id)
                {
                    active.remove(&sender_scope_key);
                }
            }
        });

        while let Some(result) = workers.try_join_next() {
            log_worker_join_result(result);
        }
    }

    while let Some(result) = workers.join_next().await {
        log_worker_join_result(result);
    }
}

/// Load workspace identity files and build a system prompt.
///
/// Follows the `OpenClaw` framework structure by default:
/// 1. Tooling — tool list + descriptions
/// 2. Safety — guardrail reminder
/// 3. Skills — full skill instructions and tool metadata
/// 4. Workspace — working directory
/// 5. Bootstrap files — AGENTS, SOUL, TOOLS, IDENTITY, USER, BOOTSTRAP, MEMORY
/// 6. Date & Time — timezone for cache stability
/// 7. Runtime — host, OS, model
///
/// When `identity_config` is set to AIEOS format, the bootstrap files section
/// is replaced with the AIEOS identity data loaded from file or inline JSON.
///
/// Daily memory files (`memory/*.md`) are NOT injected — they are accessed
/// on-demand via `memory_recall` / `memory_search` tools.
pub fn build_system_prompt(
    workspace_dir: &std::path::Path,
    model_name: &str,
    tools: &[(&str, &str)],
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
) -> String {
    build_system_prompt_with_mode(
        workspace_dir,
        model_name,
        tools,
        skills,
        identity_config,
        bootstrap_max_chars,
        false,
        crate::config::SkillsPromptInjectionMode::Full,
    )
}

pub fn build_system_prompt_with_mode(
    workspace_dir: &std::path::Path,
    model_name: &str,
    tools: &[(&str, &str)],
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
    native_tools: bool,
    skills_prompt_mode: crate::config::SkillsPromptInjectionMode,
) -> String {
    // Unified prompt builder: the SAME `SystemPromptBuilder` the TUI/`Agent`
    // path uses, with `surface = Channel` so the surface-specific hint sections
    // (Hardware / Your Task / Channel Capabilities) and the timezone-only
    // datetime turn on while persona/identity/tools/safety/skills stay shared.
    //
    // Description-only tools `(name, desc)` are wrapped as `DescriptorTool` so
    // the one `ToolsSection` renders them (no `Parameters:` line for these).
    // Tool-call protocol instructions are appended by the caller via
    // `build_tool_instructions`, so `dispatcher_instructions` stays empty here.
    //
    // Resolve the active approval preset so SafetySection renders the
    // channel-accurate guidance (Strict really drops `shell` here after
    // PR3b-strict; Smart/Manual describe owner-approval, not the TUI's inline
    // Y/N/A). Failure to read the policy is non-fatal — `None` falls back to the
    // generic safety floor. The shell allowlist is intentionally NOT surfaced on
    // channels: the Layer-A approval manager gates non-read-only tools before
    // the Layer-B shell allowlist applies, so listing globs here would mislead.
    use crate::agent::prompt::{DescriptorTool, PromptContext, PromptSurface, SystemPromptBuilder};
    use crate::tools::Tool;

    let autonomy_preset = crate::profile::ProfileManager::active()
        .ok()
        .map(|profile| crate::approval::policy_writer::read_active_preset(&profile.policy_dir()))
        .unwrap_or(None);

    let stub_tools: Vec<Box<dyn Tool>> = tools
        .iter()
        .map(|(name, desc)| Box::new(DescriptorTool::new(*name, *desc)) as Box<dyn Tool>)
        .collect();

    let ctx = PromptContext {
        workspace_dir,
        model_name,
        surface: PromptSurface::Channel { native_tools },
        bootstrap_max_chars: bootstrap_max_chars.unwrap_or(BOOTSTRAP_MAX_CHARS),
        tools: &stub_tools,
        skills,
        skills_prompt_mode,
        identity_config,
        dispatcher_instructions: "",
        autonomy_preset,
        allowed_commands: &[],
    };

    let prompt = SystemPromptBuilder::with_defaults()
        .build(&ctx)
        .unwrap_or_default();

    if prompt.trim().is_empty() {
        "You are RantaiClaw, a fast and efficient AI assistant built in Rust. Be helpful, concise, and direct."
            .to_string()
    } else {
        prompt
    }
}

fn normalize_telegram_identity(value: &str) -> String {
    value.trim().trim_start_matches('@').to_string()
}

async fn bind_telegram_identity(config: &Config, identity: &str) -> Result<()> {
    let normalized = normalize_telegram_identity(identity);
    if normalized.is_empty() {
        anyhow::bail!("Telegram identity cannot be empty");
    }

    let mut updated = config.clone();
    let Some(telegram) = updated.channels_config.telegram.as_mut() else {
        anyhow::bail!(
            "Telegram channel is not configured. Run `rantaiclaw onboard --channels-only` first"
        );
    };

    if telegram.allowed_users.iter().any(|u| u == "*") {
        println!(
            "⚠️ Telegram allowlist is currently wildcard (`*`) — binding is unnecessary until you remove '*'."
        );
    }

    if telegram
        .allowed_users
        .iter()
        .map(|entry| normalize_telegram_identity(entry))
        .any(|entry| entry == normalized)
    {
        println!("✅ Telegram identity already bound: {normalized}");
        return Ok(());
    }

    telegram.allowed_users.push(normalized.clone());
    updated.save().await?;
    println!("✅ Bound Telegram identity: {normalized}");
    println!("   Saved to {}", updated.config_path.display());
    announce_daemon_reload();
    Ok(())
}

async fn unbind_telegram_identity(config: &Config, identity: &str) -> Result<()> {
    let normalized = normalize_telegram_identity(identity);
    if normalized.is_empty() {
        anyhow::bail!("Telegram identity cannot be empty");
    }

    let mut updated = config.clone();
    let Some(telegram) = updated.channels_config.telegram.as_mut() else {
        anyhow::bail!(
            "Telegram channel is not configured. Run `rantaiclaw onboard --channels-only` first"
        );
    };

    let before = telegram.allowed_users.len();
    telegram
        .allowed_users
        .retain(|entry| normalize_telegram_identity(entry) != normalized);
    let removed = before - telegram.allowed_users.len();

    if removed == 0 {
        println!("ℹ️ Telegram identity not in allowlist: {normalized} (nothing to remove)");
        return Ok(());
    }

    let now_empty = telegram.allowed_users.is_empty();
    updated.save().await?;
    let plural = if removed == 1 { "entry" } else { "entries" };
    println!("✅ Removed Telegram identity: {normalized} ({removed} {plural} dropped)");
    println!("   Saved to {}", updated.config_path.display());
    if now_empty {
        println!(
            "⚠️ The Telegram allowlist is now empty — the bot will respond to NO ONE. \
             Add yourself with `rantaiclaw channel bind-telegram <your-username-or-id>`."
        );
    }
    announce_daemon_reload();
    Ok(())
}

/// Resolve the active profile root for the on-disk pairing-code store.
fn pairing_profile_root() -> Result<PathBuf> {
    Ok(crate::profile::ProfileManager::active()?.root)
}

/// Mint an on-demand pairing code for `channel` into the shared store and print
/// the code plus `/bind`/`/claim` instructions. Works whether or not the daemon
/// is running — a running daemon validates the code on the next pairing message
/// without a restart.
///
/// `ttl_minutes` is the validity window; `max_uses` bounds claims (`None` =
/// unlimited within the window); `grant_owner` permits `/claim` (owner). Returns
/// the minted plaintext code (also used by tests to assert it is non-empty).
fn pair_channel(
    channel: &str,
    ttl_minutes: i64,
    max_uses: Option<u32>,
    grant_owner: bool,
) -> Result<String> {
    let root = pairing_profile_root()?;
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let code = crate::security::pairing_store::mint(
        &root,
        channel,
        ttl_minutes.saturating_mul(60),
        max_uses,
        grant_owner,
        now,
    )
    .with_context(|| format!("minting pairing code for {channel}"))?;

    let uses = match max_uses {
        Some(1) => "single-use".to_string(),
        Some(n) => format!("up to {n} uses"),
        None => "multi-use".to_string(),
    };
    println!("🔐 Pairing code for {channel}: {code}   (valid {ttl_minutes} min, {uses})");
    println!("   DM the bot:  /bind {code}  (chat)  |  /claim {code}  (owner)");
    println!(
        "   No daemon restart needed — a running channel picks this up on the next pairing message."
    );
    Ok(code)
}

/// Try to reload a running managed daemon service after a config change, and
/// print a clear note about what happened either way. Shared by the channel
/// allowlist binder and the `permissions` CLI so config edits made on disk are
/// picked up without the user having to remember to bounce the service.
pub(crate) fn announce_daemon_reload() {
    match maybe_restart_managed_daemon_service() {
        Ok(true) => {
            println!("🔄 Detected running managed daemon service; reloaded automatically.");
        }
        Ok(false) => {
            println!(
                "ℹ️ No managed daemon service detected. If `rantaiclaw daemon`/`channel start` is already running, restart it to load the change."
            );
        }
        Err(e) => {
            eprintln!(
                "⚠️ Saved, but failed to reload daemon service automatically: {e}\n\
                 Restart service manually with `rantaiclaw service stop && rantaiclaw service start`."
            );
        }
    }
}

/// Reload a running managed daemon service (systemd / launchd / OpenRC) after a
/// config change, for non-CLI callers (the gateway) that must not print to
/// stdout. Returns `Ok(true)` if a managed service was restarted, `Ok(false)`
/// when none is installed. Mirrors what [`announce_daemon_reload`] does for the
/// CLI, minus the console output — callers log the outcome themselves.
pub(crate) fn reload_managed_daemon() -> Result<bool> {
    maybe_restart_managed_daemon_service()
}

fn maybe_restart_managed_daemon_service() -> Result<bool> {
    if cfg!(target_os = "macos") {
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .context("Could not find home directory")?;
        let plist = home
            .join("Library")
            .join("LaunchAgents")
            .join("com.rantaiclaw.daemon.plist");
        if !plist.exists() {
            return Ok(false);
        }

        let list_output = Command::new("launchctl")
            .arg("list")
            .output()
            .context("Failed to query launchctl list")?;
        let listed = String::from_utf8_lossy(&list_output.stdout);
        if !listed.contains("com.rantaiclaw.daemon") {
            return Ok(false);
        }

        let _ = Command::new("launchctl")
            .args(["stop", "com.rantaiclaw.daemon"])
            .output();
        let start_output = Command::new("launchctl")
            .args(["start", "com.rantaiclaw.daemon"])
            .output()
            .context("Failed to start launchd daemon service")?;
        if !start_output.status.success() {
            let stderr = String::from_utf8_lossy(&start_output.stderr);
            anyhow::bail!("launchctl start failed: {}", stderr.trim());
        }

        return Ok(true);
    }

    if cfg!(target_os = "linux") {
        // OpenRC (system-wide) takes precedence over systemd (user-level)
        let openrc_init_script = PathBuf::from("/etc/init.d/rantaiclaw");
        if openrc_init_script.exists() {
            if let Ok(status_output) = Command::new("rc-service").args(OPENRC_STATUS_ARGS).output()
            {
                // rc-service exits 0 if running, non-zero otherwise
                if status_output.status.success() {
                    let restart_output = Command::new("rc-service")
                        .args(OPENRC_RESTART_ARGS)
                        .output()
                        .context("Failed to restart OpenRC daemon service")?;
                    if !restart_output.status.success() {
                        let stderr = String::from_utf8_lossy(&restart_output.stderr);
                        anyhow::bail!("rc-service restart failed: {}", stderr.trim());
                    }
                    return Ok(true);
                }
            }
        }

        // Systemd (user-level)
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .context("Could not find home directory")?;
        let unit_path: PathBuf = home
            .join(".config")
            .join("systemd")
            .join("user")
            .join("rantaiclaw.service");
        if !unit_path.exists() {
            return Ok(false);
        }

        let active_output = Command::new("systemctl")
            .args(SYSTEMD_STATUS_ARGS)
            .output()
            .context("Failed to query systemd service state")?;
        let state = String::from_utf8_lossy(&active_output.stdout);
        if !state.trim().eq_ignore_ascii_case("active") {
            return Ok(false);
        }

        let restart_output = Command::new("systemctl")
            .args(SYSTEMD_RESTART_ARGS)
            .output()
            .context("Failed to restart systemd daemon service")?;
        if !restart_output.status.success() {
            let stderr = String::from_utf8_lossy(&restart_output.stderr);
            anyhow::bail!("systemctl restart failed: {}", stderr.trim());
        }

        return Ok(true);
    }

    Ok(false)
}

/// Every channel type this build knows about: `(key, display)` in a stable,
/// operator-facing order.
///
/// `key` is the lowercase `Channel::name()` value, which is what
/// `build_configured_channels` emits and what cron delivery selects on.
/// `channel_roster` reports this list and the factory builds from it, so a
/// channel cannot exist on one surface and not the other — the previous roster
/// was a *separate* hand-maintained list documented as "the single source of
/// truth… so the two surfaces can never disagree", and they disagreed anyway.
///
/// `webhook` is included because operators think of it as a channel and both
/// `channel list` and the doctor report it, but it is **not** a `Channel`
/// implementer — it is served by the gateway — so the factory never builds it.
/// `channel_keys_are_buildable_or_documented` pins that exception.
pub(crate) const CHANNEL_CATALOG: [(&str, &str); 16] = [
    ("telegram", "Telegram"),
    ("discord", "Discord"),
    ("slack", "Slack"),
    ("mattermost", "Mattermost"),
    ("webhook", "Webhook"),
    ("imessage", "iMessage"),
    ("matrix", "Matrix"),
    ("signal", "Signal"),
    ("whatsapp", "WhatsApp"),
    ("linq", "Linq"),
    ("nextcloud_talk", "Nextcloud Talk"),
    ("email", "Email"),
    ("irc", "IRC"),
    ("lark", "Lark"),
    ("dingtalk", "DingTalk"),
    ("qq", "QQ"),
];

/// The one channel key in [`CHANNEL_CATALOG`] that is not a `Channel`
/// implementer: the webhook is served by the gateway, so the factory never
/// builds it and the doctor reports it separately.
pub(crate) const NON_CHANNEL_CATALOG_KEYS: [&str; 1] = ["webhook"];

/// Whether `key` is configured in this build. Matrix and Lark are additionally
/// gated on their build features, so a channel configured in a build that cannot
/// run it reports as not configured rather than as silently missing.
pub(crate) fn channel_is_configured(key: &str, config: &Config) -> bool {
    let c = &config.channels_config;
    match key {
        "telegram" => c.telegram.is_some(),
        "discord" => c.discord.is_some(),
        "slack" => c.slack.is_some(),
        "mattermost" => c.mattermost.is_some(),
        "webhook" => c.webhook.is_some(),
        "imessage" => c.imessage.is_some(),
        "matrix" => cfg!(feature = "channel-matrix") && c.matrix.is_some(),
        "signal" => c.signal.is_some(),
        "whatsapp" => c.whatsapp.is_some(),
        "linq" => c.linq.is_some(),
        "nextcloud_talk" => c.nextcloud_talk.is_some(),
        "email" => c.email.is_some(),
        "irc" => c.irc.is_some(),
        "lark" => cfg!(feature = "channel-lark") && c.lark.is_some(),
        "dingtalk" => c.dingtalk.is_some(),
        "qq" => c.qq.is_some(),
        _ => false,
    }
}

/// Every channel key in [`CHANNEL_CATALOG`], for callers that need to validate
/// a user-supplied surface name against the one canonical list.
pub(crate) fn channel_catalog_keys() -> Vec<&'static str> {
    CHANNEL_CATALOG.iter().map(|(key, _)| *key).collect()
}

/// Whether `key`'s config block carries the credential it needs to run.
///
/// Stronger than [`channel_is_configured`], which only asks whether the section
/// exists: a block with an empty `bot_token` — a hand-edit, or an aborted
/// `/setup` — is present but cannot start. Each channel carries its own notion
/// of a credential; iMessage and Webhook have none (iMessage drives the local
/// Messages app, Webhook is an inbound receiver), so presence *is*
/// configuration for those two.
///
/// This lived as a private copy inside `src/tui/app.rs` alongside a second
/// copy that used the weaker predicate — two lists that could and did disagree
/// with each other and with this module. It belongs here, beside the catalog
/// they are both derived from.
pub(crate) fn channel_has_credentials(key: &str, config: &Config) -> bool {
    let c = &config.channels_config;
    let filled = |s: &str| !s.trim().is_empty();
    match key {
        "telegram" => c.telegram.as_ref().is_some_and(|t| filled(&t.bot_token)),
        "discord" => c.discord.as_ref().is_some_and(|d| filled(&d.bot_token)),
        "slack" => c.slack.as_ref().is_some_and(|s| filled(&s.bot_token)),
        "mattermost" => c
            .mattermost
            .as_ref()
            .is_some_and(|m| filled(&m.url) && filled(&m.bot_token)),
        // Presence is configuration: the webhook is served by the gateway and
        // authenticates per-request, not per-channel.
        "webhook" => c.webhook.is_some(),
        // Presence is configuration: iMessage drives the local Messages app.
        "imessage" => c.imessage.is_some(),
        "matrix" => {
            cfg!(feature = "channel-matrix")
                && c.matrix
                    .as_ref()
                    .is_some_and(|m| filled(&m.homeserver) && filled(&m.access_token))
        }
        "signal" => c
            .signal
            .as_ref()
            .is_some_and(|s| filled(&s.http_url) && filled(&s.account)),
        // Either transport counts: the Cloud API uses an access token, the
        // web client a linked session.
        "whatsapp" => c.whatsapp.as_ref().is_some_and(|w| {
            w.access_token.as_deref().is_some_and(filled)
                || w.session_path.as_deref().is_some_and(filled)
        }),
        "linq" => c.linq.as_ref().is_some_and(|l| filled(&l.api_token)),
        "nextcloud_talk" => c
            .nextcloud_talk
            .as_ref()
            .is_some_and(|n| filled(&n.base_url) && filled(&n.app_token)),
        "email" => c.email.as_ref().is_some_and(|e| {
            filled(&e.imap_host)
                && filled(&e.smtp_host)
                && filled(&e.username)
                && filled(&e.password)
        }),
        "irc" => c
            .irc
            .as_ref()
            .is_some_and(|i| filled(&i.server) && filled(&i.nickname)),
        "lark" => {
            cfg!(feature = "channel-lark")
                && c.lark
                    .as_ref()
                    .is_some_and(|l| filled(&l.app_id) && filled(&l.app_secret))
        }
        "dingtalk" => c
            .dingtalk
            .as_ref()
            .is_some_and(|d| filled(&d.client_id) && filled(&d.client_secret)),
        "qq" => {
            c.qq.as_ref()
                .is_some_and(|q| filled(&q.app_id) && filled(&q.app_secret))
        }
        _ => false,
    }
}

/// Roster keyed on credential presence rather than section presence.
///
/// The `(display label, ready?)` shape the TUI status panel needs, derived from
/// the same [`CHANNEL_CATALOG`] as [`channel_roster`] so the two cannot drift.
pub(crate) fn channel_status_roster(config: &Config) -> Vec<(&'static str, bool)> {
    CHANNEL_CATALOG
        .iter()
        .map(|(key, display)| (*display, channel_has_credentials(key, config)))
        .collect()
}

/// How many channels are configured well enough to start.
pub(crate) fn configured_channel_count(config: &Config) -> usize {
    CHANNEL_CATALOG
        .iter()
        .filter(|(key, _)| channel_has_credentials(key, config))
        .count()
}

/// Canonical channel roster: `(display label, configured?)` for every channel
/// type in a stable order, derived from [`CHANNEL_CATALOG`] rather than
/// maintained beside it.
/// Say out loud what an `approval_owners` value actually grants.
///
/// `"*"` makes **every sender on every channel** an owner — the full toolset and
/// the right to approve shell commands. The gateway already warns when
/// `allowed_users` contains `*`; nothing warned for this, and no doc said the
/// value was even accepted.
///
/// The bare `"user"` entry is the console's pre-`cli:local` identity. It is
/// still honoured, but it matches any remote sender who picks that name.
fn warn_on_risky_approval_owners(owners: &[String]) {
    if owners.iter().any(|o| o.trim() == "*") {
        tracing::warn!(
            "approval_owners contains \"*\": EVERY sender on EVERY channel is an owner \
             and may approve shell commands. Replace it with explicit sender ids."
        );
    }
    if owners
        .iter()
        .any(|o| o.trim() == crate::channels::cli::LEGACY_CLI_SENDER_ID)
    {
        tracing::warn!(
            "approval_owners contains \"{}\", the console's old unqualified identity — a \
             remote sender using that same name is also an owner. Change it to \"{}\".",
            crate::channels::cli::LEGACY_CLI_SENDER_ID,
            crate::channels::cli::CLI_SENDER_ID
        );
    }
}

pub(crate) fn channel_roster(config: &Config) -> Vec<(&'static str, bool)> {
    CHANNEL_CATALOG
        .iter()
        .map(|(key, display)| (*display, channel_is_configured(key, config)))
        .collect()
}

/// Guidance for `channel add`, which configures nothing itself.
///
/// Split out so the text is testable without capturing stdout. Both this and
/// `channel_remove_guidance` used to be `bail!`, so a script wrapping
/// `rantaiclaw channel add` saw a non-zero exit for an informational outcome.
fn channel_add_guidance(channel_type: &str) -> String {
    format!("Channel type '{channel_type}' — use `rantaiclaw onboard` to configure channels")
}

/// Guidance for `channel remove`. See [`channel_add_guidance`].
fn channel_remove_guidance(name: &str) -> String {
    format!("Remove channel '{name}' — edit ~/.rantaiclaw/config.toml directly")
}

pub(crate) async fn handle_command(command: crate::ChannelCommands, config: &Config) -> Result<()> {
    match command {
        // Dispatched in `main.rs`, which owns the async runtime these need, so
        // they never reach this match. They used to `bail!` with that routing
        // detail as the user-visible error text — an internal invariant printed
        // as if the operator had done something wrong.
        crate::ChannelCommands::Start
        | crate::ChannelCommands::Run
        | crate::ChannelCommands::Doctor => {
            unreachable!("channel start/run/doctor are dispatched in main.rs")
        }
        crate::ChannelCommands::List => {
            crate::cli_style::section("channels");
            crate::cli_style::status_row(true, "CLI", 14, "always");
            for (name, configured) in channel_roster(config) {
                crate::cli_style::status_row(
                    configured,
                    name,
                    14,
                    if configured {
                        "configured"
                    } else {
                        "not configured"
                    },
                );
            }
            if !cfg!(feature = "channel-matrix") {
                println!(
                    "  {}",
                    crate::cli_style::dim(
                        "Matrix support is disabled in this build (enable `channel-matrix`)."
                    )
                );
            }
            if !cfg!(feature = "channel-lark") {
                println!(
                    "  {}",
                    crate::cli_style::dim(
                        "Lark support is disabled in this build (enable `channel-lark`)."
                    )
                );
            }
            println!();
            println!(
                "  {}",
                crate::cli_style::dim(
                    "start: rantaiclaw channel start  ·  health: channel doctor  ·  setup: onboard"
                )
            );
            Ok(())
        }
        // Guidance, not failure. These bailed, so a script wrapping
        // `rantaiclaw channel add` saw a non-zero exit for what is an
        // informational outcome.
        crate::ChannelCommands::Add { channel_type } => {
            println!("{}", channel_add_guidance(&channel_type));
            Ok(())
        }
        crate::ChannelCommands::Remove { name } => {
            println!("{}", channel_remove_guidance(&name));
            Ok(())
        }
        crate::ChannelCommands::BindTelegram { identity } => {
            bind_telegram_identity(config, &identity).await
        }
        crate::ChannelCommands::UnbindTelegram { identity } => {
            unbind_telegram_identity(config, &identity).await
        }
        crate::ChannelCommands::Pair {
            channel,
            ttl,
            max_uses,
            no_owner,
        } => {
            pair_channel(&channel, ttl, max_uses, !no_owner)?;
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelHealthState {
    Healthy,
    Unhealthy,
    Timeout,
}

fn classify_health_result(
    result: &std::result::Result<bool, tokio::time::error::Elapsed>,
) -> ChannelHealthState {
    match result {
        Ok(true) => ChannelHealthState::Healthy,
        Ok(false) => ChannelHealthState::Unhealthy,
        Err(_) => ChannelHealthState::Timeout,
    }
}

/// Run health checks for configured channels.
/// Every channel the config actually configures, as `(key, display, channel)`.
///
/// The single construction site. It was written out separately in `doctor_channels`
/// and `start_channels_with_cancellation` (and, until it was deleted, a third copy
/// in the channel registry), and the copies had already drifted with a
/// user-visible consequence: the doctor had **no Mattermost branch**, so an
/// operator whose Mattermost bot token expired was told everything was healthy
/// while that channel silently never answered. `MattermostChannel::health_check`
/// had no live caller at all.
///
/// `key` is the lowercase `Channel::name()` value — the same identifier
/// `channels_by_name`, the per-channel allowlists and cron delivery use. `display`
/// is operator-facing. The two WhatsApp variants share the key `whatsapp` because
/// they share `Channel::name()`; they are mutually exclusive, selected by
/// `wa.mode`, so only one is ever built.
pub(crate) fn build_configured_channels(
    config: &Config,
) -> Vec<(&'static str, &'static str, Arc<dyn Channel>)> {
    let mut channels: Vec<(&'static str, &'static str, Arc<dyn Channel>)> = Vec::new();

    if let Some(ref tg) = config.channels_config.telegram {
        channels.push((
            "telegram",
            "Telegram",
            Arc::new(
                TelegramChannel::new(
                    tg.bot_token.clone(),
                    tg.allowed_users.clone(),
                    tg.mention_only,
                )
                .with_streaming(tg.stream_mode, tg.draft_update_interval_ms),
            ),
        ));
    }

    if let Some(ref dc) = config.channels_config.discord {
        channels.push((
            "discord",
            "Discord",
            Arc::new(DiscordChannel::new(
                dc.bot_token.clone(),
                dc.guild_id.clone(),
                dc.allowed_users.clone(),
                dc.listen_to_bots,
                dc.mention_only,
            )),
        ));
    }

    if let Some(ref sl) = config.channels_config.slack {
        // Socket Mode is not implemented — the channel polls
        // `conversations.history`. Say so rather than accepting an app-level
        // token in silence: an operator who supplied one is entitled to know
        // it changes nothing, and a silent no-op is how this key went
        // unnoticed for as long as it did.
        if sl
            .app_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
        {
            tracing::warn!(
                "Slack: `app_token` is set but ignored — this build polls conversations.history \
                 and does not implement Socket Mode. Remove the key, or leave it for when it does."
            );
        }
        channels.push((
            "slack",
            "Slack",
            Arc::new(SlackChannel::new(
                sl.bot_token.clone(),
                sl.channel_id.clone(),
                sl.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref mm) = config.channels_config.mattermost {
        channels.push((
            "mattermost",
            "Mattermost",
            Arc::new(MattermostChannel::new(
                mm.url.clone(),
                mm.bot_token.clone(),
                mm.channel_id.clone(),
                mm.allowed_users.clone(),
                mm.thread_replies.unwrap_or(true),
                mm.mention_only.unwrap_or(false),
            )),
        ));
    }

    if let Some(ref im) = config.channels_config.imessage {
        channels.push((
            "imessage",
            "iMessage",
            Arc::new(IMessageChannel::new(im.allowed_contacts.clone())),
        ));
    }

    #[cfg(feature = "channel-matrix")]
    if let Some(ref mx) = config.channels_config.matrix {
        channels.push((
            "matrix",
            "Matrix",
            Arc::new(MatrixChannel::new_with_session_hint(
                mx.homeserver.clone(),
                mx.access_token.clone(),
                mx.room_id.clone(),
                mx.allowed_users.clone(),
                mx.user_id.clone(),
                mx.device_id.clone(),
            )),
        ));
    }

    #[cfg(not(feature = "channel-matrix"))]
    if config.channels_config.matrix.is_some() {
        tracing::warn!(
            "Matrix channel is configured but this build was compiled without `channel-matrix`; skipping Matrix health check."
        );
    }

    if let Some(ref sig) = config.channels_config.signal {
        channels.push((
            "signal",
            "Signal",
            Arc::new(SignalChannel::new(
                sig.http_url.clone(),
                sig.account.clone(),
                sig.group_id.clone(),
                sig.allowed_from.clone(),
                sig.ignore_attachments,
                sig.ignore_stories,
            )),
        ));
    }

    if let Some(ref wa) = config.channels_config.whatsapp {
        if wa.is_ambiguous_config() {
            tracing::warn!(
                "WhatsApp config has both phone_number_id and session_path set; preferring Cloud API mode. Remove one selector to avoid ambiguity."
            );
        }
        // Runtime negotiation: detect backend type from config
        match wa.backend_type() {
            "cloud" => {
                // Cloud API mode: requires phone_number_id, access_token, verify_token
                if wa.is_cloud_config() {
                    channels.push((
                        "whatsapp",
                        "WhatsApp",
                        Arc::new(WhatsAppChannel::new(
                            wa.access_token.clone().unwrap_or_default(),
                            wa.phone_number_id.clone().unwrap_or_default(),
                            wa.verify_token.clone().unwrap_or_default(),
                            wa.allowed_numbers.clone(),
                        )),
                    ));
                } else {
                    tracing::warn!("WhatsApp Cloud API configured but missing required fields (phone_number_id, access_token, verify_token)");
                }
            }
            "web" => {
                // Web mode: requires session_path
                #[cfg(feature = "whatsapp-web")]
                if wa.is_web_config() {
                    channels.push((
                        "whatsapp",
                        "WhatsApp",
                        Arc::new(WhatsAppWebChannel::new(
                            wa.session_path.clone().unwrap_or_default(),
                            wa.pair_phone.clone(),
                            wa.pair_code.clone(),
                            wa.allowed_numbers.clone(),
                        )),
                    ));
                } else {
                    tracing::warn!("WhatsApp Web configured but session_path not set");
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    tracing::warn!("WhatsApp Web backend requires 'whatsapp-web' feature. Enable with: cargo build --features whatsapp-web");
                }
            }
            _ => {
                tracing::warn!("WhatsApp config invalid: neither phone_number_id (Cloud API) nor session_path (Web) is set");
            }
        }
    }

    if let Some(ref lq) = config.channels_config.linq {
        channels.push((
            "linq",
            "Linq",
            Arc::new(LinqChannel::new(
                lq.api_token.clone(),
                lq.from_phone.clone(),
                lq.allowed_senders.clone(),
            )),
        ));
    }

    if let Some(ref nc) = config.channels_config.nextcloud_talk {
        channels.push((
            "nextcloud_talk",
            "Nextcloud Talk",
            Arc::new(NextcloudTalkChannel::new(
                nc.base_url.clone(),
                nc.app_token.clone(),
                nc.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref email_cfg) = config.channels_config.email {
        channels.push((
            "email",
            "Email",
            Arc::new(
                EmailChannel::new(email_cfg.clone())
                    .with_approval_owners(config.channels_config.approval_owners.clone()),
            ),
        ));
    }

    if let Some(ref irc) = config.channels_config.irc {
        channels.push((
            "irc",
            "IRC",
            Arc::new(IrcChannel::new(irc::IrcChannelConfig {
                server: irc.server.clone(),
                port: irc.port,
                nickname: irc.nickname.clone(),
                username: irc.username.clone(),
                channels: irc.channels.clone(),
                allowed_users: irc.allowed_users.clone(),
                server_password: irc.server_password.clone(),
                nickserv_password: irc.nickserv_password.clone(),
                sasl_password: irc.sasl_password.clone(),
                verify_tls: irc.verify_tls.unwrap_or(true),
                allow_insecure_tls_with_password: irc.allow_insecure_tls_with_password,
                approval_owners: config.channels_config.approval_owners.clone(),
            })),
        ));
    }

    #[cfg(feature = "channel-lark")]
    if let Some(ref lk) = config.channels_config.lark {
        channels.push(("lark", "Lark", Arc::new(LarkChannel::from_config(lk))));
    }

    #[cfg(not(feature = "channel-lark"))]
    if config.channels_config.lark.is_some() {
        tracing::warn!(
            "Lark channel is configured but this build was compiled without `channel-lark`; skipping Lark health check."
        );
    }

    if let Some(ref dt) = config.channels_config.dingtalk {
        channels.push((
            "dingtalk",
            "DingTalk",
            Arc::new(DingTalkChannel::new(
                dt.client_id.clone(),
                dt.client_secret.clone(),
                dt.allowed_users.clone(),
            )),
        ));
    }

    if let Some(ref qq) = config.channels_config.qq {
        channels.push((
            "qq",
            "QQ",
            Arc::new(QQChannel::new(
                qq.app_id.clone(),
                qq.app_secret.clone(),
                qq.allowed_users.clone(),
            )),
        ));
    }

    channels
}

pub async fn doctor_channels(config: Config) -> Result<()> {
    let channels = build_configured_channels(&config);

    if channels.is_empty() {
        println!("No real-time channels configured. Run `rantaiclaw onboard` first.");
        return Ok(());
    }

    println!("🩺 RantaiClaw Channel Doctor");
    println!();

    let mut healthy = 0_u32;
    let mut unhealthy = 0_u32;
    let mut timeout = 0_u32;

    for (_key, name, channel) in channels {
        let result = tokio::time::timeout(Duration::from_secs(10), channel.health_check()).await;
        let state = classify_health_result(&result);

        match state {
            ChannelHealthState::Healthy => {
                healthy += 1;
                println!("  ✅ {name:<9} healthy");
            }
            ChannelHealthState::Unhealthy => {
                unhealthy += 1;
                println!("  ❌ {name:<9} unhealthy (auth/config/network)");
            }
            ChannelHealthState::Timeout => {
                timeout += 1;
                println!("  ⏱️  {name:<9} timed out (>10s)");
            }
        }
    }

    if config.channels_config.webhook.is_some() {
        println!("  ℹ️  Webhook   check via `rantaiclaw gateway` then GET /health");
    }

    println!();
    println!("Summary: {healthy} healthy, {unhealthy} unhealthy, {timeout} timed out");
    Ok(())
}

/// Start all configured channels and route messages to the agent
#[allow(clippy::too_many_lines)]
pub async fn start_channels(config: Config) -> Result<()> {
    // Backward-compatible entrypoint for the foreground `channels run` /
    // daemon callers (`main.rs`): they own the whole process and stop on
    // Ctrl-C, so they never need to cancel the runtime programmatically.
    // The TUI uses `start_channels_with_cancellation` instead so it can
    // restart channels in place when a channel or skill is added
    // mid-session.
    start_channels_with_cancellation(config, CancellationToken::new()).await
}

/// Build and run the channel runtime until every listener exits or
/// `shutdown` is cancelled.
///
/// Cancelling `shutdown` makes each supervised listener stop (a
/// well-behaved channel such as Telegram aborts its long-poll cleanly via
/// the same token; channels that ignore it are stopped by dropping the
/// listen future). When the listeners exit they drop their message-bus
/// senders, which closes the dispatch loop and returns `Ok(())`. This lets
/// the TUI tear the runtime down and respawn it with fresh config/skills
/// without leaking listener tasks.
pub async fn start_channels_with_cancellation(
    config: Config,
    shutdown: CancellationToken,
) -> Result<()> {
    let provider_name = resolved_default_provider(&config);
    let provider_runtime_options = providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        rantaiclaw_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        reasoning_enabled: config.runtime.reasoning_enabled,
    };
    let provider: Arc<dyn Provider> = Arc::from(
        create_resilient_provider_nonblocking(
            &provider_name,
            config.api_key.clone(),
            config.api_url.clone(),
            config.reliability.clone(),
            provider_runtime_options.clone(),
        )
        .await?,
    );

    // Warm up the provider connection pool (TLS handshake, DNS, HTTP/2 setup)
    // so the first real message doesn't hit a cold-start timeout.
    if let Err(e) = provider.warmup().await {
        tracing::warn!("Provider warmup failed (non-fatal): {e}");
    }

    // Seed the state this runtime owns. It used to go into a process-global map
    // keyed by config path, which the reload then looked up under a *separately
    // derived* key; the two derivations agreeing was an invariant nothing
    // checked. Owning one state removes the key, and with it that whole class.
    let initial_stamp = config_file_stamp(&config.config_path).await.ok();
    let runtime_config = Arc::new(Mutex::new(RuntimeConfigSlot {
        state: Some(RuntimeConfigState {
            defaults: runtime_defaults_from_config(&config),
            last_applied_stamp: initial_stamp,
            last_reload_error: None,
        }),
        ..RuntimeConfigSlot::default()
    }));

    let observer: Arc<dyn Observer> =
        Arc::from(observability::create_observer(&config.observability));
    let runtime: Arc<dyn runtime::RuntimeAdapter> =
        Arc::from(runtime::create_runtime(&config.runtime)?);
    let policy_dir = crate::profile::ProfileManager::active()
        .ok()
        .map(|p| p.policy_dir());
    let security = Arc::new(SecurityPolicy::from_config_with_policy_dir(
        &config.autonomy,
        &config.workspace_dir,
        policy_dir,
    ));
    warn_on_risky_approval_owners(&config.channels_config.approval_owners);

    // Bind an async-approval registry to the policy so shell tool
    // calls in Supervised mode can ask the user via chat reply when
    // they hit an unknown basename.
    //
    // Built with an explicit deadline, matching the tool registry below.
    // `PendingApprovals::default()` means *no* timeout — correct for the TUI,
    // where a prompt should sit until the operator acts, and wrong here: nothing
    // announces a shell request over chat, so an unanswered one blocked the
    // agent's turn forever rather than auto-denying.
    let pending = Arc::new(crate::security::PendingApprovals::new(Some(
        CHANNEL_APPROVAL_DEADLINE,
    )));
    security.set_pending(pending);
    let model = resolved_default_model(&config);
    let temperature = config.default_temperature;
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage(
        &config.memory,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        config.api_key.as_deref(),
    )?);
    let (composio_key, composio_entity_id) = if config.composio.enabled {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };
    // Build system prompt from workspace identity files + skills
    let workspace = config.workspace_dir.clone();
    let mut all_tools = tools::all_tools_with_runtime(
        Arc::new(config.clone()),
        &security,
        runtime,
        Arc::clone(&mem),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &workspace,
        &config.agents,
        config.api_key.as_deref(),
        &config,
    );

    // Merge peripheral tools (UNO Q Bridge, RPi GPIO, etc.)
    let peripheral_tools = crate::peripherals::create_peripheral_tools(&config.peripherals).await?;
    if !peripheral_tools.is_empty() {
        tracing::info!(
            count = peripheral_tools.len(),
            "Peripheral tools added to channel server"
        );
        all_tools.extend(peripheral_tools);
    }

    let tools_registry = Arc::new(all_tools);

    let skills = crate::skills::load_skills_with_config(&workspace, &config);

    // Collect tool descriptions for the prompt
    let mut tool_descs: Vec<(&str, &str)> = vec![
        (
            "shell",
            "Execute terminal commands. Use when: running local checks, build/test commands, diagnostics. Don't use when: a safer dedicated tool exists, or command is destructive without approval.",
        ),
        (
            "file_read",
            "Read file contents. Use when: inspecting project files, configs, logs. Don't use when: a targeted search is enough.",
        ),
        (
            "file_write",
            "Write file contents. Use when: applying focused edits, scaffolding files, updating docs/code. Don't use when: side effects are unclear or file ownership is uncertain.",
        ),
        (
            "memory_store",
            "Save to memory. Use when: preserving durable preferences, decisions, key context. Don't use when: information is transient/noisy/sensitive without need.",
        ),
        (
            "memory_recall",
            "Search memory. Use when: retrieving prior decisions, user preferences, historical context. Don't use when: answer is already in current context.",
        ),
        (
            "memory_forget",
            "Delete a memory entry. Use when: memory is incorrect/stale or explicitly requested for removal. Don't use when: impact is uncertain.",
        ),
    ];

    if config.browser.enabled {
        tool_descs.push((
            "browser_open",
            "Open approved HTTPS URLs in Brave Browser (allowlist-only, no scraping)",
        ));
    }
    if config.composio.enabled {
        tool_descs.push((
            "composio",
            "Execute actions on 1000+ apps via Composio (Gmail, Notion, GitHub, Slack, etc.). Use action='list' to discover actions, 'list_accounts' to retrieve connected account IDs, 'execute' to run (optionally with connected_account_id), and 'connect' for OAuth.",
        ));
    }
    tool_descs.push((
        "schedule",
        "Manage scheduled tasks (create/list/get/cancel/pause/resume). Supports recurring cron and one-shot delays.",
    ));
    tool_descs.push((
        "pushover",
        "Send a Pushover notification to your device. Requires PUSHOVER_TOKEN and PUSHOVER_USER_KEY in .env file.",
    ));
    if !config.agents.is_empty() {
        tool_descs.push((
            "delegate",
            "Delegate a subtask to a specialized agent. Use when: a task benefits from a different model (e.g. fast summarization, deep reasoning, code generation). The sub-agent runs a single prompt and returns its response.",
        ));
    }

    let bootstrap_max_chars = if config.agent.compact_context {
        Some(6000)
    } else {
        None
    };
    let native_tools = provider.supports_native_tools();
    let mut system_prompt = build_system_prompt_with_mode(
        &workspace,
        &model,
        &tool_descs,
        &skills,
        Some(&config.identity),
        bootstrap_max_chars,
        native_tools,
        config.skills.prompt_injection_mode,
    );
    if !native_tools {
        system_prompt.push_str(&build_tool_instructions(tools_registry.as_ref()));
    }

    if !skills.is_empty() {
        tracing::info!(
            "Skills loaded: {}",
            skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Collect active channels
    // One construction site: the doctor probes exactly what the runtime starts.
    // These were written out separately and had already drifted — the doctor was
    // missing Mattermost entirely.
    let channels: Vec<Arc<dyn Channel>> = build_configured_channels(&config)
        .into_iter()
        .map(|(_key, _display, channel)| channel)
        .collect();

    if channels.is_empty() {
        tracing::info!("No channels configured. Run `rantaiclaw onboard` to set up channels.");
        return Ok(());
    }

    let effective_backend = memory::effective_memory_backend_name(
        &config.memory.backend,
        Some(&config.storage.provider.config),
    );
    tracing::info!(
        "RantaiClaw Channel Server: model={} memory={} (auto-save={}) channels={}",
        model,
        effective_backend,
        if config.memory.auto_save { "on" } else { "off" },
        channels
            .iter()
            .map(|c| c.name())
            .collect::<Vec<_>>()
            .join(", ")
    );
    tracing::info!("Channel listeners running (Ctrl+C to stop)");

    crate::health::mark_component_ok("channels");

    let initial_backoff_secs = config
        .reliability
        .channel_initial_backoff_secs
        .max(DEFAULT_CHANNEL_INITIAL_BACKOFF_SECS);
    let max_backoff_secs = config
        .reliability
        .channel_max_backoff_secs
        .max(DEFAULT_CHANNEL_MAX_BACKOFF_SECS);

    // Single message bus — all channels send messages here
    let (tx, rx) = tokio::sync::mpsc::channel::<traits::ChannelMessage>(100);

    // Spawn a listener for each channel
    let mut handles = Vec::new();
    for ch in &channels {
        handles.push(spawn_supervised_listener(
            ch.clone(),
            tx.clone(),
            initial_backoff_secs,
            max_backoff_secs,
            shutdown.clone(),
        ));
    }
    drop(tx); // Drop our copy so rx closes when all channels stop

    let channels_by_name = Arc::new(
        channels
            .iter()
            .map(|ch| (ch.name().to_string(), Arc::clone(ch)))
            .collect::<HashMap<_, _>>(),
    );
    let max_in_flight_messages = compute_max_in_flight_messages(channels.len());

    tracing::info!("In-flight message limit: {max_in_flight_messages}");

    let mut provider_cache_seed: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    provider_cache_seed.insert(provider_name.clone(), Arc::clone(&provider));
    let message_timeout_secs =
        effective_channel_message_timeout_secs(config.channels_config.message_timeout_secs);
    let interrupt_on_new_message = config
        .channels_config
        .telegram
        .as_ref()
        .is_some_and(|tg| tg.interrupt_on_new_message);

    let approval_owners = Arc::new(config.channels_config.approval_owners.clone());

    // Persist conversation history across restarts only when the memory backend
    // is sqlite (it owns `brain.db`). For markdown/none backends we leave
    // persistence off so we don't create a brain.db they otherwise wouldn't have.
    // On open failure we degrade gracefully to in-memory-only history.
    let history_store: Option<Arc<history_store::ChannelHistoryStore>> =
        if config.memory.backend == "sqlite" {
            match history_store::ChannelHistoryStore::open(&config.workspace_dir) {
                Ok(store) => Some(Arc::new(store)),
                Err(e) => {
                    tracing::warn!(
                        "channel history persistence disabled (open failed): {e}; \
                         conversation history will be in-memory only"
                    );
                    None
                }
            }
        } else {
            None
        };

    // Seed the in-memory map from disk so a restart resumes live threads.
    let mut seeded_histories: HashMap<String, Vec<ChatMessage>> = HashMap::new();
    if let Some(store) = history_store.as_ref() {
        // Before loading: drop rows keyed by the pre-conversation-scope scheme
        // (unreachable now, and they hold the cross-chat transcripts that scheme
        // produced) and rows past the retention window.
        if let Err(e) = store.prune_at_startup() {
            tracing::warn!("channel history maintenance failed (non-fatal): {e}");
        }
        match store.load_all() {
            Ok(loaded) => {
                if !loaded.is_empty() {
                    tracing::info!(
                        "loaded {} persisted channel conversation(s) from brain.db",
                        loaded.len()
                    );
                }
                seeded_histories = loaded;
            }
            Err(e) => {
                tracing::warn!("failed to load persisted channel history: {e}");
            }
        }
    }

    let runtime_ctx = Arc::new(ChannelRuntimeContext {
        runtime_config,
        channels_by_name,
        provider: Arc::clone(&provider),
        default_provider: Arc::new(provider_name),
        memory: Arc::clone(&mem),
        tools_registry: Arc::clone(&tools_registry),
        observer,
        system_prompt: Arc::new(system_prompt),
        model: Arc::new(model.clone()),
        temperature,
        auto_save_memory: config.memory.auto_save,
        max_tool_iterations: config.agent.max_tool_iterations,
        min_relevance_score: config.memory.min_relevance_score,
        conversation_histories: Arc::new(Mutex::new(seeded_histories)),
        history_store,
        provider_cache: Arc::new(Mutex::new(provider_cache_seed)),
        route_overrides: Arc::new(Mutex::new(HashMap::new())),
        api_key: config.api_key.clone(),
        api_url: config.api_url.clone(),
        reliability: Arc::new(config.reliability.clone()),
        provider_runtime_options,
        workspace_dir: Arc::new(config.workspace_dir.clone()),
        message_timeout_secs,
        interrupt_on_new_message,
        multimodal: config
            .multimodal
            .clone()
            .with_runtime_workspace(config.workspace_dir.clone()),
        security: Arc::clone(&security),
        // Gate tools on polling channels unless explicitly opted into
        // unattended execution. Default (off) → deny tools that need
        // approval; the bot answers from context/RAG but won't run tools
        // for an arbitrary chat sender. Matches the gateway default.
        // Always built, never conditional on `autonomous_tools`. Whether the
        // gate is *active* is decided per message from the reloaded
        // `autonomous_tools` flag; constructing the manager here means an
        // operator can re-arm the gate live instead of needing a restart. It
        // holds the same `Arc<SecurityPolicy>` the tools hold, so a reload that
        // changes autonomy moves the gate with it.
        channel_approval: Some(Arc::new(
            crate::approval::ApprovalManager::from_config(&config.autonomy)
                .with_policy(Arc::clone(&security)),
        )),
        approval_owners: Arc::clone(&approval_owners),
        // An unanswered in-chat approval auto-denies so a forgotten prompt never
        // leaves a tool call hanging (secure default). The prompt's "Auto-deny
        // in N min" line is derived from this same value.
        tool_approvals: Arc::new(crate::security::PendingApprovals::new(Some(
            CHANNEL_APPROVAL_DEADLINE,
        ))),
        // Role ceiling for non-owner senders: safe (auto-approved) tools +
        // configured guest_allowed_tools, with shell limited to
        // guest_allowed_commands. Built once (role-based, not per-user).
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            config.autonomy.auto_approve.clone(),
            &config.channels_config.guest_allowed_tools,
            &config.channels_config.guest_allowed_commands,
        )),
    });

    run_message_dispatch_loop(rx, runtime_ctx, max_in_flight_messages).await;

    // Wait for all channel tasks
    for h in handles {
        let _ = h.await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    /// Every channel keeps its allowlist gate inside the polling loop that
    /// `listen()` runs, and no test enters that loop — the gate line can be
    /// deleted with the whole suite still green. Only Slack's is extracted into
    /// a callable function (`classify_inbound`, tested in slack.rs); the rest
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
                "fn listen(",
                "self.is_user_allowed(",
                "",
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
            (
                "qq (c2c)",
                include_str!("qq.rs"),
                "fn listen(",
                "self.is_user_allowed(user_openid)",
                "",
            ),
            (
                "qq (group)",
                include_str!("qq.rs"),
                "fn listen(",
                "self.is_user_allowed(author_id)",
                "",
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
    use crate::memory::{Memory, MemoryCategory, SqliteMemory};
    use crate::observability::NoopObserver;
    use crate::providers::{ChatMessage, Provider};
    use crate::tools::{Tool, ToolResult};
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    /// An owner turn must tell the model the sender is a verified owner so a
    /// cautious model does not self-refuse owner-only tools; a guest turn must
    /// not. The base prompt is preserved either way.
    #[test]
    fn channel_system_prompt_marks_owner_turns_only() {
        let owner = build_channel_system_prompt("BASE-PROMPT", "telegram", "12345", true);
        let guest = build_channel_system_prompt("BASE-PROMPT", "telegram", "12345", false);

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
        let p = build_channel_system_prompt("BASE", "telegram", "123456789", false);
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
        let p = build_channel_system_prompt("BASE", "irc", "#room", false);
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

        unbind_telegram_identity(&config, "*").await.unwrap();

        assert_eq!(reload_allowed_users(&cfg_path), vec!["rantaiclaw_user"]);
    }

    #[tokio::test]
    async fn unbind_telegram_normalizes_at_prefix_when_matching() {
        let tmp = TempDir::new().unwrap();
        let (config, cfg_path) = telegram_config_in(&tmp, &["rantaiclaw_user", "123456789"]);

        // Leading '@' is stripped before comparison, mirroring bind/auth.
        unbind_telegram_identity(&config, "@rantaiclaw_user")
            .await
            .unwrap();

        assert_eq!(reload_allowed_users(&cfg_path), vec!["123456789"]);
    }

    #[tokio::test]
    async fn unbind_telegram_missing_identity_is_noop_and_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let (config, cfg_path) = telegram_config_in(&tmp, &["rantaiclaw_user"]);

        unbind_telegram_identity(&config, "someone_else")
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

        let other_err =
            anyhow::anyhow!("OpenAI Codex API error (502 Bad Gateway): error code: 502");
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            append_sender_turn(&ctx, &sender, ChatMessage::user(format!("msg-{idx}")));
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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

        assert!(compact_sender_history(&ctx, &sender));

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
                || (len <= CHANNEL_HISTORY_COMPACT_CONTENT_CHARS + 3
                    && turn.content.ends_with("..."))
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
        let seeded_runtime_config = Arc::new(Mutex::new(RuntimeConfigSlot {
            state: Some(RuntimeConfigState {
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
                },
                last_applied_stamp: None,
                last_reload_error: None,
            }),
            ..RuntimeConfigSlot::default()
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
    async fn maybe_apply_runtime_config_update_hot_reloads_owners_guest_gate_and_allowed_commands()
    {
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("initial apply");
        let initial = runtime_defaults_snapshot(&ctx);
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
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("reload apply");

        let reloaded = runtime_defaults_snapshot(&ctx);
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
            live_approval_owners(&ctx).as_slice(),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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

        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("initial apply");

        // Stamp is mtime+len, so the rewrite must be distinguishable.
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_config(vec!["user_a".to_string(), "user_b".to_string()]);

        maybe_apply_runtime_config_update(&ctx)
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
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("apply despite provider build failure");

        std::thread::sleep(std::time::Duration::from_millis(10));
        // Tighten: remove user_b. This is the direction that must never stall.
        write_config(vec!["user_a".to_string()]);

        maybe_apply_runtime_config_update(&ctx)
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
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("apply despite provider build failure");
        assert_eq!(live_approval_owners(&ctx).len(), 2, "both owners applied");

        std::thread::sleep(std::time::Duration::from_millis(10));
        write_broken_provider_config(
            &config_path,
            vec!["rantaiclaw_operator".to_string()],
            Vec::new(),
            0.0,
            false,
        );
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("reload despite provider build failure");

        assert_eq!(
            live_approval_owners(&ctx).as_slice(),
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
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("apply despite provider build failure");
        let wide = runtime_defaults_snapshot(&ctx).guest_gate;
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
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("reload despite provider build failure");

        let tightened = runtime_defaults_snapshot(&ctx).guest_gate;
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
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("apply despite provider build failure");

        std::thread::sleep(std::time::Duration::from_millis(10));
        write_broken_provider_config(&config_path, Vec::new(), Vec::new(), 0.75, false);
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("reload despite provider build failure");

        let applied = runtime_defaults_snapshot(&ctx);
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
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("apply despite provider build failure");
        assert!(
            runtime_defaults_snapshot(&ctx).autonomous_tools,
            "opted out of gating"
        );

        std::thread::sleep(std::time::Duration::from_millis(10));
        write_broken_provider_config(&config_path, Vec::new(), Vec::new(), 0.0, false);
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("reload despite provider build failure");

        assert!(
            !runtime_defaults_snapshot(&ctx).autonomous_tools,
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
        maybe_apply_runtime_config_update(&ctx)
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
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("apply once the config is readable");

        assert_eq!(
            live_approval_owners(&ctx).as_slice(),
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

        let _ = runtime_defaults_snapshot(&ctx);
        assert!(
            ctx.runtime_config
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .fallback_warned,
            "taking the synthesised fallback must be reported"
        );

        // Still exactly one report after repeated per-message consultation.
        let _ = runtime_defaults_snapshot(&ctx);
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
        let ctx_a =
            allowlist_test_ctx(temp_a.path(), Arc::new(TelegramRecordingChannel::default()));
        let ctx_b =
            allowlist_test_ctx(temp_b.path(), Arc::new(TelegramRecordingChannel::default()));

        write_broken_provider_config(
            &temp_a.path().join("config.toml"),
            vec!["owner_a".to_string()],
            Vec::new(),
            0.0,
            false,
        );
        maybe_apply_runtime_config_update(&ctx_a)
            .await
            .expect("apply for runtime A");

        assert_eq!(
            live_approval_owners(&ctx_a).as_slice(),
            &["owner_a".to_string()]
        );
        assert!(
            live_approval_owners(&ctx_b).is_empty(),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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

        maybe_apply_runtime_config_update(&ctx)
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

        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("reload returns Ok even when the new provider build fails");

        // We took the keep-old-provider branch (the failure is recorded)...
        assert!(
            last_reload_error(&ctx).is_some(),
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
            runtime_defaults_snapshot(&ctx).autonomy_preset,
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
        maybe_apply_runtime_config_update(&ctx)
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("initial apply");

        // A sender pins their own provider/model in-channel — differs from the
        // default, so it is stored as an override.
        set_route_selection(
            &ctx,
            "sender-1",
            ChannelRouteSelection {
                provider: "groq".to_string(),
                model: "model-b".to_string(),
            },
        );
        assert_eq!(
            get_route_selection(&ctx, "sender-1").provider,
            "groq",
            "override is active before the switch"
        );

        // Operator switches provider in the Web UI → config.toml changes.
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_config("deepseek", "model-c");
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("reload apply");

        // The pinned sender must now follow the new default (override cleared).
        let route = get_route_selection(&ctx, "sender-1");
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
    async fn maybe_apply_runtime_config_update_keeps_provider_and_records_reason_on_build_failure()
    {
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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

        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("initial apply");
        assert_eq!(
            runtime_defaults_snapshot(&ctx).default_provider,
            "openrouter"
        );

        // Switch to a provider that cannot be built.
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_config("nonexistent-provider-xyz", "whatever");
        // Must return Ok (kept the old provider), not propagate the build error.
        maybe_apply_runtime_config_update(&ctx)
            .await
            .expect("reload keeps old provider on failure");

        assert_eq!(
            runtime_defaults_snapshot(&ctx).default_provider,
            "openrouter",
            "kept the working provider instead of swapping to a broken one"
        );
        assert!(
            last_reload_error(&ctx).is_some(),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
                category: crate::memory::MemoryCategory::Conversation,
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
        let completion = Arc::new(InFlightTaskCompletion::new());

        let guarded = Arc::clone(&completion);
        let handle = tokio::spawn(async move {
            let _guard = CompletionGuard(Arc::clone(&guarded));
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
            let completion = Arc::new(InFlightTaskCompletion::new());
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
            !json_candidate_starts_its_line(msg, inline),
            "a brace preceded by prose must be rejected before parsing"
        );

        let isolated = msg[inline + 1..].find('{').expect("isolated brace") + inline + 1;
        assert!(
            json_candidate_starts_its_line(msg, isolated),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
        let result = handle_command(
            crate::ChannelCommands::Add {
                channel_type: "telegram".to_string(),
            },
            &config,
        )
        .await;

        assert!(result.is_ok(), "guidance must not be reported as a failure");
        let text = channel_add_guidance("telegram");
        assert!(text.contains("telegram"), "names the requested type");
        assert!(text.contains("onboard"), "points at the command that works");
    }

    /// Same for `channel remove`.
    #[tokio::test]
    async fn channel_remove_reports_guidance_without_failing() {
        let config = Config::default();
        let result = handle_command(
            crate::ChannelCommands::Remove {
                name: "telegram".to_string(),
            },
            &config,
        )
        .await;

        assert!(result.is_ok(), "guidance must not be reported as a failure");
        let text = channel_remove_guidance("telegram");
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
        c.discord = serde_json::from_value(json!({"bot_token": "t", "allowed_users": []}))
            .expect("discord");
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
        let built: Vec<&str> = build_configured_channels(&config)
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
        let roster = channel_roster(&config);

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

    /// Feature-gated channels must be reported as unconfigured in a build that
    /// cannot run them, never silently dropped.
    #[test]
    fn feature_gated_channels_follow_the_build() {
        let config = config_with_every_channel();
        let built: Vec<&str> = build_configured_channels(&config)
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
        let built: Vec<&str> = build_configured_channels(&config)
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            requires: Default::default(),
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
        assert!(prompt
            .contains("<instruction>Always run cargo test before final response.</instruction>"));
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
            requires: Default::default(),
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
        assert!(!prompt
            .contains("<instruction>Always run cargo test before final response.</instruction>"));
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
            requires: Default::default(),
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
        mem.store("age_fact", "Age is 45", MemoryCategory::Conversation, None)
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
        // Stored under a specific conversation scope (as the channel autosave
        // now does), it must be recalled when that conversation asks.
        mem.store(
            "scoped_fact",
            "Project ships Friday",
            MemoryCategory::Conversation,
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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
            runtime_config: Arc::new(Mutex::new(RuntimeConfigSlot::default())),
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

    #[test]
    fn strip_isolated_tool_json_artifacts_removes_tool_calls_and_results() {
        let mut known_tools = HashSet::new();
        known_tools.insert("schedule".to_string());

        let input = r#"{"name":"schedule","parameters":{"action":"create","message":"test"}}
{"name":"schedule","parameters":{"action":"cancel","task_id":"test"}}
Let me create the reminder properly:
{"name":"schedule","parameters":{"action":"create","message":"Go to sleep"}}
{"result":{"task_id":"abc","status":"scheduled"}}
Done reminder set for 1:38 AM."#;

        let result = strip_isolated_tool_json_artifacts(input, &known_tools);
        let normalized = result
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            normalized,
            "Let me create the reminder properly:\nDone reminder set for 1:38 AM."
        );
    }

    #[test]
    fn strip_isolated_tool_json_artifacts_preserves_non_tool_json() {
        let mut known_tools = HashSet::new();
        known_tools.insert("shell".to_string());

        let input = r#"{"name":"profile","parameters":{"timezone":"UTC"}}
This is an example JSON object for profile settings."#;

        let result = strip_isolated_tool_json_artifacts(input, &known_tools);
        assert_eq!(result, input);
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
        let state = classify_health_result(&Ok(true));
        assert_eq!(state, ChannelHealthState::Healthy);
    }

    #[test]
    fn classify_health_ok_false() {
        let state = classify_health_result(&Ok(false));
        assert_eq!(state, ChannelHealthState::Unhealthy);
    }

    #[tokio::test]
    async fn classify_health_timeout() {
        let result = tokio::time::timeout(Duration::from_millis(1), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            true
        })
        .await;
        let state = classify_health_result(&result);
        assert_eq!(state, ChannelHealthState::Timeout);
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
        let handle = spawn_supervised_listener(channel, tx, 1, 1, CancellationToken::new());

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
        let handle = spawn_supervised_listener_with_health_interval(
            channel,
            tx,
            1,
            1,
            Duration::from_millis(20),
            CancellationToken::new(),
        );

        tokio::time::sleep(Duration::from_millis(35)).await;
        let first_last_ok = crate::health::snapshot_json()["components"][&component_name]
            ["last_ok"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(!first_last_ok.is_empty());

        tokio::time::sleep(Duration::from_millis(70)).await;
        let second_last_ok = crate::health::snapshot_json()["components"][&component_name]
            ["last_ok"]
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
        let handle = spawn_supervised_listener(channel, tx, 1, 60, shutdown.clone());

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
}
