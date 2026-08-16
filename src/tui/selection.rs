//! Chat-pane text selection: the pure logic behind drag-to-highlight and
//! Ctrl+C-to-copy.
//!
//! The TUI captures the mouse (wheel scrolling needs it — see
//! `enter_fullscreen`), which means the terminal's native drag-selection never
//! fires. This module implements selection *inside* the app instead, the way
//! Hermes and zeroclaw do: drag highlights whole display lines, Ctrl+C with an
//! active selection copies via OSC 52.
//!
//! Everything here is pure and unit-tested. The anchored unit is a **rendered
//! line** — an index into `render_message_lines`' output for one message —
//! NOT a `Message::content` line. The renderer prepends a role-label line and
//! appends tool-block lines, so content-line indices never align with what is
//! on screen; anchoring and extraction must both index the same rendered
//! output or a copy silently grabs the wrong lines.

use ratatui::layout::Rect;
use ratatui::text::Line;

/// `(message index in the transcript, rendered-line index within that
/// message's `render_message_lines` output)`.
///
/// Display-line indices shift every frame while a reply streams; message
/// indices and rendered-line offsets don't, so a selection anchored this way
/// survives streaming appends. It does NOT survive history *replacement*
/// (`/compress`, `/clear`) — callers clear the selection at those sites, and
/// [`extract_text`] clamps defensively in case one is missed.
pub type LineAnchor = (usize, usize);

/// An active selection: where the drag started and where it currently ends.
/// `anchor`/`head` may be in either order on screen — use [`Selection::range`]
/// for the normalized form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: LineAnchor,
    pub head: LineAnchor,
}

impl Selection {
    /// Normalized `(first, last)` endpoints, both inclusive, ordered by
    /// `(msg_idx, rendered_line_idx)`.
    pub fn range(&self) -> (LineAnchor, LineAnchor) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// True when `a` falls inside the normalized range (inclusive).
    pub fn contains(&self, a: LineAnchor) -> bool {
        let (lo, hi) = self.range();
        lo <= a && a <= hi
    }
}

/// True when screen cell `(col, row)` lies inside the chat pane's inner text
/// area — the pane rect shrunk by its 1-cell `Borders::ALL` frame.
pub fn point_in_chat_inner(col: u16, row: u16, area: Rect) -> bool {
    if area.width < 3 || area.height < 3 {
        return false;
    }
    let x0 = area.x + 1;
    let x1 = area.x + area.width - 2;
    let y0 = area.y + 1;
    let y1 = area.y + area.height - 2;
    (x0..=x1).contains(&col) && (y0..=y1).contains(&row)
}

/// Map a screen `row` inside the chat pane to a global display-line index.
///
/// `start` is the global index of the first visible display line — the same
/// `start` the renderer computed for its window (`lines[start..end]`); the
/// caller stashes it at render time so the hit-test and the drawn frame can
/// never disagree.
pub fn global_line_at(row: u16, area: Rect, start: usize) -> Option<usize> {
    if area.height < 3 {
        return None;
    }
    let y0 = area.y + 1;
    let y1 = area.y + area.height - 2;
    if !(y0..=y1).contains(&row) {
        return None;
    }
    Some(start + (row - y0) as usize)
}

/// Resolve the anchor for a global display line, scanning to the nearest
/// anchored neighbour when the hit line itself carries none (turn separators,
/// the streaming spinner). Prefers the earlier neighbour, then the later one —
/// a drag that crosses a separator extends to the adjacent message instead of
/// dying on the blank line.
pub fn anchor_at(provenance: &[Option<LineAnchor>], global_idx: usize) -> Option<LineAnchor> {
    if let Some(Some(a)) = provenance.get(global_idx) {
        return Some(*a);
    }
    // Nearest anchored line before the hit…
    if let Some(a) = provenance
        .get(..global_idx.min(provenance.len()))
        .into_iter()
        .flatten()
        .rev()
        .find_map(|p| *p)
    {
        return Some(a);
    }
    // …else the nearest one after it.
    provenance
        .get(global_idx..)
        .into_iter()
        .flatten()
        .find_map(|p| *p)
}

