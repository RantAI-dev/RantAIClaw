//! Structured events emitted during a single agent turn.
//!
//! Consumed by TUI / other callers that want richer feedback than the
//! plain `on_delta: mpsc::Sender<String>` stream used by channels.

use crate::cost::TokenUsage;

pub type AgentEventSender = tokio::sync::mpsc::Sender<AgentEvent>;

// Serde derives intentionally omitted: these events flow through in-process
// mpsc channels only. Add Serialize/Deserialize if a future caller needs to
// log or persist them.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Streaming text fragment from the provider. Multiple emitted per turn.
    Chunk(String),

    /// A tool call has started. Emitted once per call, before execution.
    ToolCallStart {
        id: String,
        name: String,
        args: serde_json::Value,
    },

    /// A tool call has finished. `output_preview` is truncated to ~500 chars.
    /// Full output still lives in the conversation history.
    ToolCallEnd {
        id: String,
        ok: bool,
        output_preview: String,
    },

    /// Usage totals for the turn. Emitted once, immediately before `Done`.
    Usage(TokenUsage),

    /// Terminal event for a turn. `cancelled=true` when `CancellationToken` fired.
    Done { final_text: String, cancelled: bool },

    /// Non-recoverable error. Followed by `Done { cancelled: false, final_text: "" }`.
    Error(String),

    /// The actor finished rebuilding the agent after a `TurnRequest::Reload`.
    /// Carries fresh snapshots that the TUI keeps cached (so views like
    /// `/mcp` reflect the new state without re-probing). Emitted at
    /// reload-complete time, not per turn.
    ReloadComplete {
        /// Names of MCP servers configured after the reload.
        mcp_servers_configured: Vec<String>,
        /// Per-server qualified tool names actually discovered.
        mcp_tools_by_server: std::collections::HashMap<String, Vec<String>>,
    },

    /// Emitted right before the actor begins a compaction so the TUI can
    /// show a "compacting…" working indicator instead of leaving the
    /// scrollback silent during the side LLM call.
    CompactionStart {
        /// Number of messages currently in the agent's conversation
        /// history (system prompt included) before compaction.
        original_count: usize,
        /// Number of trailing chat turns the compactor will preserve
        /// verbatim. Anything older is folded into the summary.
        keep_last: usize,
    },

    /// Terminal event for a compaction. Followed by no `Done` — compaction
    /// is not a regular turn. The TUI uses this to swap its in-memory
    /// message buffer + persist the new history.
    ///
    /// The `summary` is the markdown text the model returned; the final
    /// on-disk history is `[system_prompt, system(summary), ...kept]`.
    CompactionComplete {
        /// The freshly-generated summary text (markdown, sectioned).
        summary: String,
        /// How many messages were in the history before compaction.
        original_count: usize,
        /// How many trailing user-turn entries survived verbatim.
        /// The TUI uses this to trim its own `ctx.messages` list
        /// to match the agent's post-compaction shape.
        keep_last: usize,
        /// Total `ConversationMessage` entries in the agent's history
        /// after compaction (system + summary envelope + recent).
        kept_count: usize,
    },
}

#[derive(Debug, Clone)]
pub struct TurnResult {
    pub text: String,
    pub usage: TokenUsage,
    pub cancelled: bool,
}

pub(crate) const TOOL_OUTPUT_PREVIEW_MAX: usize = 500;

pub(crate) fn truncate_preview(s: &str) -> String {
    if s.len() <= TOOL_OUTPUT_PREVIEW_MAX {
        s.to_string()
    } else {
        let mut out = String::with_capacity(TOOL_OUTPUT_PREVIEW_MAX + '…'.len_utf8());
        // char-boundary-safe truncation
        let mut end = TOOL_OUTPUT_PREVIEW_MAX;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        out.push_str(&s[..end]);
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_preview_under_limit_is_unchanged() {
        let s = "hello world";
        assert_eq!(truncate_preview(s), s);
    }

    #[test]
    fn tool_output_preview_over_limit_is_truncated_with_ellipsis() {
        let s = "x".repeat(TOOL_OUTPUT_PREVIEW_MAX + 100);
        let out = truncate_preview(&s);
        assert!(out.ends_with('…'));
        assert!(out.len() <= TOOL_OUTPUT_PREVIEW_MAX + 4); // 3 bytes for ellipsis
    }

    #[test]
    fn tool_output_preview_respects_char_boundaries() {
        // Place a 4-byte emoji (🦀) so that byte 500 falls inside it —
        // forces truncate_preview to walk `end` back from 500 to the
        // nearest valid char boundary.
        let mut s = String::new();
        for _ in 0..497 {
            s.push('a');
        }
        s.push('🦀'); // 4 bytes: spans byte indices 497..501, so byte 500 is mid-codepoint
        for _ in 0..100 {
            s.push('b');
        } // ensure s.len() > TOOL_OUTPUT_PREVIEW_MAX
        assert!(s.len() > TOOL_OUTPUT_PREVIEW_MAX);

        let out = truncate_preview(&s);

        // The output must end with the ellipsis AND the byte just before '…' must be
        // a valid char boundary (the walk-back worked).
        assert!(out.ends_with('…'));
        let pre_ellipsis_end = out.len() - '…'.len_utf8();
        assert!(out.is_char_boundary(pre_ellipsis_end));

        // More importantly: the 🦀 must NOT appear truncated. Either it's fully
        // present (walk-back kept boundary before byte 497) or fully absent
        // (walk-back stopped exactly at byte 497). Never partial.
        // The walk from 500 → 499 → 498 → 497 lands at 497, which is the start
        // of 🦀. So the emoji is EXCLUDED from the truncated output.
        assert!(
            !out.contains('🦀'),
            "walk-back should have stopped at byte 497, excluding the mid-cut emoji"
        );
        // And the body before the emoji should be fully present.
        let body_before_emoji: String = "a".repeat(497);
        assert!(out.starts_with(&body_before_emoji));
    }

    #[test]
    fn agent_event_variants_construct() {
        let _ = AgentEvent::Chunk("hi".into());
        let _ = AgentEvent::ToolCallStart {
            id: "1".into(),
            name: "shell".into(),
            args: serde_json::json!({"cmd": "ls"}),
        };
        let _ = AgentEvent::ToolCallEnd {
            id: "1".into(),
            ok: true,
            output_preview: "ok".into(),
        };
        let _ = AgentEvent::Done {
            final_text: "done".into(),
            cancelled: false,
        };
        let _ = AgentEvent::Error("boom".into());
    }
}
