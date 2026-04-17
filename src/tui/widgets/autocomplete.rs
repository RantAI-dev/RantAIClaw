use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
    Frame,
};

pub struct Autocomplete {
    suggestions: Vec<String>,
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

    pub fn update(&mut self, suggestions: Vec<String>) {
        self.suggestions = suggestions;
        self.visible = !self.suggestions.is_empty();
        if self.visible {
            self.state.select(Some(0));
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

    pub fn selected(&self) -> Option<&String> {
        self.state.selected().and_then(|i| self.suggestions.get(i))
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

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.visible || self.suggestions.is_empty() {
            return;
        }

        let height: u16 = (self.suggestions.len() + 2)
            .min(8)
            .try_into()
            .unwrap_or(8u16);
        let popup_area = Rect {
            x: area.x,
            y: area.y.saturating_sub(height),
            width: area.width.min(40),
            height,
        };

        let items: Vec<ListItem> = self
            .suggestions
            .iter()
            .map(|s| ListItem::new(Line::from(s.as_str())))
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Suggestions"))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(Clear, popup_area);
        frame.render_stateful_widget(list, popup_area, &mut self.state);
    }
}

impl Default for Autocomplete {
    fn default() -> Self {
        Self::new()
    }
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
        ac.update(vec!["/help".to_string(), "/quit".to_string()]);
        assert!(ac.is_visible());
        assert_eq!(ac.selected(), Some(&"/help".to_string()));
    }

    #[test]
    fn autocomplete_navigates_next_and_previous() {
        let mut ac = Autocomplete::new();
        ac.update(vec!["/a".to_string(), "/b".to_string(), "/c".to_string()]);
        ac.next();
        assert_eq!(ac.selected(), Some(&"/b".to_string()));
        ac.next();
        assert_eq!(ac.selected(), Some(&"/c".to_string()));
        ac.next();
        assert_eq!(ac.selected(), Some(&"/a".to_string()));
        ac.previous();
        assert_eq!(ac.selected(), Some(&"/c".to_string()));
    }

    #[test]
    fn autocomplete_hides_when_empty() {
        let mut ac = Autocomplete::new();
        ac.update(vec!["/help".to_string()]);
        assert!(ac.is_visible());
        ac.update(vec![]);
        assert!(!ac.is_visible());
    }
}
