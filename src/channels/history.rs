//! Per-sender conversation history: the in-memory map and its write-through to
//! the durable store.
//!
//! Moved out of `mod.rs` verbatim (plan 121, row 4). No behaviour change.

use super::{
    ChannelRuntimeContext, CHANNEL_HISTORY_COMPACT_CONTENT_CHARS,
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

/// Trim a compaction slice so no row is left whose body exceeds the per-message
/// char cap. Tool rows and XML `[Tool results]` user rows are erased in pairs
/// (result + preceding carrier call row) since a truncated body would let the
/// model read a partial value it cannot reason about safely; non-attached rows
/// (plain user/assistant prose) are dropped whole — there is no way to shorten
/// one without introducing ellipsis the cache contract forbids.
fn drop_overlong_rows(turns: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::with_capacity(turns.len());
    for turn in turns {
        let overlong = turn.content.chars().count() > CHANNEL_HISTORY_COMPACT_CONTENT_CHARS;
        match turn.role.as_str() {
            // A `tool` row is attached only when the row immediately before it
            // is an assistant tool-call carrier. If that carrier was already
            // dropped by an earlier pass (because we erased the previous pair),
            // the tool row is orphan and must follow the same fate.
            "tool" => {
                let attached_to_call = out
                    .last()
                    .map(|t| {
                        t.role == "assistant"
                            && crate::channels::dispatch::is_tool_call_carrier(&t.content)
                    })
                    .unwrap_or(false);
                if !attached_to_call {
                    continue;
                }
                if overlong {
                    // Drop the carrier call row we just pushed and skip this
                    // result row. The pair leaves no orphan behind.
                    out.pop();
                    continue;
                }
                out.push(turn);
            }
            "user" => {
                if turn.content.starts_with("[Tool results]")
                    || turn.content.starts_with("[Tool Results]")
                {
                    let attached_to_call = out
                        .last()
                        .map(|t| {
                            t.role == "assistant"
                                && crate::channels::dispatch::is_tool_call_carrier(&t.content)
                        })
                        .unwrap_or(false);
                    if !attached_to_call {
                        continue;
                    }
                    if overlong {
                        out.pop();
                        continue;
                    }
                    out.push(turn);
                    continue;
                }
                if overlong {
                    // Plain user prose — drop whole, no mid-row ellipsis.
                    continue;
                }
                out.push(turn);
            }
            "assistant" => {
                if overlong {
                    continue;
                }
                out.push(turn);
            }
            _ => {
                if !overlong {
                    out.push(turn);
                }
            }
        }
    }
    out
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
    let normalized = super::dispatch::normalize_cached_channel_turns(turns[keep_from..].to_vec());
    let compacted = drop_overlong_rows(normalized);

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

/// Trim a cache vector down to at most `MAX_CHANNEL_HISTORY` rows by dropping
/// whole turns from the front. A turn starts at any `user` row whose content
/// is not the XML `[Tool results]` carrier (those belong to the previous
/// tool-using turn). Drops up to but never into the next turn — the slice is
/// always a clean cut, so the next call to `normalize_cached_channel_turns`
/// never has to repair an orphan `tool` row that we ourselves created by
/// trimming half a turn.
fn trim_to_whole_turns(turns: &mut Vec<ChatMessage>) {
    loop {
        if turns.len() <= MAX_CHANNEL_HISTORY {
            return;
        }
        let next_turn_start = turns.iter().enumerate().skip(1).find_map(|(idx, turn)| {
            if turn.role == "user"
                && !turn.content.starts_with("[Tool results]")
                && !turn.content.starts_with("[Tool Results]")
            {
                Some(idx)
            } else {
                None
            }
        });
        match next_turn_start {
            Some(idx) => {
                turns.drain(0..idx);
            }
            None => {
                // No remaining turn boundary in the cache; everything left
                // belongs to a single turn we cannot shrink. Drop the whole
                // thing — a partial turn is worse than no history.
                turns.clear();
                return;
            }
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
    trim_to_whole_turns(turns);
    let snapshot = turns.clone();
    drop(histories);
    persist_sender_turns(ctx, sender_key, &snapshot);
}

/// Append a batch of turns the dispatch just produced (tool call, tool result,
/// and the recorded final assistant message), and persist the resulting snapshot
/// once. Used in place of repeated [`append_sender_turn`] calls when several
/// turns land in the same atomic dispatch, so the store writes once instead of
/// `turns.len()` times.
pub(crate) fn append_sender_turns(
    ctx: &ChannelRuntimeContext,
    sender_key: &str,
    turns: &[ChatMessage],
) {
    if turns.is_empty() {
        return;
    }
    let mut histories = ctx
        .conversation_histories
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let entry = histories.entry(sender_key.to_string()).or_default();
    entry.extend(turns.iter().cloned());
    trim_to_whole_turns(entry);
    let snapshot = entry.clone();
    drop(histories);
    persist_sender_turns(ctx, sender_key, &snapshot);
}
