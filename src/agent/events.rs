//! Structured events emitted during a single agent turn.
//!
//! Consumed by TUI / other callers that want richer feedback than the
//! plain `on_delta: mpsc::Sender<String>` stream used by channels.

use crate::cost::TokenUsage;

pub type AgentEventSender = tokio::sync::mpsc::Sender<AgentEvent>;

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
        let mut out = String::with_capacity(TOOL_OUTPUT_PREVIEW_MAX + 1);
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
        // Emoji takes 4 bytes in UTF-8 — ensure we don't slice mid-codepoint.
        let mut s = String::new();
        for _ in 0..TOOL_OUTPUT_PREVIEW_MAX { s.push('a'); }
        s.push('🦀'); // 4 bytes
        let out = truncate_preview(&s);
        assert!(out.is_char_boundary(out.len() - '…'.len_utf8()));
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
        let _ = AgentEvent::Done { final_text: "done".into(), cancelled: false };
        let _ = AgentEvent::Error("boom".into());
    }
}
