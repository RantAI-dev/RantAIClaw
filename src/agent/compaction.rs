//! Context-window compaction. Folds older turns into a structured
//! markdown summary so the agent can continue past its context budget
//! without losing the thread.
//!
//! Modeled on Claude Code's `/compact`, OpenAI Codex CLI's `/compact`,
//! and aider's `/summarize`: section-structured summary that surfaces
//! decisions, state touched, open questions, and the most recent
//! thread separately, so the agent can pick those up selectively
//! rather than rehydrating from a free-form narrative.
//!
//! Entry point: [`Agent::compact_streaming`]. Reuses the existing
//! tools-disabled provider call pattern (same shape as
//! `force_final_summary` in `loop_.rs`) — no new networking surface.

use crate::providers::{ChatMessage, ChatRequest, ConversationMessage, ToolCall};

/// System prompt that ships with every compaction request. Kept here
/// rather than at the call site so all callers get the same shape.
///
/// SOTA-aligned: explicitly asks for five sections, names them, and
/// requires `_None._` for empty sections so the format stays
/// machine-parseable.
pub(crate) const COMPACTION_SYSTEM_PROMPT: &str = "\
You are summarizing an agent conversation for context handoff. The \
user's agent has accumulated history and the context window is \
filling up. Produce a structured markdown summary that lets the \
agent pick up seamlessly from where it left off.\n\
\n\
Output exactly these five sections, no preamble, no closing remarks:\n\
\n\
## Summary\n\
2-4 sentence narrative of what's been worked on overall.\n\
\n\
## Key facts established\n\
- Decisions made, preferences surfaced, values discovered. \
  Include filenames, function names, command flags, configuration \
  values. Prefix each bullet with a short `[label]` so the agent can \
  scan quickly.\n\
\n\
## State touched\n\
- Files read, modified, or created. Commands of note. Tool calls \
  that changed system state. One bullet per concrete change.\n\
\n\
## Open questions / TODOs\n\
- Threads left dangling. Things the user mentioned but did not \
  pursue. Errors hit that may need follow-up.\n\
\n\
## Most recent thread\n\
1-2 sentences on what the user was just trying to do, so the \
agent does not lose the current task focus when the older turns \
are dropped.\n\
\n\
Be concrete and grounded. Cite filenames, function names, flags. \
Never invent details. If a section has nothing to report, write \
exactly `_None._` under its heading.";

/// User prompt that wraps the conversation excerpt. The actual
/// historical messages are sent as their own chat turns *before* this
/// prompt so the model sees them as real conversation context, not as
/// stringified text — gives noticeably better summaries on most
/// providers.
pub(crate) const COMPACTION_USER_PROMPT: &str = "\
Above is the portion of our conversation that needs to be \
compacted. Produce the summary as instructed in the system message.";

/// Walk `history` from the end and find the index where the
/// (`keep_last`)-th user message from the back sits. That index is
/// the boundary: everything strictly before it is compacted,
/// everything at-or-after is preserved verbatim.
///
/// Returns `None` if fewer than `keep_last + 1` user messages exist
/// (nothing meaningful to compact yet).
pub(crate) fn compute_split_index(
    history: &[ConversationMessage],
    keep_last: usize,
) -> Option<usize> {
    let mut user_count: usize = 0;
    for (idx, msg) in history.iter().enumerate().rev() {
        if let ConversationMessage::Chat(c) = msg {
            if c.role == "user" {
                user_count += 1;
                if user_count == keep_last {
                    // Split just before this user message — anything
                    // strictly older gets compacted.
                    return Some(idx);
                }
            }
        }
    }
    // Not enough user messages to keep the requested count — nothing
    // older than the kept range to compact.
    None
}

