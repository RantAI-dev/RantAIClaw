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

pub mod admin;

// Four of the module's ten external symbols live in the CLI surface, so they
// keep their `crate::channels::` path.
pub use admin::doctor_channels;
// `main.rs` declares its own `mod channels`, so the binary reaches these
// through `channels::` while the library target does not use them — which the
// lib-only lint pass reads as an unused import. The binary's use is real; the
// allow is about which target is being compiled, not about dead code.
#[allow(unused_imports)]
pub(crate) use admin::{
    announce_daemon_reload, channel_roster, handle_command, reload_managed_daemon,
};
pub mod approval_relay;
pub mod auto_start_state;
pub mod cli;
pub mod commands;
pub mod conversation;
pub mod dingtalk;
pub mod discord;
pub mod dispatch;
pub mod email_channel;
pub mod factory;

// `src/cron/scheduler.rs` builds channels to resolve a delivery target, so the
// construction table keeps its `crate::channels::` path.
pub(crate) use factory::build_configured_channels;
pub mod format;
pub mod history;
mod history_store;
pub mod imessage;
pub mod irc;
#[cfg(feature = "channel-lark")]
pub mod lark;
pub mod linq;
#[cfg(feature = "channel-matrix")]
pub mod matrix;
pub mod mattermost;
pub mod media;
pub mod nextcloud_talk;
pub mod pairing;
pub mod prompt;
pub mod routing;
pub mod supervisor;

// The prompt builders are part of this module's external surface (`src/agent`
// and `src/cron` call them), so they keep their `crate::channels::` path.
pub use prompt::{
    build_system_prompt, build_system_prompt_with_mode, channel_supports_announce_delivery,
};
pub mod qq;
pub mod qr_terminal;
pub mod sanitize;
pub mod signal;
pub mod slack;
pub mod telegram;
pub mod traits;
pub mod whatsapp;
#[cfg(feature = "whatsapp-web")]
pub mod whatsapp_http;
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
/// `supervisor::CompletionGuard` releases the signal even on a panic, so this should never
/// fire. It exists because the wait sits on the path that drains the shared
/// message bus for **all** channels: if it ever hangs, every platform goes quiet
/// with nothing logged. Generous enough not to cut a legitimately slow turn
/// short — the turn budget bounds that separately.
const IN_FLIGHT_COMPLETION_WAIT_TIMEOUT_SECS: u64 = 120;
const IN_FLIGHT_COMPLETION_WAIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(IN_FLIGHT_COMPLETION_WAIT_TIMEOUT_SECS);
const CHANNEL_HEALTH_HEARTBEAT_SECS: u64 = 30;

/// How long one `health_check()` may take before the probe counts as failed.
///
/// The probe is a network round trip on sixteen channels, so it needs a bound —
/// a platform that accepts the connection and never answers would otherwise
/// leave the channel's status frozen at whatever it last reported.
const CHANNEL_HEALTH_PROBE_TIMEOUT_SECS: u64 = 10;

/// Consecutive failed probes before a channel is reported unhealthy.
///
/// One failure is a blip — a dropped packet, a rate-limit, a platform's own
/// hiccup — and flapping the status on those makes the health surface worth
/// less than no surface at all. Three misses at the heartbeat interval is
/// ninety seconds of a channel genuinely not answering.
const CHANNEL_HEALTH_FAILURE_THRESHOLD: u32 = 3;

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

