//! Slash-command autocomplete dropdown — Hermes / Claude-Code style.
//!
//! Pops up the moment the user types `/` in the input buffer and filters
//! by prefix on every keystroke. Each row shows the command name in the
//! brand sky-blue and the description in muted gray, two-column laid out
//! so descriptions don't crowd the names.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState},
    Frame,
};

const NAME_COLOR: Color = Color::Rgb(94, 184, 255);
const DESC_COLOR: Color = Color::Rgb(107, 114, 128);
const BORDER_COLOR: Color = Color::Rgb(40, 70, 140);
const HIGHLIGHT_BG: Color = Color::Rgb(20, 30, 70);
const NAME_COL_W: usize = 28;

pub struct Autocomplete {
    suggestions: Vec<(String, String)>,
    state: ListState,
    visible: bool,
}

impl Autocomplete {
    pub fn new() -> Self {
        Self {
            suggestions: Vec::new(),
            state: ListState::default(),
            visible: false,
        }
    }

    /// Replace the suggestion list. Empty → hidden.
    pub fn update(&mut self, suggestions: Vec<(String, String)>) {
        self.suggestions = suggestions;
        self.visible = !self.suggestions.is_empty();
        if self.visible {
            // Preserve the current selection when possible (so typing more
            // characters keeps the highlight stable as long as the prefix
            // still matches); otherwise reset to the first row.
            let prev_selected = self
                .state
                .selected()
                .and_then(|i| self.suggestions.get(i).cloned());
            let new_idx = prev_selected
                .and_then(|(name, _)| {
                    self.suggestions.iter().position(|(n, _)| n == &name)
                })
                .unwrap_or(0);
            self.state.select(Some(new_idx));
        } else {
            self.state.select(None);
        }
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.suggestions.clear();
        self.state.select(None);
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// The currently highlighted command name, if visible.
    pub fn selected(&self) -> Option<&str> {
        self.state
            .selected()
            .and_then(|i| self.suggestions.get(i))
            .map(|(name, _)| name.as_str())
    }

    pub fn next(&mut self) {
        if self.suggestions.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => (i + 1) % self.suggestions.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn previous(&mut self) {
        if self.suggestions.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.suggestions.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    /// Render the dropdown. Caller passes the area immediately *below* the
    /// input box; the widget consumes whatever vertical space it needs and
    /// ignores the rest.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.visible || self.suggestions.is_empty() || area.height < 3 {
            return;
        }

        // Cap visible rows at the available height (minus borders) so
        // the dropdown can scale up in alt-screen mode without an
        // arbitrary 8-item ceiling. Inline mode passes a tight area
        // and gets its small dropdown naturally.
        let max_visible = (area.height as usize).saturating_sub(2).max(1);
        let visible_count = self.suggestions.len().min(max_visible);
        let height: u16 = (visible_count + 2).try_into().unwrap_or(area.height);

        let popup = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: height.min(area.height),
        };

        let inner_w = popup.width.saturating_sub(2) as usize;
        let desc_col_w = inner_w.saturating_sub(NAME_COL_W + 2);

        let items: Vec<ListItem> = self
            .suggestions
            .iter()
            .map(|(name, desc)| {
                let name_pad = if name.chars().count() < NAME_COL_W {
                    NAME_COL_W - name.chars().count()
                } else {
                    1
                };
                let truncated_desc = truncate(desc, desc_col_w);
                let line = Line::from(vec![
                    Span::styled(
                        name.clone(),
                        Style::default()
                            .fg(NAME_COLOR)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" ".repeat(name_pad)),
                    Span::styled(truncated_desc, Style::default().fg(DESC_COLOR)),
                ]);
                ListItem::new(line)
            })
            .collect();

        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "/commands",
                Style::default().fg(NAME_COLOR).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {} matches", self.suggestions.len()),
                Style::default().fg(DESC_COLOR),
            ),
            Span::raw(" "),
        ]);

        let list = List::new(items)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(BORDER_COLOR)),
            )
            .highlight_style(
                Style::default()
                    .bg(HIGHLIGHT_BG)
                    .fg(NAME_COLOR)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(Clear, popup);
        frame.render_stateful_widget(list, popup, &mut self.state);
    }
}

impl Default for Autocomplete {
    fn default() -> Self {
        Self::new()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let cap = max.saturating_sub(1);
    let truncated: String = s.chars().take(cap).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autocomplete_starts_hidden() {
        let ac = Autocomplete::new();
        assert!(!ac.is_visible());
        assert!(ac.selected().is_none());
    }

    #[test]
    fn autocomplete_shows_with_suggestions() {
        let mut ac = Autocomplete::new();
        ac.update(vec![
            ("/help".into(), "Show this help".into()),
            ("/quit".into(), "Exit".into()),
        ]);
        assert!(ac.is_visible());
        assert_eq!(ac.selected(), Some("/help"));
    }

    #[test]
    fn autocomplete_navigates_next_and_previous() {
        let mut ac = Autocomplete::new();
        ac.update(vec![
            ("/a".into(), "a".into()),
            ("/b".into(), "b".into()),
            ("/c".into(), "c".into()),
        ]);
        ac.next();
        assert_eq!(ac.selected(), Some("/b"));
        ac.next();
        assert_eq!(ac.selected(), Some("/c"));
        ac.next();
        assert_eq!(ac.selected(), Some("/a"));
        ac.previous();
        assert_eq!(ac.selected(), Some("/c"));
    }

    #[test]
    fn autocomplete_hides_when_empty() {
        let mut ac = Autocomplete::new();
        ac.update(vec![("/help".into(), "h".into())]);
        assert!(ac.is_visible());
        ac.update(vec![]);
        assert!(!ac.is_visible());
    }

    #[test]
    fn autocomplete_preserves_selection_through_filter() {
        let mut ac = Autocomplete::new();
        ac.update(vec![
            ("/help".into(), "h".into()),
            ("/quit".into(), "q".into()),
            ("/retry".into(), "r".into()),
        ]);
        ac.next();
        ac.next();
        assert_eq!(ac.selected(), Some("/retry"));
        // User keeps typing: filter narrows but `/retry` is still in the list.
        ac.update(vec![("/retry".into(), "r".into())]);
        assert_eq!(ac.selected(), Some("/retry"));
    }

    #[test]
    fn truncate_appends_ellipsis_when_too_long() {
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a", 0), "");
    }
}
