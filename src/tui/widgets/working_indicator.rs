//! Wall-clock-driven "agent is working" indicator.
//!
//! The point of this widget is to **keep moving even when the provider
//! is silent**. The frame index is computed from elapsed time, not
//! from token arrival, so a 30-second provider stall still animates
//! and the user can see that the runtime is alive. This is the Hermes
//! lesson: token-driven spinners freeze during the exact moments the
//! user most needs reassurance.
//!
//! Output is a single [`Line`] suitable for a 1-row status bar:
//!
//! ```text
//! ⠹ thinking… ⏱ 12s · esc to interrupt
//! ⠼ running shell(brew --version)… ⏱ 3s · esc to interrupt
//! ⠿ cancelling…
//! ```
//!
//! The elapsed timer resets per tool call when one is in-flight, so a
//! multi-step run shows per-step time. Operators care about *which*
//! step is slow, not the cumulative turn duration.

use std::time::{Duration, Instant};

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Time each spinner frame is visible. 80 ms matches what every other
/// modern agent CLI uses (Hermes, OpenClaw, sst/opencode).
const FRAME_INTERVAL: Duration = Duration::from_millis(80);

/// What the agent is currently doing. The caller (TUI render loop)
/// decides which variant to build from `AppState`.
#[derive(Debug)]
pub enum WorkingState<'a> {
    /// Provider is streaming text; no tool in flight.
    Thinking { turn_started: Instant },
    /// A tool call is executing. Timer resets per call so the user can
    /// see which step is slow.
    Tool {
        name: &'a str,
        tool_started: Instant,
    },
    /// User pressed Esc; we're waiting for the in-flight cancel to take
    /// effect. No timer — the relevant clock is "we asked, did it stop
    /// yet" and that should be milliseconds.
    Cancelling,
}