/// Concatenate one rendered line's span contents into plain text.
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Extract the selected text for a normalized `(first, last)` range.
///
/// `rendered_for(msg_idx)` must produce the SAME lines the pane renders for
/// that message (the shared rendered-lines function) — single source, the two
/// cannot diverge. Out-of-range message or line indices are clamped: a
/// dangling selection (history replaced under it) yields whatever still
/// exists, or an empty string — never a panic.
///
/// Output: rendered lines joined with `\n`, messages joined with `\n\n`.
/// WYSIWYG minus borders and wrap — multi-line code fences pass through
/// verbatim (the markdown parser is inline-only), inline styling is already
/// resolved (`**bold**` copies as `bold`), and a range that includes a
/// message's first rendered line carries its role label.
pub fn extract_text(
    messages_len: usize,
    range: (LineAnchor, LineAnchor),
    mut rendered_for: impl FnMut(usize) -> Vec<Line<'static>>,
) -> String {
    let ((msg_a, line_a), (msg_b, line_b)) = range;
    if messages_len == 0 || msg_a >= messages_len {
        return String::new();
    }
    let msg_b = msg_b.min(messages_len - 1);
    if msg_a > msg_b {
        return String::new();
    }

    let mut parts: Vec<String> = Vec::new();
    for msg_idx in msg_a..=msg_b {
        let lines = rendered_for(msg_idx);
        if lines.is_empty() {
            continue;
        }
        let lo = if msg_idx == msg_a { line_a } else { 0 };
        let hi = if msg_idx == msg_b {
            line_b.min(lines.len().saturating_sub(1))
        } else {
            lines.len() - 1
        };
        if lo > hi || lo >= lines.len() {
            continue;
        }
        let text: Vec<String> = lines[lo..=hi].iter().map(line_text).collect();
        parts.push(text.join("\n"));
    }
    parts.join("\n\n")
}

/// Encoded-payload cap: xterm's classic OSC 52 limit. Larger selections get
/// no escape at all (the terminal would truncate or drop it silently) — the
/// caller tells the user instead.
pub const OSC52_MAX_ENCODED: usize = 99 * 1024;