/// Convert a slice of `ConversationMessage` into the flattened
/// `ChatMessage` form the provider sees during compaction. Drops the
/// original system prompt (the compaction system prompt takes its
/// place), renders tool calls as inline assistant text, and renders
/// tool results as a synthetic user message — preserves enough
/// signal for the summarizer to mention "agent ran `cargo test`"
/// without forcing the provider to support tool-call shapes
/// out-of-band.
pub(crate) fn flatten_for_summary(history: &[ConversationMessage]) -> Vec<ChatMessage> {
    history
        .iter()
        .filter_map(|m| match m {
            ConversationMessage::Chat(c) if c.role == "system" => None,
            ConversationMessage::Chat(c) => Some(c.clone()),
            ConversationMessage::AssistantToolCalls { text, tool_calls } => {
                let mut s = String::new();
                if let Some(t) = text {
                    if !t.is_empty() {
                        s.push_str(t);
                        s.push('\n');
                    }
                }
                for tc in tool_calls {
                    s.push_str(&render_tool_call(tc));
                    s.push('\n');
                }
                if s.is_empty() {
                    None
                } else {
                    Some(ChatMessage::assistant(s.trim_end().to_string()))
                }
            }
            ConversationMessage::ToolResults(results) => {
                let mut s = String::from("[tool results]\n");
                for r in results {
                    let preview = preview(&r.content, 400);
                    s.push_str(&format!("- ({}): {preview}\n", r.tool_call_id));
                }
                Some(ChatMessage::user(s.trim_end().to_string()))
            }
        })
        .collect()
}

fn render_tool_call(tc: &ToolCall) -> String {
    let args_preview = preview(&tc.arguments, 200);
    format!("[tool call: {}({})]", tc.name, args_preview)
}

fn preview(s: &str, max: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(max).collect();
        format!("{truncated}…")
    }
}

/// Build the message sequence that gets sent to the provider for the
/// compaction side call. Layout:
///   1. compaction system prompt (sections + format rules)
///   2. flattened to-be-compacted conversation
///   3. closing user prompt asking for the summary
pub(crate) fn build_side_request(to_compact: &[ConversationMessage]) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(to_compact.len() + 2);
    out.push(ChatMessage::system(COMPACTION_SYSTEM_PROMPT.to_string()));
    out.extend(flatten_for_summary(to_compact));
    out.push(ChatMessage::user(COMPACTION_USER_PROMPT.to_string()));
    out
}

/// Same as [`build_side_request`] but for callers that already hold a
/// flat `Vec<ChatMessage>` (the CLI agent loop in `loop_.rs`). The
/// original system prompt is dropped — the compaction system prompt
/// takes its place — so the model gets exactly one system message
/// telling it how to summarise.
pub(crate) fn build_chat_side_request(to_compact: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(to_compact.len() + 2);
    out.push(ChatMessage::system(COMPACTION_SYSTEM_PROMPT.to_string()));
    out.extend(to_compact.iter().filter(|m| m.role != "system").cloned());
    out.push(ChatMessage::user(COMPACTION_USER_PROMPT.to_string()));
    out
}

/// Wrap a freshly-generated summary in a single system message that
/// gets injected back into the history right after the agent's own
/// system prompt. Marker text on the boundaries so a future
/// `/uncompact` (or human reader) can find the seam.
pub(crate) fn summary_envelope(summary: &str) -> ChatMessage {
    ChatMessage::system(format!(
        "[Compacted summary of earlier conversation]\n\n{}\n\n[End compacted summary]",
        summary.trim()
    ))
}

/// Stream a summary as `Chunk` events to the supplied sender so the
/// TUI sees the summary appear progressively, the same way it sees a
/// regular streaming reply. Cancelling mid-stream is honoured.
pub(crate) async fn stream_summary_as_chunks(
    summary: &str,
    events: Option<&crate::agent::events::AgentEventSender>,
) {
    let Some(tx) = events else { return };
    let mut buf = String::new();
    for word in summary.split_inclusive(char::is_whitespace) {
        buf.push_str(word);
        if buf.len() >= 64 {
            let piece = std::mem::take(&mut buf);
            if tx
                .send(crate::agent::events::AgentEvent::Chunk(piece))
                .await
                .is_err()
            {
                return;
            }
        }
    }
    if !buf.is_empty() {
        let _ = tx.send(crate::agent::events::AgentEvent::Chunk(buf)).await;
    }
}