/// Render the indicator as a single `Line`.
///
/// `now` is taken as a parameter so tests don't have to call
/// `Instant::now()` and so the same instant is shared with whatever
/// other widget rendered this frame.
pub fn render(state: &WorkingState<'_>, now: Instant) -> Line<'static> {
    let muted = Style::default().fg(Color::Rgb(107, 114, 128));
    let sky = Style::default().fg(Color::Rgb(94, 184, 255));
    let coral = Style::default()
        .fg(Color::Rgb(255, 123, 123))
        .add_modifier(Modifier::BOLD);

    match state {
        WorkingState::Cancelling => {
            // Use a faster-cycling frame source for cancelling so the
            // visual feedback is distinct from normal streaming.
            let frame = frame_index(now, Instant::now() - Duration::from_secs(0));
            Line::from(vec![
                Span::raw(" "),
                Span::styled(SPINNER_FRAMES[frame].to_string(), coral),
                Span::raw(" "),
                Span::styled("cancelling…", coral),
            ])
        }
        WorkingState::Thinking { turn_started } => {
            let frame = frame_index(now, *turn_started);
            let elapsed = now.saturating_duration_since(*turn_started);
            Line::from(vec![
                Span::raw(" "),
                Span::styled(SPINNER_FRAMES[frame].to_string(), sky),
                Span::raw(" "),
                Span::styled("thinking… ", sky.add_modifier(Modifier::BOLD)),
                Span::styled("⏱ ", muted),
                Span::styled(format_elapsed(elapsed), muted),
                Span::styled("  ·  ", muted),
                Span::styled("esc to interrupt", muted),
            ])
        }
        WorkingState::Tool { name, tool_started } => {
            let frame = frame_index(now, *tool_started);
            let elapsed = now.saturating_duration_since(*tool_started);
            Line::from(vec![
                Span::raw(" "),
                Span::styled(SPINNER_FRAMES[frame].to_string(), sky),
                Span::raw(" "),
                Span::styled("running ", sky),
                Span::styled((*name).to_string(), sky.add_modifier(Modifier::BOLD)),
                Span::styled("… ", sky),
                Span::styled("⏱ ", muted),
                Span::styled(format_elapsed(elapsed), muted),
                Span::styled("  ·  ", muted),
                Span::styled("esc to interrupt", muted),
            ])
        }
    }
}

/// Wall-clock frame index. Uses elapsed-since-start so the animation
/// is smooth across redraws — the render loop just calls us with the
/// current `Instant` and gets the right frame without needing to
/// store any per-spinner state.
fn frame_index(now: Instant, started: Instant) -> usize {
    let elapsed = now.saturating_duration_since(started);
    let ticks = elapsed.as_millis() / FRAME_INTERVAL.as_millis();
    (ticks as usize) % SPINNER_FRAMES.len()
}

/// Compact elapsed format matching OpenClaw's `formatElapsed`:
/// `42s`, `1m 5s`, `1h 2m`. Sub-second resolution is rounded down.
pub fn format_elapsed(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    if total < 60 {
        format!("{total}s")
    } else if total < 3600 {
        format!("{}m {}s", total / 60, total % 60)
    } else {
        format!("{}h {}m", total / 3600, (total % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_index_advances_with_time() {
        let start = Instant::now();
        let a = frame_index(start, start);
        let b = frame_index(start + Duration::from_millis(80), start);
        let c = frame_index(start + Duration::from_millis(160), start);
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 2);
    }

    #[test]
    fn frame_index_wraps_at_ten_frames() {
        let start = Instant::now();
        let frame_at_800ms = frame_index(start + Duration::from_millis(80 * 10), start);
        assert_eq!(frame_at_800ms, 0, "10 frames × 80ms wraps back to 0");
    }

    #[test]
    fn format_elapsed_under_a_minute() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(42)), "42s");
        assert_eq!(format_elapsed(Duration::from_millis(999)), "0s");
    }

    #[test]
    fn format_elapsed_minutes_and_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m 5s");
        assert_eq!(format_elapsed(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn format_elapsed_hours_and_minutes() {
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1h 0m");
        assert_eq!(format_elapsed(Duration::from_secs(3725)), "1h 2m");
    }

    #[test]
    fn render_thinking_contains_verb_and_timer() {
        let start = Instant::now();
        let line = render(
            &WorkingState::Thinking {
                turn_started: start,
            },
            start + Duration::from_secs(12),
        );
        let joined: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<&str>>()
            .join("");
        assert!(joined.contains("thinking…"), "verb present: {joined:?}");
        assert!(joined.contains("12s"), "elapsed present: {joined:?}");
        assert!(joined.contains("esc"), "interrupt hint: {joined:?}");
    }

    #[test]
    fn render_tool_uses_tool_name_not_turn_age() {
        let start = Instant::now();
        let line = render(
            &WorkingState::Tool {
                name: "shell",
                tool_started: start,
            },
            start + Duration::from_secs(3),
        );
        let joined: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<&str>>()
            .join("");
        assert!(joined.contains("running"), "verb: {joined:?}");
        assert!(joined.contains("shell"), "tool name: {joined:?}");
        assert!(
            joined.contains("3s"),
            "per-tool elapsed (not turn elapsed): {joined:?}"
        );
    }

    #[test]
    fn render_cancelling_says_cancelling() {
        let line = render(&WorkingState::Cancelling, Instant::now());
        let joined: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<&str>>()
            .join("");
        assert!(joined.contains("cancelling…"));
    }

    #[test]
    fn render_thinking_spinner_frame_changes_with_time() {
        // Two renders 240ms apart should produce different spinner glyphs.
        let start = Instant::now();
        let early = render(
            &WorkingState::Thinking {
                turn_started: start,
            },
            start + Duration::from_millis(0),
        );
        let later = render(
            &WorkingState::Thinking {
                turn_started: start,
            },
            start + Duration::from_millis(240),
        );
        let glyph_at = |line: &Line<'_>| line.spans.get(1).map(|s| s.content.to_string());
        assert_ne!(
            glyph_at(&early),
            glyph_at(&later),
            "wall-clock progress must advance the frame"
        );
    }
}