/// Build the OSC 52 clipboard-write escape for `text`:
/// `ESC ] 52 ; c ; <base64> BEL`. `None` when the encoded payload exceeds
/// [`OSC52_MAX_ENCODED`].
pub fn osc52_sequence(text: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;

    let encoded = B64.encode(text.as_bytes());
    if encoded.len() > OSC52_MAX_ENCODED {
        return None;
    }
    let mut seq = Vec::with_capacity(encoded.len() + 8);
    seq.extend_from_slice(b"\x1b]52;c;");
    seq.extend_from_slice(encoded.as_bytes());
    seq.push(0x07);
    Some(seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn lines(texts: &[&str]) -> Vec<Line<'static>> {
        texts
            .iter()
            .map(|t| Line::from(Span::raw((*t).to_string())))
            .collect()
    }

    // ── Selection normalization ────────────────────────────────

    #[test]
    fn range_orders_endpoints_both_directions() {
        let fwd = Selection {
            anchor: (0, 1),
            head: (2, 0),
        };
        let bwd = Selection {
            anchor: (2, 0),
            head: (0, 1),
        };
        assert_eq!(fwd.range(), ((0, 1), (2, 0)));
        assert_eq!(bwd.range(), ((0, 1), (2, 0)));
    }

    #[test]
    fn contains_is_inclusive_and_lexicographic() {
        let s = Selection {
            anchor: (1, 2),
            head: (3, 0),
        };
        assert!(s.contains((1, 2)));
        assert!(s.contains((2, 999)));
        assert!(s.contains((3, 0)));
        assert!(!s.contains((1, 1)));
        assert!(!s.contains((3, 1)));
    }

    // ── Hit-testing ────────────────────────────────────────────

    #[test]
    fn point_in_chat_inner_excludes_borders() {
        let a = rect(0, 0, 10, 6);
        assert!(point_in_chat_inner(1, 1, a));
        assert!(point_in_chat_inner(8, 4, a));
        assert!(!point_in_chat_inner(0, 1, a)); // left border
        assert!(!point_in_chat_inner(9, 1, a)); // right border
        assert!(!point_in_chat_inner(1, 0, a)); // top border
        assert!(!point_in_chat_inner(1, 5, a)); // bottom border
    }

    #[test]
    fn global_line_at_maps_rows_through_window_start() {
        let a = rect(0, 2, 20, 8); // inner rows: 3..=8
        assert_eq!(global_line_at(3, a, 40), Some(40));
        assert_eq!(global_line_at(8, a, 40), Some(45));
        assert_eq!(global_line_at(2, a, 40), None); // top border
        assert_eq!(global_line_at(9, a, 40), None); // bottom border
    }

    #[test]
    fn degenerate_area_hits_nothing() {
        let a = rect(0, 0, 2, 2);
        assert!(!point_in_chat_inner(0, 0, a));
        assert_eq!(global_line_at(0, a, 0), None);
    }

    // ── Anchor resolution over provenance ──────────────────────

    #[test]
    fn anchor_at_returns_direct_hit() {
        let prov = vec![Some((0, 0)), Some((0, 1)), None, Some((1, 0))];
        assert_eq!(anchor_at(&prov, 1), Some((0, 1)));
    }

    #[test]
    fn anchor_at_separator_prefers_earlier_neighbour() {
        let prov = vec![Some((0, 0)), Some((0, 1)), None, Some((1, 0))];
        assert_eq!(anchor_at(&prov, 2), Some((0, 1)));
    }

    #[test]
    fn anchor_at_leading_gap_falls_forward() {
        let prov = vec![None, None, Some((0, 0))];
        assert_eq!(anchor_at(&prov, 0), Some((0, 0)));
    }

    #[test]
    fn anchor_at_all_none_is_none() {
        let prov: Vec<Option<LineAnchor>> = vec![None, None];
        assert_eq!(anchor_at(&prov, 1), None);
        assert_eq!(anchor_at(&prov, 99), None); // past the end
    }

    // ── Extraction ─────────────────────────────────────────────

    fn two_messages(idx: usize) -> Vec<Line<'static>> {
        match idx {
            0 => lines(&["You: first", "second line"]),
            1 => lines(&["Assistant: reply", "```rust", "fn main() {}", "```"]),
            _ => Vec::new(),
        }
    }

    #[test]
    fn extract_within_one_message() {
        let got = extract_text(2, ((0, 0), (0, 1)), two_messages);
        assert_eq!(got, "You: first\nsecond line");
    }

    #[test]
    fn extract_across_messages_keeps_fence_verbatim() {
        let got = extract_text(2, ((0, 1), (1, 3)), two_messages);
        assert_eq!(
            got,
            "second line\n\nAssistant: reply\n```rust\nfn main() {}\n```"
        );
    }

    #[test]
    fn extract_mid_message_endpoints() {
        let got = extract_text(2, ((1, 1), (1, 2)), two_messages);
        assert_eq!(got, "```rust\nfn main() {}");
    }

    #[test]
    fn extract_dangling_message_index_is_empty_not_panic() {
        // History was replaced under the selection (/compress) — msg 5 gone.
        let got = extract_text(2, ((5, 0), (6, 3)), two_messages);
        assert_eq!(got, "");
    }

    #[test]
    fn extract_dangling_line_index_clamps() {
        let got = extract_text(2, ((1, 2), (1, 999)), two_messages);
        assert_eq!(got, "fn main() {}\n```");
    }

    #[test]
    fn extract_start_line_past_message_end_skips() {
        let got = extract_text(2, ((0, 99), (1, 0)), two_messages);
        assert_eq!(got, "Assistant: reply");
    }

    #[test]
    fn extract_empty_history_is_empty() {
        let got = extract_text(0, ((0, 0), (0, 0)), |_| Vec::new());
        assert_eq!(got, "");
    }

    // ── OSC 52 ─────────────────────────────────────────────────

    #[test]
    fn osc52_known_vector() {
        // base64("hi") == "aGk="
        let seq = osc52_sequence("hi").unwrap();
        assert_eq!(seq, b"\x1b]52;c;aGk=\x07".to_vec());
    }

    #[test]
    fn osc52_size_cap_boundary() {
        // 4 base64 chars per 3 input bytes: this input encodes just under the cap…
        let under = "x".repeat(OSC52_MAX_ENCODED / 4 * 3 - 3);
        assert!(osc52_sequence(&under).is_some());
        // …and this one lands just over it.
        let over = "x".repeat(OSC52_MAX_ENCODED / 4 * 3 + 3);
        assert!(osc52_sequence(&over).is_none());
    }
}
