//! Rendering helpers for TUI chat messages.
//!
//! Pure functions that convert message content (assistant/user text plus
//! optional tool-call records) into ratatui `Line`s. Kept independent of
//! `TuiApp` state so they're easy to unit test.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde::{Deserialize, Serialize};

use super::app::ToolBlockState;

/// Maximum characters to show for tool args inline. Anything longer is
/// truncated with an ellipsis. Keeps the chat compact when a tool call has
/// a large payload (e.g. file contents).
pub const TOOL_ARG_PREVIEW_MAX: usize = 80;

/// Maximum characters to show for tool result preview inline.
pub const TOOL_RESULT_PREVIEW_MAX: usize = 80;

/// Persisted tool-call snapshot stored alongside assistant messages so the
/// chat history can re-render tool blocks after the streaming session ends.
///
/// Mirrors [`ToolBlockState`] but uses owned, serde-friendly types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
    /// `None` while still running; `Some((ok, preview))` once the result lands.
    pub result: Option<(bool, String)>,
}

impl From<&ToolBlockState> for PersistedToolCall {
    fn from(b: &ToolBlockState) -> Self {
        Self {
            id: b.id.clone(),
            name: b.name.clone(),
            args: b.args.clone(),
            result: b.result.clone(),
        }
    }
}

/// Serialize a list of tool blocks into JSON for storage in
/// `Message.tool_calls`. Returns `None` if the list is empty so we don't
/// pay for an empty `[]` blob in the database.
pub fn serialize_tool_calls(blocks: &[ToolBlockState]) -> Option<String> {
    if blocks.is_empty() {
        return None;
    }
    let persisted: Vec<PersistedToolCall> = blocks.iter().map(PersistedToolCall::from).collect();
    serde_json::to_string(&persisted).ok()
}

/// Parse persisted tool calls from a `Message.tool_calls` JSON string.
/// Returns an empty `Vec` on parse failure so a corrupt row never crashes
/// the render loop.
pub fn parse_persisted_tool_calls(json: &str) -> Vec<PersistedToolCall> {
    serde_json::from_str(json).unwrap_or_default()
}

/// Color/style palette used by the renderer. Keeping this as a single
/// struct lets tests pin specific colors and lets future work swap out
/// themes without touching the parsing logic.
#[derive(Debug, Clone, Copy)]
pub struct RenderTheme {
    pub user_label: Color,
    pub assistant_label: Color,
    pub system_label: Color,
    pub tool_name: Color,
    pub tool_args: Color,
    pub tool_result_ok: Color,
    pub tool_result_err: Color,
    pub code: Color,
}

impl Default for RenderTheme {
    fn default() -> Self {
        // rantai-agents brand palette (matches src/onboard/branding.rs):
        //   sky #5eb8ff (94,184,255) — accent / labels
        //   blue #3b8cff (59,140,255) — assistant / titles
        //   amber #ffbb5c (255,187,92) — system messages
        //   mint #7ee2b3 (126,226,179) — tool ok
        //   coral #ff7b7b (255,123,123) — tool error
        //   muted #6b7280 (107,114,128) — secondary text
        Self {
            user_label: Color::Rgb(94, 184, 255),
            assistant_label: Color::Rgb(59, 140, 255),
            system_label: Color::Rgb(255, 187, 92),
            tool_name: Color::Rgb(94, 184, 255),
            tool_args: Color::Rgb(107, 114, 128),
            tool_result_ok: Color::Rgb(126, 226, 179),
            tool_result_err: Color::Rgb(255, 123, 123),
            code: Color::Rgb(94, 184, 255),
        }
    }
}