pub(crate) fn effective_channel_message_timeout_secs(configured: u64) -> u64 {
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
pub(crate) struct ChannelRouteSelection {
    provider: String,
    model: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ChannelRuntimeDefaults {
    pub(crate) default_provider: String,
    pub(crate) model: String,
    pub(crate) temperature: f64,
    pub(crate) api_key: Option<String>,
    pub(crate) api_url: Option<String>,
    pub(crate) reliability: crate::config::ReliabilityConfig,
    /// Senders authorized to approve over a channel
    /// (`[channels_config] approval_owners`). Reloaded from disk so owner
    /// changes apply without a `channels run` restart. `Arc` keeps `.clone()`
    /// cheap.
    pub(crate) approval_owners: Arc<Vec<String>>,
    /// Per-role capability ceiling for non-owners. Rebuilt from config on
    /// reload so guest tool/command allowances apply without a restart.
    pub(crate) guest_gate: Arc<crate::approval::GuestGate>,
    /// Autonomy shell-command basenames (`[autonomy] allowed_commands`). Synced
    /// into the live `SecurityPolicy` runtime allowlist on reload so
    /// owner-added commands take effect without a restart.
    pub(crate) allowed_commands: Arc<Vec<String>>,
    /// Autonomy level (`[autonomy] level`). Hot-swapped into the live
    /// `SecurityPolicy` on reload so `rantaiclaw autonomy <preset>` (e.g.
    /// Off → Full) applies without a `channels run`/daemon restart.
    pub(crate) autonomy_level: crate::security::AutonomyLevel,
    /// Active approval preset, refreshed on reload. The channel system prompt
    /// is built once at startup (it reads bootstrap files and skills off disk),
    /// so without carrying this the prompt kept describing whatever preset was
    /// active when the daemon started — the gate moved, the briefing did not.
    pub(crate) autonomy_preset: crate::approval::policy_writer::PolicyPreset,
    /// Per-channel sender allowlists, keyed by the same lowercase channel name
    /// used by `channels_by_name`. Reloaded from disk so a console or CLI
    /// allowlist edit reaches a running listener without restarting the daemon —
    /// which, since the daemon hosts the gateway, used to kill the request that
    /// made the edit.
    pub(crate) allowlists: Arc<HashMap<String, Vec<String>>>,
    /// Per-message behaviour knobs that used to be read from the boot-time
    /// `ctx`, so editing them in `config.toml` did nothing until a restart even
    /// though the reload reported success. They carry no gate semantics — the
    /// timeout bounds a turn, the iteration cap bounds a tool loop — so they
    /// live here purely to close the "reload said applied, nothing changed" gap.
    pub(crate) message_timeout_secs: u64,
    pub(crate) max_tool_iterations: usize,
    pub(crate) auto_save_memory: bool,
    pub(crate) min_relevance_score: f64,
    /// `[channels_config] autonomous_tools`. Reloaded so an operator can re-arm
    /// the in-chat approval gate without a restart. This is the security-relevant
    /// direction: `false` means "gate tools", and before this it was read once at
    /// startup, so turning the gate back **on** was reported as applied and did
    /// nothing. The `ApprovalManager` itself is now always constructed at boot,
    /// so flipping this flag costs nothing per message.
    pub(crate) autonomous_tools: bool,
    /// Per-channel `mention_only`, carried only so reload can detect an edit it
    /// cannot apply and say so. See `channel_mention_only`.
    pub(crate) mention_only: Arc<HashMap<String, bool>>,
    /// Whether replies thread, per channel: the shared
    /// `[channels_config] thread_replies` with any per-channel override
    /// applied. Reloaded like the rest so the switch takes effect without a
    /// restart — threading moves where replies appear, and an operator turning
    /// it off should not have to bounce the daemon.
    pub(crate) thread_replies: Arc<HashMap<String, bool>>,
}

pub(crate) const SYSTEMD_STATUS_ARGS: [&str; 3] = ["--user", "is-active", "rantaiclaw.service"];
pub(crate) const SYSTEMD_RESTART_ARGS: [&str; 3] = ["--user", "restart", "rantaiclaw.service"];
pub(crate) const OPENRC_STATUS_ARGS: [&str; 2] = ["rantaiclaw", "status"];
pub(crate) const OPENRC_RESTART_ARGS: [&str; 2] = ["rantaiclaw", "restart"];

#[derive(Clone)]
pub(crate) struct ChannelRuntimeContext {
    /// Reloaded config state for *this* runtime's config file.
    ///
    /// Was a process-global `HashMap<PathBuf, _>` keyed by config path. Entries
    /// were inserted and never removed, a gateway and a channel runtime in one
    /// process shared and clobbered each other's entry, and every test that
    /// touched it was order-dependent on every other. One context owns one
    /// state; `None` means nothing has been loaded yet, which is the same
    /// condition the old "no entry for this path" fallback keyed on.
    pub(crate) runtime_config: Arc<Mutex<routing::RuntimeConfigSlot>>,
    pub(crate) channels_by_name: Arc<HashMap<String, Arc<dyn Channel>>>,
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) default_provider: Arc<String>,
    pub(crate) memory: Arc<dyn Memory>,
    pub(crate) tools_registry: Arc<Vec<Box<dyn Tool>>>,
    pub(crate) observer: Arc<dyn Observer>,
    pub(crate) system_prompt: Arc<String>,
    pub(crate) model: Arc<String>,
    pub(crate) temperature: f64,
    pub(crate) auto_save_memory: bool,
    pub(crate) max_tool_iterations: usize,
    pub(crate) min_relevance_score: f64,
    pub(crate) conversation_histories: ConversationHistoryMap,
    /// Durable backing for `conversation_histories`. `Some` persists each
    /// in-memory mutation to `brain.db` (and seeds the map at startup) so
    /// conversation threads survive daemon restarts. `None` means persistence
    /// is disabled (non-sqlite memory backends, or an open failure) and history
    /// stays in-memory only, exactly as before.
    pub(crate) history_store: Option<Arc<history_store::ChannelHistoryStore>>,
    pub(crate) provider_cache: ProviderCacheMap,
    pub(crate) route_overrides: RouteSelectionMap,
    pub(crate) api_key: Option<String>,
    pub(crate) api_url: Option<String>,
    pub(crate) reliability: Arc<crate::config::ReliabilityConfig>,
    pub(crate) provider_runtime_options: providers::ProviderRuntimeOptions,
    pub(crate) workspace_dir: Arc<PathBuf>,
    pub(crate) message_timeout_secs: u64,
    pub(crate) interrupt_on_new_message: bool,
    pub(crate) multimodal: crate::config::MultimodalConfig,
    /// Shared security policy. Carries the runtime allowlist + bound
    /// `PendingApprovals` registry. Read by the approval-reply parser
    /// before each inbound message is routed to the agent.
    pub(crate) security: Arc<crate::security::SecurityPolicy>,
    /// Per-tool approval gate for polling channels. `Some` (default) means
    /// tools that need approval at the current autonomy level are denied —
    /// polling channels do NOT run tools unattended. `None` only when
    /// `[channels_config] autonomous_tools = true`, restoring the
    /// run-everything behaviour. The shared session-allowlist stays empty
    /// (channels never grant interactive approval), so it's safe to share
    /// one manager across senders.
    pub(crate) channel_approval: Option<Arc<crate::approval::ApprovalManager>>,
    /// Senders authorized to APPROVE tool calls over a channel
    /// (`[channels_config] approval_owners`). Empty ⇒ nobody can approve, so the
    /// in-chat relay is never offered and approval-required tools auto-deny.
    /// Shared with the dispatch loop's reply parser.
    pub(crate) approval_owners: Arc<Vec<String>>,
    /// Dedicated registry for in-chat whole-tool approvals (Layer A). Separate
    /// from the shell allowlist `PendingApprovals` on `security`. The per-message
    /// [`ChatRelayApprovalBackend`] registers + awaits here; the dispatch loop's
    /// `try_handle_tool_reply` resolves it when an owner replies.
    pub(crate) tool_approvals: Arc<crate::security::PendingApprovals>,
    /// Per-role capability ceiling applied to non-owner ("guest") senders. The
    /// ceiling is role-based (same for every guest), so it's built once from
    /// config; a turn uses it only when the sender isn't an owner. Owners get
    /// the full toolset.
    pub(crate) guest_gate: Arc<crate::approval::GuestGate>,
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
    let provider_name = routing::resolved_default_provider(&config);
    let provider_runtime_options = providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        rantaiclaw_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        reasoning_enabled: config.runtime.reasoning_enabled,
    };
    let provider: Arc<dyn Provider> = Arc::from(
        routing::create_resilient_provider_nonblocking(
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
    let initial_stamp = routing::config_file_stamp(&config.config_path).await.ok();
    let runtime_config = Arc::new(Mutex::new(routing::RuntimeConfigSlot {
        state: Some(routing::RuntimeConfigState {
            defaults: routing::runtime_defaults_from_config(&config),
            last_applied_stamp: initial_stamp,
            last_reload_error: None,
        }),
        ..routing::RuntimeConfigSlot::default()
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
    admin::warn_on_risky_approval_owners(&config.channels_config.approval_owners);

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
    let model = routing::resolved_default_model(&config);
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
    let channels: Vec<Arc<dyn Channel>> = factory::build_configured_channels(&config)
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
        handles.push(supervisor::spawn_supervised_listener(
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
    let max_in_flight_messages = supervisor::compute_max_in_flight_messages(channels.len());

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

    dispatch::run_message_dispatch_loop(rx, runtime_ctx, max_in_flight_messages).await;

    // Wait for all channel tasks
    for h in handles {
        let _ = h.await;
    }

    Ok(())
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
