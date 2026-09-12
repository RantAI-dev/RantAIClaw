//! The message dispatch core: one inbound message end to end, and the loop that
//! runs them.
//!
//! Moved out of `mod.rs` verbatim (plan 121, row 10). No behaviour change. Its
//! tests stayed in `mod_tests.rs` with the fixtures they share, so the moved
//! items are `pub(crate)`.

use super::traits;
use super::{
    approval_relay, channel_message_timeout_budget_secs, commands, conversation, history, media,
    prompt, routing, sanitize, supervisor, ChannelRuntimeContext, AUTOSAVE_MIN_MESSAGE_CHARS,
    CHANNEL_DRAIN_DEADLINE, CHANNEL_NOTICE_SEND_TIMEOUT, FAILED_TURN_MARKER,
    IN_FLIGHT_COMPLETION_WAIT_TIMEOUT, MEMORY_CONTEXT_ENTRY_MAX_CHARS, MEMORY_CONTEXT_MAX_CHARS,
    MEMORY_CONTEXT_MAX_ENTRIES, RESTART_NOTICE, TIMED_OUT_TURN_MARKER, UNDELIVERED_ATTACHMENT_NOTE,
    UNDELIVERED_TURN_MARKER,
};
use crate::agent::loop_::run_tool_call_loop;
use crate::memory::Memory;
use crate::providers::{self, ChatMessage};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub(crate) fn conversation_memory_key(msg: &traits::ChannelMessage) -> String {
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
/// `chat_id[:thread_id]`, Discord/Slack `channel_id`). `thread_ts` narrows it to
/// a platform thread where the platform has one (Slack, Mattermost) and is empty
/// on Telegram and Discord. Matrix sets `reply_target` to the sender, but a
/// Matrix channel is pinned to one configured room, so there is only ever one
/// conversation there and nothing merges.
///
/// Nothing that changes from one message to the next may enter this key.
/// Telegram and Discord once carried the id of the prompting message in
/// `thread_ts` so replies would quote it, and every message became a
/// conversation of its own: no history past one exchange, and a `/model` choice
/// no later message read. The quote now travels in `reply_anchor`, which this
/// function does not read, and
/// `every_tier_channel_keeps_one_conversation_across_consecutive_messages`
/// checks the key through each tier channel's own parser.
///
/// Route overrides use this same value, so a `/model` pin follows the
/// conversation rather than following the person into every chat they are in.
pub(crate) fn conversation_history_key(msg: &traits::ChannelMessage) -> String {
    conversation::ConversationKey::new(&msg.channel, &msg.reply_target)
        .in_thread(msg.thread_ts.as_deref())
        .resolve()
}

/// The scope layered memory stores and recalls under.
///
/// Deliberately the **same** value as [`conversation_history_key`]: memory and
/// history describe the same conversation, and keying them differently is what
/// produced MEM-SCOPE-SENDER. This existed as an inline `ConversationKey::new`
/// built from `msg.sender` while history used `msg.reply_target`, so a private
/// DM, every group the bot shared with that person, and every forum topic
/// collapsed into one memory scope — a detail recalled from a private chat
/// could surface when they next spoke in a group. Named so a test can pin it.
pub(crate) fn conversation_memory_scope(msg: &traits::ChannelMessage) -> String {
    conversation_history_key(msg)
}

pub(crate) fn interruption_scope_key(msg: &traits::ChannelMessage) -> String {
    format!("{}_{}_{}", msg.channel, msg.reply_target, msg.sender)
}

pub(crate) fn normalize_cached_channel_turns(turns: Vec<ChatMessage>) -> Vec<ChatMessage> {
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

pub(crate) fn is_context_window_overflow_error(err: &anyhow::Error) -> bool {
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

pub(crate) async fn build_memory_context(
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
pub(crate) fn extract_tool_context_summary(history: &[ChatMessage], start_index: usize) -> String {
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
pub(crate) const CHANNEL_EMPTY_REPLY_FALLBACK: &str =
    "I worked on that but don't have a final answer to show — want me to try again?";

/// Make a reply safe to deliver to a human: strip a leading internal
/// `[Used tools: …]` annotation (that belongs in history, not the chat) and
/// substitute a graceful message when nothing meaningful remains. The tool
/// summary is still recorded separately in conversation history.
pub(crate) fn clean_delivered_reply(text: &str) -> String {
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

/// What a person saw when a reply could not be delivered (plan 355).
///
/// Every channel sends the text first and each attachment after, aborting on the
/// first failure, so the shape of the reply says what reached the chat: text plus
/// a marker means they read the text and never got the file, while a reply that
/// was only markers means they saw nothing at all. `Channel::send` returns one
/// `Result` and cannot say which, so this reads the reply dispatch just tried to
/// send instead of changing the trait.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeliveryFailure {
    /// Nothing reached the chat: either there was no attachment to blame, or the
    /// reply was only markers so no text was sent before the upload failed.
    NothingSent { files: Vec<String> },
    /// The text reached the chat; at least one of these attachments did not.
    TextDelivered { files: Vec<String> },
}

impl DeliveryFailure {
    /// Read the shape from the reply that was attempted.
    pub(crate) fn classify(reply: &str) -> Self {
        let (text, attachments) = media::parse_attachment_markers(reply);
        let files: Vec<String> = attachments
            .iter()
            .map(|attachment| attachment_display_name(&attachment.target))
            .collect();
        if files.is_empty() || text.trim().is_empty() {
            return Self::NothingSent { files };
        }
        Self::TextDelivered { files }
    }

    /// The one line the conversation is told. It never claims the answer was
    /// lost when the text went through, because the text goes first everywhere.
    pub(crate) fn notice(&self) -> String {
        let (files, tail) = match self {
            Self::NothingSent { files } if files.is_empty() => {
                return "I could not deliver my last reply. Please ask again.".to_string();
            }
            Self::NothingSent { files } => (
                files,
                "There was nothing else in that reply, so please ask again.",
            ),
            Self::TextDelivered { files } => (files, "The message above is the rest of my reply."),
        };
        format!("{} {tail}", attachment_phrase(files))
    }

    /// What history records, given the reply this turn would have recorded.
    ///
    /// The model's next turn has to work from what the person actually read, so a
    /// half-delivered reply keeps that text — the `[Used tools: …]` summary
    /// included, since it is the model's own bookkeeping — with the markers
    /// removed and a note added. The blanket marker stays for a reply that never
    /// left.
    pub(crate) fn history_entry(&self, recorded: &str) -> String {
        match self {
            Self::NothingSent { .. } => UNDELIVERED_TURN_MARKER.to_string(),
            Self::TextDelivered { .. } => {
                let (text, _) = media::parse_attachment_markers(recorded);
                format!("{text}\n{UNDELIVERED_ATTACHMENT_NOTE}")
            }
        }
    }
}

/// Which attachments to blame.
///
/// A channel uploads the markers in order and stops at the first failure, so with
/// one marker the name is certain. With several, all that is true is that at least
/// one of them did not arrive: naming them all as failed would claim a file the
/// person may well have received.
fn attachment_phrase(files: &[String]) -> String {
    match files {
        [only] => format!("I could not attach {only}."),
        _ => format!(
            "At least one attachment did not arrive ({}).",
            human_list(files)
        ),
    }
}

/// The last segment of the target: the file name for a path, and the final
/// segment of a URL. Falls back to the target as written when there is no segment
/// to take.
fn attachment_display_name(target: &str) -> String {
    std::path::Path::new(target)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(target)
        .to_string()
}

/// `a`, `a and b`, `a, b and c`.
fn human_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Tell the conversation that delivery failed (plan 355).
///
/// Best-effort and bounded like the restart notice, so a platform that does not
/// answer cannot hold the turn open, and logged by message id rather than text.
async fn send_delivery_failure_notice(
    channel: &dyn traits::Channel,
    msg: &traits::ChannelMessage,
    failure: &DeliveryFailure,
) {
    let outbound = msg.reply(failure.notice());
    let (channel_name, message_id) = (channel.name(), msg.id.as_str());
    match tokio::time::timeout(CHANNEL_NOTICE_SEND_TIMEOUT, channel.send(&outbound)).await {
        Ok(Ok(())) => {
            tracing::info!(
                channel = channel_name,
                message_id,
                "told the conversation that delivery failed"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(
                channel = channel_name,
                message_id,
                "could not send the delivery notice: {e}"
            );
        }
        Err(_) => {
            tracing::warn!(
                channel = channel_name,
                message_id,
                "delivery notice timed out"
            );
        }
    }
}

/// How a turn ended, as far as the dispatch loop needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnEnd {
    /// The turn reached something the user sees: a reply, an error message, a
    /// runtime command's answer.
    Finished,
    /// The turn was cancelled before the model answered, so the user got
    /// nothing from it.
    Cancelled,
}

pub(crate) async fn process_channel_message(
    ctx: Arc<ChannelRuntimeContext>,
    msg: traits::ChannelMessage,
    cancellation_token: CancellationToken,
) -> TurnEnd {
    if cancellation_token.is_cancelled() {
        return TurnEnd::Cancelled;
    }

    // Pre-v0.6.7 used `println!` here, which leaks into the TUI's
    // alt-screen and corrupts rendering when channels are auto-started
    // alongside `rantaiclaw` (a v0.6.6 tester saw an inbound Telegram line
    // — "[telegram] from <sender>: ..." — printed straight into the local
    // chat surface). Tracing routes to the log file in TUI
    // mode and to whatever subscriber daemon mode installs — operator
    // can `RUST_LOG=info` + tail the log file. It carries the message's id and
    // length, never its text: the journal is not the conversation (plan 352).
    tracing::info!(
        channel = %msg.channel,
        sender = %msg.sender,
        message_id = %msg.id,
        chars = msg.content.chars().count(),
        "channel message received"
    );

    let target_channel = ctx.channels_by_name.get(&msg.channel).cloned();
    if let Err(err) = routing::maybe_apply_runtime_config_update(ctx.as_ref()).await {
        tracing::warn!("Failed to apply runtime config update: {err}");
    }
    if commands::handle_runtime_command_if_needed(ctx.as_ref(), &msg, target_channel.as_ref()).await
    {
        return TurnEnd::Finished;
    }

    let history_key = conversation_history_key(&msg);
    let route = routing::get_route_selection(ctx.as_ref(), &history_key);
    let runtime_defaults = routing::runtime_defaults_snapshot(ctx.as_ref());
    let active_provider = match routing::get_or_create_provider(ctx.as_ref(), &route.provider).await
    {
        Ok(provider) => provider,
        Err(err) => {
            let safe_err = providers::sanitize_api_error(&err.to_string());
            let message = format!(
                "⚠️ Failed to initialize provider `{}`. Please run `/models` to choose another provider.\nDetails: {safe_err}",
                route.provider
            );
            if let Some(channel) = target_channel.as_ref() {
                let _ = channel.send(&msg.reply(message)).await;
            }
            return TurnEnd::Finished;
        }
    };
    // Conversation scope for layered memory: one scope per chat/thread, the same
    // identity used for history keying — literally the same function, so the two
    // cannot drift again.
    //
    // This used to build its own key from `msg.sender`, while history keyed on
    // `msg.reply_target`. The comment claimed "one scope per chat" and the code
    // gave one scope per *person*: a private DM, every group the bot shared with
    // that person, and every forum topic collapsed into one memory scope, so a
    // detail recalled from a private chat could surface when they next spoke in
    // a group. Plan 118 fixed exactly this for conversation history and recorded
    // that memory still had it.
    let conversation_scope = conversation_memory_scope(&msg);

    if runtime_defaults.auto_save_memory
        && msg.content.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS
    {
        let autosave_key = conversation_memory_key(&msg);
        // Raw inbound text, stored unread and re-injected into later prompts as
        // established context. Screen it the same way an agent-initiated write
        // is screened — this is the path where untrusted content actually
        // arrives, and nobody reviews it in between.
        crate::memory::autosave_screened(
            ctx.memory.as_ref(),
            &autosave_key,
            &msg.content,
            Some(conversation_scope.as_str()),
        )
        .await;
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
    history::append_sender_turn(ctx.as_ref(), &history_key, ChatMessage::user(&msg.content));

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
    // Re-render the persona section too, from `persona.toml` fresh, so a
    // `PUT /api/v1/personality` reaches an already-running channel listener
    // without a restart — the same per-message in-memory splice the safety
    // section uses (`ctx.system_prompt` is built once at channel start).
    let base_prompt = crate::agent::prompt::replace_persona_section(
        &base_prompt,
        &crate::agent::prompt::render_persona_section(),
    );
    // The channel declares its own media support. A channel that cannot deliver
    // an attachment must not be told it can, or the model emits markers that
    // reach the user as literal text. Bound here rather than inline because the
    // text is owned now: it names the workspace path (plan 356).
    let delivery_instructions = ctx
        .channels_by_name
        .get(&msg.channel)
        .and_then(|channel| channel.delivery_instructions(ctx.workspace_dir.as_path()));
    let system_prompt = prompt::build_channel_system_prompt(
        &base_prompt,
        &msg.channel,
        &msg.reply_target,
        sender_is_owner,
        delivery_instructions.as_deref(),
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
            match channel.send_draft(&msg.reply("...")).await {
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
        (Some(channel), Some(token)) => Some(supervisor::spawn_scoped_typing_task(
            Arc::clone(channel),
            msg.reply_target.clone(),
            // The thread the reply will land in, so a channel that shows
            // progress by posting can put it where the answer goes.
            msg.thread_ts.clone(),
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
                &msg,
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
            // Carry this turn's chat into tool execution. `ShellTool` is a
            // `Tool` and the trait has no originating message, so without this
            // a shell approval registers unscoped and cannot be answered by a
            // bare `ok` from the chat that triggered it.
            crate::security::TURN_SCOPE.scope(
            (msg.channel.clone(), msg.reply_target.clone()),
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
                ctx.ledger.as_deref(),
            ),
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
        supervisor::log_worker_join_result(handle.await);
    }

    let cancelled = matches!(llm_result, LlmExecutionResult::Cancelled);
    match llm_result {
        LlmExecutionResult::Cancelled => {
            // A newer message from the same sender, or a shutdown's drain
            // deadline. The dispatch loop knows which, and answers for the second.
            tracing::info!(
                channel = %msg.channel,
                sender = %msg.sender,
                "Cancelled an in-flight channel request"
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
                sanitize::sanitize_channel_response(&response, ctx.tools_registry.as_ref());
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
            // Moved verbatim in plan 121 row 10. `u64::try_from` rather than
            // the `as` cast the line carried: same value for any real elapsed
            // time, and the gate counts a moved line as a changed one.
            let elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            tracing::info!(
                channel = %msg.channel,
                message_id = %msg.id,
                ms = elapsed_ms,
                chars = delivered_response.chars().count(),
                "channel reply"
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
                            channel.send(&msg.reply(&delivered_response)).await.is_ok()
                        }
                    }
                } else {
                    match channel.send(&msg.reply(&delivered_response)).await {
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

            // A failed send is not silence (plan 355). The conversation is told
            // what did not arrive, and history keeps what the person actually
            // read: on every channel the text goes first and the attachment
            // after, so a blanket "not delivered" would make the model answer a
            // question the user had already been answered.
            let recorded = if delivered {
                history_response
            } else {
                let failure = DeliveryFailure::classify(&delivered_response);
                if let Some(channel) = target_channel.as_ref() {
                    send_delivery_failure_notice(channel.as_ref(), &msg, &failure).await;
                }
                failure.history_entry(&history_response)
            };
            history::append_sender_turn(
                ctx.as_ref(),
                &history_key,
                ChatMessage::assistant(recorded),
            );
        }
        LlmExecutionResult::Completed(Ok(Err(e))) => {
            if crate::agent::loop_::is_tool_loop_cancelled(&e) || cancellation_token.is_cancelled()
            {
                tracing::info!(
                    channel = %msg.channel,
                    sender = %msg.sender,
                    "Cancelled an in-flight channel request"
                );
                if let (Some(channel), Some(draft_id)) =
                    (target_channel.as_ref(), draft_message_id.as_deref())
                {
                    if let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await {
                        tracing::debug!("Failed to cancel draft on {}: {err}", channel.name());
                    }
                }
                return TurnEnd::Cancelled;
            }

            if is_context_window_overflow_error(&e) {
                let compacted = history::compact_sender_history(ctx.as_ref(), &history_key);
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
                        let _ = channel.send(&msg.reply(error_text)).await;
                    }
                }
                return TurnEnd::Finished;
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
            history::append_sender_turn(
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
                    let _ = channel.send(&msg.reply(reply)).await;
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
            history::append_sender_turn(
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
                    let _ = channel.send(&msg.reply(error_text)).await;
                }
            }
        }
    }

    if cancelled {
        TurnEnd::Cancelled
    } else {
        TurnEnd::Finished
    }
}

/// Clear a message's thread and quote when its channel has threaded replies
/// turned off.
///
/// One place decides whether replies thread. Channels fill `thread_ts` and
/// `reply_anchor` unconditionally; clearing both here — before the message
/// reaches the agent, the approval relay, or history — means a channel added
/// later cannot forget to honour the switch. Both, because Slack and Mattermost
/// thread through the first while Telegram and Discord quote through the
/// second, and every reply is built from these two fields by
/// `ChannelMessage::reply`.
fn apply_thread_setting(ctx: &ChannelRuntimeContext, msg: &mut traits::ChannelMessage) {
    if !routing::thread_replies_enabled(ctx, &msg.channel) {
        msg.thread_ts = None;
        msg.reply_anchor = None;
    }
}

/// The conversations a shutdown has already told to resend, so each gets one
/// line however many of its messages were stopped (plan 353, decision D3).
type NotifiedConversations = Arc<std::sync::Mutex<HashSet<String>>>;

/// The line that tells a conversation shutdown stopped its message before it
/// was answered. Taken from the message before its turn consumes it: the
/// conversation it belongs to, the channel to send on, the id to log, and the
/// outbound message addressed to the same chat and thread.
struct RestartNotice {
    conversation: String,
    channel: String,
    message_id: String,
    outbound: traits::SendMessage,
    notified: NotifiedConversations,
}

impl RestartNotice {
    fn for_message(msg: &traits::ChannelMessage, notified: &NotifiedConversations) -> Self {
        Self {
            conversation: conversation_history_key(msg),
            channel: msg.channel.clone(),
            message_id: msg.id.clone(),
            outbound: msg.reply(RESTART_NOTICE),
            notified: Arc::clone(notified),
        }
    }

    /// Sends nothing when the conversation was already told. Bounded by
    /// `CHANNEL_NOTICE_SEND_TIMEOUT`, so a platform that does not answer cannot
    /// use up the drain the other notices need. Logs the message id, never the
    /// text.
    async fn send(&self, ctx: &ChannelRuntimeContext) {
        let first_for_conversation = self
            .notified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(self.conversation.clone());
        if !first_for_conversation {
            return;
        }
        let Some(target) = ctx.channels_by_name.get(&self.channel) else {
            return;
        };
        let (channel, message_id) = (self.channel.as_str(), self.message_id.as_str());
        match tokio::time::timeout(CHANNEL_NOTICE_SEND_TIMEOUT, target.send(&self.outbound)).await {
            Ok(Ok(())) => tracing::info!(channel, message_id, "sent a restart notice"),
            Ok(Err(e)) => {
                tracing::warn!(channel, message_id, "could not send a restart notice: {e}");
            }
            Err(_) => tracing::warn!(channel, message_id, "a restart notice timed out"),
        }
    }

    /// Send the notice for a message that will never start, on the loop's join
    /// set so the drain waits for it.
    fn spawn_for(
        workers: &mut tokio::task::JoinSet<()>,
        ctx: &Arc<ChannelRuntimeContext>,
        msg: &traits::ChannelMessage,
        notified: &NotifiedConversations,
    ) {
        let notice = Self::for_message(msg, notified);
        let ctx = Arc::clone(ctx);
        workers.spawn(async move { notice.send(&ctx).await });
    }
}

pub(crate) async fn run_message_dispatch_loop(
    mut rx: tokio::sync::mpsc::Receiver<traits::ChannelMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    max_in_flight_messages: usize,
    // The loop used to end only when every `Sender` dropped, which was true
    // while the listeners were the only producers. The gateway now holds one
    // too (plan 313), so sender-drop alone would keep this alive until the HTTP
    // server's state is dropped — a shutdown ordering hazard. The token ends it
    // regardless of who still holds a sender.
    shutdown: CancellationToken,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_in_flight_messages));
    let mut workers = tokio::task::JoinSet::new();
    let in_flight_by_sender = Arc::new(tokio::sync::Mutex::new(HashMap::<
        String,
        supervisor::InFlightSenderTaskState,
    >::new()));
    let task_sequence = Arc::new(AtomicU64::new(1));

    // Cancelled once running turns have had `CHANNEL_DRAIN_DEADLINE` after
    // shutdown; each worker still running then stops its turn and says so.
    let stop_running_turns = CancellationToken::new();
    let notified: NotifiedConversations = Arc::default();

    while let Some(mut msg) = tokio::select! {
        biased;
        () = shutdown.cancelled() => None,
        m = rx.recv() => m,
    } {
        apply_thread_setting(ctx.as_ref(), &mut msg);
        // Intercept approval replies before the message reaches the agent.
        // Try the whole-tool relay first (`/approve X`, `/deny X` — Layer A),
        // then the shell allowlist relay (`/allow X`, `y X`, … — Layer B). Both
        // are stateless: they consult only their pending registry and return an
        // acknowledgement if the text was a recognised reply, else `None` so
        // normal chat falls through. Owner authority is enforced inside each.
        // Refresh runtime config from disk first so reply authorization reads
        // the LIVE owner list (mirrors the per-message path) — owner changes
        // apply without a `channels run` restart.
        if let Err(err) = routing::maybe_apply_runtime_config_update(ctx.as_ref()).await {
            tracing::warn!("Failed to apply runtime config update: {err}");
        }
        let live_owners = routing::live_approval_owners(ctx.as_ref());
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
                let ack = msg.reply(reply);
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

        // A message waiting for a free worker when shutdown begins never
        // starts. Its conversation is told, rather than left without an answer.
        let permit = tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                RestartNotice::spawn_for(&mut workers, &ctx, &msg, &notified);
                break;
            }
            permit = Arc::clone(&semaphore).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };

        let worker_ctx = Arc::clone(&ctx);
        let in_flight = Arc::clone(&in_flight_by_sender);
        let task_sequence = Arc::clone(&task_sequence);
        let stop_running_turns = stop_running_turns.clone();
        let notified = Arc::clone(&notified);
        workers.spawn(async move {
            let _permit = permit;
            let interrupt_enabled =
                worker_ctx.interrupt_on_new_message && msg.channel == "telegram";
            let sender_scope_key = interruption_scope_key(&msg);
            let cancellation_token = CancellationToken::new();
            let completion = Arc::new(supervisor::InFlightTaskCompletion::new());
            let task_id = task_sequence.fetch_add(1, Ordering::Relaxed);

            // Releases waiters on EVERY exit path, including a panic. Held for
            // the rest of the closure; see `supervisor::CompletionGuard`.
            let _completion_guard = supervisor::CompletionGuard(Arc::clone(&completion));

            if interrupt_enabled {
                let previous = {
                    let mut active = in_flight.lock().await;
                    active.insert(
                        sender_scope_key.clone(),
                        supervisor::InFlightSenderTaskState {
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

            // Built before the turn takes the message, in case shutdown stops it.
            let notice = RestartNotice::for_message(&msg, &notified);
            let notice_ctx = Arc::clone(&worker_ctx);

            let turn = process_channel_message(worker_ctx, msg, cancellation_token.clone());
            tokio::pin!(turn);
            let end = tokio::select! {
                end = &mut turn => end,
                () = stop_running_turns.cancelled() => {
                    cancellation_token.cancel();
                    turn.await
                }
            };
            // Answer only for a turn the drain stopped. One a newer message
            // interrupted is followed by that message's own turn.
            if end == TurnEnd::Cancelled && stop_running_turns.is_cancelled() {
                notice.send(&notice_ctx).await;
            }

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
            supervisor::log_worker_join_result(result);
        }
    }

    if !shutdown.is_cancelled() {
        // Every sender is gone, so nothing more can arrive: let turns finish.
        while let Some(result) = workers.join_next().await {
            supervisor::log_worker_join_result(result);
        }
        return;
    }

    // Shutdown. A message still queued never starts; tell its conversation.
    rx.close();
    while let Ok(mut msg) = rx.try_recv() {
        apply_thread_setting(ctx.as_ref(), &mut msg);
        RestartNotice::spawn_for(&mut workers, &ctx, &msg, &notified);
    }

    // Turns still running get until the drain deadline, then stop and say so.
    let deadline = tokio::time::sleep(CHANNEL_DRAIN_DEADLINE);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            joined = workers.join_next() => match joined {
                Some(result) => supervisor::log_worker_join_result(result),
                None => break,
            },
            () = &mut deadline, if !stop_running_turns.is_cancelled() => stop_running_turns.cancel(),
        }
    }
}