/// Render a single content line with block-level markdown awareness:
/// strips ATX-style heading prefixes (`#`, `##`, `###`) and styles the
/// remainder as a bold heading; recognises `- ` / `* ` bullets and styles
/// the marker. Falls back to inline-markdown parsing for normal lines.
pub fn render_block_line(text: &str, theme: &RenderTheme) -> Line<'static> {
    let stripped = text.trim_start_matches(' ');
    let leading_ws_len = text.len() - stripped.len();
    let leading = &text[..leading_ws_len];

    // ATX headings: `# `, `## `, `### `, `#### `, `##### `, `###### `.
    if let Some(rest) = stripped
        .strip_prefix("###### ")
        .or_else(|| stripped.strip_prefix("##### "))
        .or_else(|| stripped.strip_prefix("#### "))
        .or_else(|| stripped.strip_prefix("### "))
        .or_else(|| stripped.strip_prefix("## "))
        .or_else(|| stripped.strip_prefix("# "))
    {
        let mut spans = Vec::new();
        if !leading.is_empty() {
            spans.push(Span::raw(leading.to_string()));
        }
        // Style the heading body bold + assistant-blue. Inline markdown
        // inside the heading still parses (so `**foo**` inside `###`
        // doesn't show literal stars).
        let inner = parse_inline_markdown(rest, theme);
        for span in inner {
            let style = span
                .style
                .add_modifier(Modifier::BOLD)
                .fg(theme.assistant_label);
            spans.push(Span::styled(span.content.into_owned(), style));
        }
        return Line::from(spans);
    }

    // Bullet markers: `- ` / `* ` / `+ ` (unordered). Style the marker
    // muted, parse the rest inline.
    if let Some(rest) = stripped
        .strip_prefix("- ")
        .or_else(|| stripped.strip_prefix("* "))
        .or_else(|| stripped.strip_prefix("+ "))
    {
        let mut spans = Vec::new();
        if !leading.is_empty() {
            spans.push(Span::raw(leading.to_string()));
        }
        spans.push(Span::styled(
            "• ".to_string(),
            Style::default().fg(theme.tool_args),
        ));
        spans.extend(parse_inline_markdown(rest, theme));
        return Line::from(spans);
    }

    Line::from(parse_inline_markdown(text, theme))
}

/// Render a complete message (label + content + tool blocks) into one or
/// more lines. Tool blocks render as indented sub-lines under the content.
pub fn render_message_lines(
    role: &str,
    content: &str,
    persisted: &[PersistedToolCall],
    streaming_blocks: &[ToolBlockState],
    theme: &RenderTheme,
) -> Vec<Line<'static>> {
    let (label, color) = match role {
        "user" => ("You", theme.user_label),
        "assistant" => ("Assistant", theme.assistant_label),
        _ => ("System", theme.system_label),
    };

    let mut lines = Vec::new();

    // First line: bold label + the first run of the content (with markdown).
    let mut first = vec![Span::styled(
        format!("{label}: "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];
    let mut content_iter = content.split('\n');
    if let Some(first_para) = content_iter.next() {
        let body = render_block_line(first_para, theme);
        first.extend(body.spans);
    }
    lines.push(Line::from(first));
    for rest in content_iter {
        lines.push(render_block_line(rest, theme));
    }

    // Tool blocks (persisted from history first, then any in-progress
    // streaming blocks). Indent so they read as belonging to the message.
    for tc in persisted {
        lines.push(render_tool_block_line(
            &tc.name,
            &tc.args,
            tc.result.as_ref(),
            theme,
        ));
    }
    for tc in streaming_blocks {
        lines.push(render_tool_block_line(
            &tc.name,
            &tc.args,
            tc.result.as_ref(),
            theme,
        ));
    }

    lines
}

/// Render a single tool block as one line:
/// `  └─ <name>(<args>) → <result>`.
fn render_tool_block_line(
    name: &str,
    args: &serde_json::Value,
    result: Option<&(bool, String)>,
    theme: &RenderTheme,
) -> Line<'static> {
    let arg_preview = preview_args(args);
    let mut spans = vec![
        Span::raw("  └─ "),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(theme.tool_name)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("({arg_preview})"),
            Style::default().fg(theme.tool_args),
        ),
    ];
    match result {
        None => {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                "…",
                Style::default()
                    .fg(theme.tool_args)
                    .add_modifier(Modifier::DIM),
            ));
        }
        Some((ok, preview)) => {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                if *ok { "✓ " } else { "✗ " }.to_string(),
                Style::default().fg(if *ok {
                    theme.tool_result_ok
                } else {
                    theme.tool_result_err
                }),
            ));
            spans.push(Span::styled(
                truncate_preview(preview, TOOL_RESULT_PREVIEW_MAX),
                Style::default().fg(theme.tool_args),
            ));
        }
    }
    Line::from(spans)
}