/// Result of a successful compaction. Returned to the actor, which
/// uses it to emit `AgentEvent::CompactionComplete`.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// Markdown summary the model produced.
    pub summary: String,
    /// Number of `ConversationMessage` entries before compaction.
    pub original_count: usize,
    /// Number of recent `ConversationMessage` entries preserved verbatim.
    pub kept_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ToolResultMessage;

    fn chat(role: &str, content: &str) -> ConversationMessage {
        ConversationMessage::Chat(ChatMessage {
            role: role.into(),
            content: content.into(),
        })
    }

    #[test]
    fn compute_split_keeps_last_n_user_turns() {
        // 4 user messages: u1, u2, u3, u4. keep_last=2 should split
        // before u3 so [u3, a3, u4, a4] survive.
        let history = vec![
            chat("system", "you are an agent"),
            chat("user", "u1"),
            chat("assistant", "a1"),
            chat("user", "u2"),
            chat("assistant", "a2"),
            chat("user", "u3"),
            chat("assistant", "a3"),
            chat("user", "u4"),
            chat("assistant", "a4"),
        ];
        let split = compute_split_index(&history, 2).expect("should compact");
        // u3 is at index 5
        assert_eq!(split, 5);
        // Confirm the kept slice starts with u3.
        let kept = &history[split..];
        match &kept[0] {
            ConversationMessage::Chat(c) => assert_eq!(c.content, "u3"),
            _ => panic!("expected chat"),
        }
    }

    #[test]
    fn compute_split_returns_none_when_history_too_short() {
        let history = vec![
            chat("system", "sys"),
            chat("user", "u1"),
            chat("assistant", "a1"),
        ];
        assert_eq!(compute_split_index(&history, 5), None);
    }

    #[test]
    fn flatten_drops_original_system_prompt() {
        let history = vec![chat("system", "ignore me"), chat("user", "hello")];
        let flat = flatten_for_summary(&history);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].role, "user");
        assert_eq!(flat[0].content, "hello");
    }

    #[test]
    fn flatten_renders_tool_calls_inline() {
        let history = vec![ConversationMessage::AssistantToolCalls {
            text: Some("running checks".into()),
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "shell".into(),
                arguments: "{\"command\":\"cargo test\"}".into(),
            }],
        }];
        let flat = flatten_for_summary(&history);
        assert_eq!(flat.len(), 1);
        assert!(flat[0].content.contains("running checks"));
        assert!(flat[0].content.contains("tool call: shell"));
        assert!(flat[0].content.contains("cargo test"));
    }

    #[test]
    fn flatten_renders_tool_results_as_user() {
        let history = vec![ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "1".into(),
            content: "exit 0\nall green".into(),
        }])];
        let flat = flatten_for_summary(&history);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].role, "user");
        assert!(flat[0].content.starts_with("[tool results]"));
        assert!(flat[0].content.contains("all green"));
    }

    #[test]
    fn build_side_request_has_system_then_history_then_closing_user() {
        let history = vec![chat("user", "hello"), chat("assistant", "hi")];
        let req = build_side_request(&history);
        assert!(req.len() >= 4);
        assert_eq!(req.first().unwrap().role, "system");
        assert_eq!(req.last().unwrap().role, "user");
        assert!(req.last().unwrap().content.contains("compacted"));
    }

    #[test]
    fn build_chat_side_request_drops_original_system_and_wraps_with_compaction_prompts() {
        let history = vec![
            ChatMessage::system("you are an agent — ignore me"),
            ChatMessage::user("first turn"),
            ChatMessage::assistant("first reply"),
        ];
        let req = build_chat_side_request(&history);
        // System prompt first, closing user prompt last, the original
        // system message filtered out — same contract as the
        // ConversationMessage-flavoured helper.
        assert_eq!(req.first().unwrap().role, "system");
        assert!(req.first().unwrap().content.contains("summarizing"));
        assert_eq!(req.last().unwrap().role, "user");
        assert!(req.last().unwrap().content.contains("compacted"));
        // Should be: compaction-system + 2 history + closing-user = 4.
        assert_eq!(req.len(), 4);
        // Original system message must not appear.
        assert!(!req.iter().any(|m| m.content.contains("ignore me")));
    }

    #[test]
    fn summary_envelope_wraps_with_boundary_markers() {
        let env = summary_envelope("## Summary\nDid stuff.");
        assert!(env
            .content
            .contains("[Compacted summary of earlier conversation]"));
        assert!(env.content.contains("[End compacted summary]"));
        assert!(env.content.contains("Did stuff."));
    }
}
