//! The message dispatch core: one inbound message end to end, and the loop that
//! runs them.
//!
//! Moved out of `mod.rs` verbatim (plan 121, row 10). No behaviour change. Its
//! tests stayed in `mod_tests.rs` with the fixtures they share, so the moved
//! items are `pub(crate)`.

use super::traits::{self, SendMessage};
use super::truncate_with_ellipsis;
use super::{
    approval_relay, channel_message_timeout_budget_secs, commands, conversation, history, prompt,
    routing, sanitize, supervisor, ChannelRuntimeContext, AUTOSAVE_MIN_MESSAGE_CHARS,
    FAILED_TURN_MARKER, IN_FLIGHT_COMPLETION_WAIT_TIMEOUT, MEMORY_CONTEXT_ENTRY_MAX_CHARS,
    MEMORY_CONTEXT_MAX_CHARS, MEMORY_CONTEXT_MAX_ENTRIES, TIMED_OUT_TURN_MARKER,
    UNDELIVERED_TURN_MARKER,
};
use crate::agent::loop_::run_tool_call_loop;
use crate::memory::Memory;
use crate::providers::{self, ChatMessage};
use anyhow::Result;
use std::collections::HashMap;
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
/// `chat_id[:thread_id]`, Discord/Slack `channel_id`), and it is stable per
/// conversation — checked against Telegram, Discord, Slack and Matrix before
/// this was adopted. Matrix sets `reply_target` to the sender, but a Matrix
/// channel is pinned to one configured room, so there is only ever one
/// conversation there and nothing merges.
///
/// Route overrides use this same value, so a `/model` pin follows the
/// conversation rather than following the person into every chat they are in.
pub(crate) fn conversation_history_key(msg: &traits::ChannelMessage) -> String {
    conversation::ConversationKey::new(&msg.channel, &msg.reply_target)
        .in_thread(msg.thread_ts.as_deref())
        .resolve()
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

pub(crate) async fn process_channel_message(
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
    if let Err(err) = routing::maybe_apply_runtime_config_update(ctx.as_ref()).await {
        tracing::warn!("Failed to apply runtime config update: {err}");
    }
    if commands::handle_runtime_command_if_needed(ctx.as_ref(), &msg, target_channel.as_ref()).await
    {
        return;
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
    let system_prompt = prompt::build_channel_system_prompt(
        &base_prompt,
        &msg.channel,
        &msg.reply_target,
        sender_is_owner,
        // The channel declares its own media support. A channel that cannot
        // deliver an attachment must not be told it can, or the model emits
        // markers that reach the user as literal text.
        ctx.channels_by_name
            .get(&msg.channel)
            .and_then(|channel| channel.delivery_instructions()),
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
        (Some(channel), Some(token)) => Some(supervisor::spawn_scoped_typing_task(
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
        supervisor::log_worker_join_result(handle.await);
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
                ms = elapsed_ms,
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

            history::append_sender_turn(
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

pub(crate) async fn run_message_dispatch_loop(
    mut rx: tokio::sync::mpsc::Receiver<traits::ChannelMessage>,
    ctx: Arc<ChannelRuntimeContext>,
    max_in_flight_messages: usize,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_in_flight_messages));
    let mut workers = tokio::task::JoinSet::new();
    let in_flight_by_sender = Arc::new(tokio::sync::Mutex::new(HashMap::<
        String,
        supervisor::InFlightSenderTaskState,
    >::new()));
    let task_sequence = Arc::new(AtomicU64::new(1));

    while let Some(mut msg) = rx.recv().await {
        // One place decides whether replies thread. Channels fill `thread_ts`
        // unconditionally; clearing it here — before the message reaches the
        // agent, the approval relay, or history — means a channel added later
        // cannot forget to honour the switch, and the ten dispatch sites that
        // copy `thread_ts` onto the outbound message need no change.
        if !routing::thread_replies_enabled(ctx.as_ref(), &msg.channel) {
            msg.thread_ts = None;
        }
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
            supervisor::log_worker_join_result(result);
        }
    }

    while let Some(result) = workers.join_next().await {
        supervisor::log_worker_join_result(result);
    }
}