/// Compact JSON args for inline display. Falls back to a short string when
/// the JSON is large enough to dominate a chat row.
fn preview_args(args: &serde_json::Value) -> String {
    let compact = match args {
        serde_json::Value::Object(map) if map.is_empty() => return String::new(),
        serde_json::Value::Null => return String::new(),
        _ => args.to_string(),
    };
    truncate_preview(&compact, TOOL_ARG_PREVIEW_MAX)
}

fn truncate_preview(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Parse a single line of text into ratatui `Span`s, applying inline
/// markdown for **bold**, *italic*, and `inline code`. The parser is
/// intentionally minimal — multi-line constructs (code fences, lists,
/// blockquotes) are not handled here because the renderer treats every
/// newline-delimited segment independently.
pub fn parse_inline_markdown(text: &str, theme: &RenderTheme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let bytes = text.as_bytes();
    let mut buf = String::new();
    let mut i = 0;

    while i < bytes.len() {
        // Try strong: **...**
        if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            if let Some(end) = find_marker(text, i + 2, "**") {
                flush_buf(&mut spans, &mut buf, Style::default());
                let inner = &text[i + 2..end];
                spans.push(Span::styled(
                    inner.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                i = end + 2;
                continue;
            }
        }
        // Try emphasis: *...*  (single star, must not be **)
        if bytes[i] == b'*' && bytes.get(i + 1) != Some(&b'*') {
            if let Some(end) = find_marker(text, i + 1, "*") {
                // Reject if marker is immediately followed by '*' (bold),
                // already handled above; here we know it's a real emphasis.
                flush_buf(&mut spans, &mut buf, Style::default());
                let inner = &text[i + 1..end];
                spans.push(Span::styled(
                    inner.to_string(),
                    Style::default().add_modifier(Modifier::ITALIC),
                ));
                i = end + 1;
                continue;
            }
        }
        // Try inline code: `...`
        if bytes[i] == b'`' {
            if let Some(end) = find_marker(text, i + 1, "`") {
                flush_buf(&mut spans, &mut buf, Style::default());
                let inner = &text[i + 1..end];
                spans.push(Span::styled(
                    inner.to_string(),
                    Style::default().fg(theme.code),
                ));
                i = end + 1;
                continue;
            }
        }
        buf.push(text[i..].chars().next().unwrap_or(' '));
        i += text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
    }
    flush_buf(&mut spans, &mut buf, Style::default());
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

fn flush_buf(spans: &mut Vec<Span<'static>>, buf: &mut String, style: Style) {
    if !buf.is_empty() {
        spans.push(Span::styled(std::mem::take(buf), style));
    }
}

/// Find the next occurrence of `marker` in `text` starting at byte
/// offset `from`. Returns the byte position of the marker start, or
/// `None` if not found.
fn find_marker(text: &str, from: usize, marker: &str) -> Option<usize> {
    text[from..].find(marker).map(|p| from + p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> RenderTheme {
        RenderTheme::default()
    }

    #[test]
    fn parse_inline_plain_text_is_one_span() {
        let spans = parse_inline_markdown("hello world", &theme());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "hello world");
    }

    #[test]
    fn parse_inline_bold_is_three_spans_with_bold_modifier() {
        let spans = parse_inline_markdown("a **b** c", &theme());
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "a ");
        assert_eq!(spans[1].content, "b");
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[2].content, " c");
    }

    #[test]
    fn parse_inline_italic_uses_italic_modifier() {
        let spans = parse_inline_markdown("a *b* c", &theme());
        assert_eq!(spans.len(), 3);
        assert!(spans[1].style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(spans[1].content, "b");
    }

    #[test]
    fn parse_inline_code_styled_with_theme_code_color() {
        let spans = parse_inline_markdown("run `ls -la`", &theme());
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].content, "ls -la");
        assert_eq!(spans[1].style.fg, Some(theme().code));
    }

    #[test]
    fn parse_inline_unmatched_markers_treated_as_literal() {
        let spans = parse_inline_markdown("a *b without close", &theme());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "a *b without close");
    }

    #[test]
    fn parse_inline_bold_and_code_combined() {
        let spans = parse_inline_markdown("**big** then `code`", &theme());
        assert_eq!(spans.len(), 3);
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[2].content, "code");
        assert_eq!(spans[2].style.fg, Some(theme().code));
    }

    #[test]
    fn parse_inline_handles_utf8_in_buffer() {
        let spans = parse_inline_markdown("Hi 🦀 friend", &theme());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "Hi 🦀 friend");
    }

    #[test]
    fn render_message_lines_user_message_gets_green_label() {
        let lines = render_message_lines("user", "hello", &[], &[], &theme());
        assert_eq!(lines.len(), 1);
        let label_span = &lines[0].spans[0];
        assert_eq!(label_span.content, "You: ");
        assert_eq!(label_span.style.fg, Some(theme().user_label));
    }

    #[test]
    fn render_message_lines_multiline_content_produces_multiple_lines() {
        let lines = render_message_lines("assistant", "first\nsecond\nthird", &[], &[], &theme());
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn render_message_lines_appends_persisted_tool_blocks() {
        let persisted = vec![PersistedToolCall {
            id: "c1".into(),
            name: "shell".into(),
            args: serde_json::json!({"cmd": "ls"}),
            result: Some((true, "files".into())),
        }];
        let lines = render_message_lines("assistant", "I ran a tool.", &persisted, &[], &theme());
        assert_eq!(lines.len(), 2);
        let tool_line_text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(tool_line_text.contains("shell"));
        assert!(tool_line_text.contains("✓"));
        assert!(tool_line_text.contains("files"));
    }

    #[test]
    fn render_message_lines_includes_in_progress_streaming_block_with_ellipsis() {
        let block = ToolBlockState {
            id: "c1".into(),
            name: "shell".into(),
            args: serde_json::json!({"cmd": "ls"}),
            result: None,
            started_at: std::time::Instant::now(),
        };
        let lines = render_message_lines("assistant", "Running.", &[], &[block], &theme());
        assert_eq!(lines.len(), 2);
        let tool_line_text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(tool_line_text.contains("shell"));
        assert!(tool_line_text.contains('…'));
    }

    #[test]
    fn serialize_and_parse_tool_calls_roundtrip() {
        let blocks = vec![ToolBlockState {
            id: "c1".into(),
            name: "shell".into(),
            args: serde_json::json!({"cmd": "ls"}),
            result: Some((true, "ok".into())),
            started_at: std::time::Instant::now(),
        }];
        let json = serialize_tool_calls(&blocks).expect("non-empty");
        let parsed = parse_persisted_tool_calls(&json);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "shell");
        assert_eq!(parsed[0].result, Some((true, "ok".into())));
    }

    #[test]
    fn serialize_tool_calls_empty_returns_none() {
        let result = serialize_tool_calls(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn parse_persisted_tool_calls_corrupt_returns_empty() {
        let parsed = parse_persisted_tool_calls("not json");
        assert!(parsed.is_empty());
    }

    #[test]
    fn truncate_preview_chops_at_grapheme_boundary_with_ellipsis() {
        let s: String = "a".repeat(200);
        let out = truncate_preview(&s, 10);
        assert_eq!(out.chars().count(), 11); // 10 + ellipsis
        assert!(out.ends_with('…'));
    }

    #[test]
    fn preview_args_empty_object_returns_empty_string() {
        let preview = preview_args(&serde_json::json!({}));
        assert_eq!(preview, "");
    }
}
