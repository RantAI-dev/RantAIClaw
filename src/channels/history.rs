//! Per-sender conversation history: the in-memory map and its write-through to
//! the durable store.
//!
//! Moved out of `mod.rs` verbatim (plan 121, row 4). No behaviour change.

use super::{
    truncate_with_ellipsis, ChannelRuntimeContext, CHANNEL_HISTORY_COMPACT_CONTENT_CHARS,
    CHANNEL_HISTORY_COMPACT_KEEP_MESSAGES, MAX_CHANNEL_HISTORY,
};
use crate::providers::ChatMessage;

pub(crate) fn clear_sender_history(ctx: &ChannelRuntimeContext, sender_key: &str) {
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

pub(crate) fn compact_sender_history(ctx: &ChannelRuntimeContext, sender_key: &str) -> bool {
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
    let mut compacted =
        super::dispatch::normalize_cached_channel_turns(turns[keep_from..].to_vec());

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
pub(crate) fn persist_sender_turns(
    ctx: &ChannelRuntimeContext,
    sender_key: &str,
    turns: &[ChatMessage],
) {
    if let Some(store) = ctx.history_store.as_ref() {
        if let Err(e) = store.save(sender_key, turns) {
            tracing::warn!("failed to persist channel history for {sender_key}: {e}");
        }
    }
}

pub(crate) fn append_sender_turn(ctx: &ChannelRuntimeContext, sender_key: &str, turn: ChatMessage) {
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
