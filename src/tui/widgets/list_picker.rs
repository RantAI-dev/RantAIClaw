//! Generic interactive list picker — modal overlay navigated via
//! Up/Down/Enter/Esc, with a built-in search row at the top: any
//! printable character types into the query and filters the list live;
//! Backspace removes a character; Up/Down move within the filtered
//! view; Enter selects the highlighted match; Esc dismisses. Used by
//! `/model`, `/sessions`, `/resume`, `/personality`, and any future
//! picker. Each picker carries a `kind` tag so the app's key handler
//! can dispatch the right action when the user presses Enter.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

/// What kind of picker is open. The app's Enter handler matches on this
/// to know which side-effect to run with the selected key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListPickerKind {
    Model,
    Session,
    Personality,
    Skill,
    Help,
}

/// One row in the picker. `key` is opaque (provider:model, session id,
/// personality name…); `primary` is the highlighted label and
/// `secondary` is a muted description shown to its right.
#[derive(Debug, Clone)]
pub struct ListPickerItem {
    pub key: String,
    pub primary: String,
    pub secondary: String,
}

/// Page size for fullscreen-mode pagination. The picker shows up to
/// this many items per page; Left/Right step through pages.
pub const PAGE_SIZE: usize = 5;

/// Which UI element currently owns the cursor. Default is `Search` —
/// the picker opens with focus on the search bar so the user can type
/// immediately. Down enters the list; Up at list[0] returns to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Search,
    List,
}

#[derive(Debug)]
pub struct ListPicker {
    pub kind: ListPickerKind,
    pub title: String,
    pub items: Vec<ListPickerItem>,
    /// Index into the *current page* of the filtered view (0..PAGE_SIZE).
    /// Reset to 0 whenever the query or page changes.
    pub selected: usize,
    pub list_state: ListState,
    /// Empty-state hint shown when `items` is empty (no items, vs. no matches).
    pub empty_hint: String,
    /// Active search query. Empty = no filter.
    pub query: String,
    /// Current page index (0-based). Reset to 0 whenever the query changes.
    pub page: usize,
    /// Which UI element owns the cursor: search bar or list. Defaults
    /// to `Search`. Down moves Search→List; Up at list[0] returns to
    /// Search; typing returns focus to Search.
    pub focus: Focus,
}

impl ListPicker {
    pub fn new(
        kind: ListPickerKind,
        title: impl Into<String>,
        items: Vec<ListPickerItem>,
        preselect_key: Option<&str>,
        empty_hint: impl Into<String>,
    ) -> Self {
        let absolute = preselect_key
            .and_then(|k| items.iter().position(|i| i.key == k))
            .unwrap_or(0);
        let page = if items.is_empty() {
            0
        } else {
            absolute / PAGE_SIZE
        };
        let initial = absolute % PAGE_SIZE;
        let mut list_state = ListState::default();
        if !items.is_empty() {
            list_state.select(Some(initial));
        }
        Self {
            kind,
            title: title.into(),
            items,
            selected: initial,
            list_state,
            empty_hint: empty_hint.into(),
            query: String::new(),
            page,
            focus: Focus::Search,
        }
    }

    /// Total page count for the current filtered view (>= 1 even when
    /// the view is empty, so consumers can format `1/1` cleanly).
    pub fn page_count(&self) -> usize {
        let len = self.visible_len();
        if len == 0 {
            1
        } else {
            len.div_ceil(PAGE_SIZE)
        }
    }

    /// Slice of `filtered_indices` for the current page.
    pub fn page_indices(&self) -> Vec<usize> {
        let visible = self.filtered_indices();
        let start = self.page.saturating_mul(PAGE_SIZE);
        if start >= visible.len() {
            return Vec::new();
        }
        let end = (start + PAGE_SIZE).min(visible.len());
        visible[start..end].to_vec()
    }

    pub fn next_page(&mut self) {
        if self.page + 1 < self.page_count() {
            self.page += 1;
            self.selected = 0;
            self.list_state.select(Some(0));
            self.focus = Focus::Search;
        }
    }

    pub fn prev_page(&mut self) {
        if self.page > 0 {
            self.page -= 1;
            self.selected = 0;
            self.list_state.select(Some(0));
            self.focus = Focus::Search;
        }
    }

    /// Indices into `self.items` whose primary or secondary text
    /// case-insensitively contains the current query. Empty query
    /// returns all indices.
    pub fn filtered_indices(&self) -> Vec<usize> {
        if self.query.is_empty() {
            return (0..self.items.len()).collect();
        }
        let q = self.query.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.primary.to_lowercase().contains(&q)
                    || item.secondary.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Number of currently-visible rows after applying the query.
    pub fn visible_len(&self) -> usize {
        if self.query.is_empty() {
            self.items.len()
        } else {
            self.filtered_indices().len()
        }
    }

    pub fn move_up(&mut self) {
        match self.focus {
            // Up from search bar is a no-op (already at the top).
            Focus::Search => {}
            Focus::List => {
                if self.selected == 0 {
                    // At top of list → return cursor to search bar.
                    self.focus = Focus::Search;
                } else {
                    self.selected -= 1;
                    self.list_state.select(Some(self.selected));
                }
            }
        }
    }

    pub fn move_down(&mut self) {
        let len = self.page_indices().len();
        if len == 0 {
            return;
        }
        match self.focus {
            // First Down enters the list at the currently-highlighted
            // index (preserves the "preview" of what Enter would pick).
            Focus::Search => {
                self.focus = Focus::List;
                self.list_state.select(Some(self.selected));
            }
            Focus::List => {
                self.selected = (self.selected + 1) % len;
                self.list_state.select(Some(self.selected));
            }
        }
    }

    /// The currently-highlighted item, resolving page + selected back to
    /// the underlying `items` vec.
    pub fn current(&self) -> Option<&ListPickerItem> {
        let page = self.page_indices();
        let pos = self.selected.min(page.len().saturating_sub(1));
        page.get(pos).and_then(|i| self.items.get(*i))
    }

    /// Append a character to the query and reset the filtered cursor to
    /// the top of the new view.
    pub fn push_query_char(&mut self, c: char) {
        self.query.push(c);
        self.reset_selection_to_top();
    }

    /// Remove the last character from the query (no-op when empty).
    pub fn pop_query_char(&mut self) {
        if self.query.pop().is_some() {
            self.reset_selection_to_top();
        }
    }

    fn reset_selection_to_top(&mut self) {
        self.selected = 0;
        self.page = 0;
        // Typing implies the user is editing the query — return focus
        // to the search bar so subsequent Up/Down behave consistently.
        self.focus = Focus::Search;
        if self.visible_len() == 0 {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(0));
        }
    }

    /// Render the picker as a modal panel inside `area`. The search
    /// query is shown in the title bar (the top border line), so all
    /// inner rows go to list items — important in inline-viewport mode
    /// where vertical space is tight.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if area.height < 6 || area.width < 30 {
            return;
        }
        let panel = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        frame.render_widget(Clear, panel);

        let sky = Color::Rgb(94, 184, 255);
        let muted = Color::Rgb(107, 114, 128);
        let frame_color = Color::Rgb(40, 70, 140);
        let dark_bg = Color::Rgb(4, 11, 46);
        let coral = Color::Rgb(255, 138, 101);

        let visible_indices = self.filtered_indices();

        // Title bar doubles as the search input. Empty query → show the
        // hint; non-empty → show `Title › query▎  (n/total)`.
        let title_line = if self.query.is_empty() {
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    self.title.clone(),
                    Style::default().fg(sky).add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled("type", Style::default().fg(sky)),
                Span::styled(" to filter · ", Style::default().fg(muted)),
                Span::styled("↑/↓", Style::default().fg(sky)),
                Span::styled(" · ", Style::default().fg(muted)),
                Span::styled("Enter", Style::default().fg(sky)),
                Span::styled(" · ", Style::default().fg(muted)),
                Span::styled("Esc ", Style::default().fg(sky)),
            ])
        } else {
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    self.title.clone(),
                    Style::default().fg(sky).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" › ", Style::default().fg(muted)),
                Span::styled(
                    self.query.clone(),
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled("▎", Style::default().fg(coral)),
                Span::styled(
                    format!("  ({}/{}) ", visible_indices.len(), self.items.len()),
                    Style::default().fg(muted),
                ),
            ])
        };

        let block = Block::default()
            .title(title_line)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(frame_color));

        if self.items.is_empty() {
            let body = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {}", self.empty_hint),
                    Style::default().fg(muted),
                )),
            ])
            .block(block)
            .wrap(Wrap { trim: false });
            frame.render_widget(body, panel);
            return;
        }

        if visible_indices.is_empty() {
            let body = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  No matches for '{}'.", self.query),
                    Style::default().fg(muted),
                )),
            ])
            .block(block)
            .wrap(Wrap { trim: false });
            frame.render_widget(body, panel);
            return;
        }

        // Clamp selection to valid range against the filtered view.
        if self.selected >= visible_indices.len() {
            self.selected = visible_indices.len() - 1;
            self.list_state.select(Some(self.selected));
        }

        let items: Vec<ListItem> = visible_indices
            .iter()
            .enumerate()
            .map(|(filtered_i, original_i)| {
                let e = &self.items[*original_i];
                let highlight = filtered_i == self.selected;
                let primary_style = if highlight {
                    Style::default()
                        .fg(dark_bg)
                        .bg(sky)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(sky)
                };
                let secondary_style = if highlight {
                    Style::default().fg(dark_bg).bg(sky)
                } else {
                    Style::default().fg(muted)
                };
                let mut spans = vec![Span::styled(format!(" {} ", e.primary), primary_style)];
                if !e.secondary.is_empty() {
                    spans.push(Span::styled("   ", secondary_style));
                    spans.push(Span::styled(e.secondary.clone(), secondary_style));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        let list = List::new(items).block(block);
        frame.render_stateful_widget(list, panel, &mut self.list_state);
    }

    /// Fullscreen picker render — used when the picker has the entire
    /// alt-screen to itself. Layout: title row, search input box,
    /// scrollable list area, hotkey footer. Matches the Hermes /
    /// Claude-Code resume-picker UX.
    pub fn render_fullscreen(&mut self, frame: &mut Frame, area: Rect) {
        if area.height < 8 || area.width < 30 {
            return;
        }

        let sky = Color::Rgb(94, 184, 255);
        let muted = Color::Rgb(107, 114, 128);
        let frame_color = Color::Rgb(40, 70, 140);
        let dark_bg = Color::Rgb(4, 11, 46);
        let coral = Color::Rgb(255, 138, 101);
        let emerald = Color::Rgb(52, 211, 153);

        // Outer 1-row margin so the picker doesn't kiss the terminal
        // edges, then split into: title, search box, list, footer.
        let outer = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };
        frame.render_widget(Clear, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),     // title
                Constraint::Length(1),     // spacer
                Constraint::Length(3),     // search input box (bordered)
                Constraint::Length(1),     // spacer
                Constraint::Min(3),        // list
                Constraint::Length(2),     // footer (1 line + spacer)
            ])
            .split(outer);

        // Title — includes filtered count + page indicator.
        let visible_indices = self.filtered_indices();
        let page_count = self.page_count();
        let title_line = Line::from(vec![
            Span::styled(
                self.title.clone(),
                Style::default().fg(coral).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   {}/{}", visible_indices.len(), self.items.len()),
                Style::default().fg(muted),
            ),
            Span::styled(
                format!("  ·  page {}/{}", self.page + 1, page_count),
                Style::default().fg(muted),
            ),
        ]);
        frame.render_widget(Paragraph::new(title_line), chunks[0]);

        // Search box (bordered, rounded). Border lights up coral when
        // the search bar has focus; dim frame color otherwise. Cursor
        // block is shown only when focused.
        let search_focused = self.focus == Focus::Search;
        let search_border_color = if search_focused { coral } else { frame_color };
        let search_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(search_border_color));
        let search_line = if self.query.is_empty() {
            let mut spans = vec![
                Span::styled(" 🔎 ", Style::default().fg(sky)),
                Span::styled(
                    "Search…",
                    Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                ),
            ];
            if search_focused {
                spans.insert(
                    2,
                    Span::styled("▎ ", Style::default().fg(coral)),
                );
            }
            Line::from(spans)
        } else {
            let mut spans = vec![
                Span::styled(" 🔎 ", Style::default().fg(sky)),
                Span::styled(
                    self.query.clone(),
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
            ];
            if search_focused {
                spans.push(Span::styled("▎", Style::default().fg(coral)));
            }
            Line::from(spans)
        };
        let search_widget = Paragraph::new(search_line).block(search_block);
        frame.render_widget(search_widget, chunks[2]);

        // List.
        let list_area = chunks[4];
        if self.items.is_empty() {
            let body = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {}", self.empty_hint),
                    Style::default().fg(muted),
                )),
            ])
            .wrap(Wrap { trim: false });
            frame.render_widget(body, list_area);
        } else if visible_indices.is_empty() {
            let body = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  No matches for '{}'.", self.query),
                    Style::default().fg(muted),
                )),
            ])
            .wrap(Wrap { trim: false });
            frame.render_widget(body, list_area);
        } else {
            // Render only the current page slice.
            let page_indices = self.page_indices();
            if self.selected >= page_indices.len() && !page_indices.is_empty() {
                self.selected = page_indices.len() - 1;
                self.list_state.select(Some(self.selected));
            }

            // Each item is rendered as TWO lines (Claude-Code-style):
            // primary text on row 1, muted secondary on row 2. The
            // selected row glows when the LIST has focus; while focus
            // sits on the search bar it stays a muted "preview" so
            // the user can tell which area owns the cursor.
            let list_focused = self.focus == Focus::List;
            let items: Vec<ListItem> = page_indices
                .iter()
                .enumerate()
                .map(|(page_i, original_i)| {
                    let e = &self.items[*original_i];
                    let is_selected = page_i == self.selected;
                    let highlight = is_selected && list_focused;
                    let arrow = if highlight {
                        "▸ "
                    } else if is_selected {
                        "› "
                    } else {
                        "  "
                    };
                    let primary_style = if highlight {
                        Style::default().fg(emerald).add_modifier(Modifier::BOLD)
                    } else if is_selected {
                        Style::default().fg(sky).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(sky)
                    };
                    let secondary_style = Style::default().fg(muted);
                    let mut lines = vec![Line::from(vec![
                        Span::styled(arrow, primary_style),
                        Span::styled(e.primary.clone(), primary_style),
                    ])];
                    if !e.secondary.is_empty() {
                        lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(e.secondary.clone(), secondary_style),
                        ]));
                    }
                    lines.push(Line::from(""));
                    ListItem::new(lines)
                })
                .collect();
            let _ = dark_bg; // intentionally unused in fullscreen variant
            let list = List::new(items);
            frame.render_stateful_widget(list, list_area, &mut self.list_state);
        }

        // Footer with hotkey help.
        let footer = Line::from(vec![
            Span::styled("↑/↓", Style::default().fg(sky)),
            Span::styled(" navigate · ", Style::default().fg(muted)),
            Span::styled("←/→", Style::default().fg(sky)),
            Span::styled(" page · ", Style::default().fg(muted)),
            Span::styled("type", Style::default().fg(sky)),
            Span::styled(" to filter · ", Style::default().fg(muted)),
            Span::styled("Enter", Style::default().fg(sky)),
            Span::styled(" select · ", Style::default().fg(muted)),
            Span::styled("Esc", Style::default().fg(sky)),
            Span::styled(" cancel", Style::default().fg(muted)),
        ]);
        frame.render_widget(Paragraph::new(footer), chunks[5]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, primary: &str) -> ListPickerItem {
        ListPickerItem {
            key: key.into(),
            primary: primary.into(),
            secondary: format!("{primary} desc"),
        }
    }

    fn picker(items: Vec<ListPickerItem>) -> ListPicker {
        ListPicker::new(ListPickerKind::Model, "Test", items, None, "empty")
    }

    #[test]
    fn picker_starts_with_search_focus() {
        let p = picker(vec![item("a", "x"), item("b", "y")]);
        assert_eq!(p.focus, Focus::Search);
    }

    #[test]
    fn first_down_moves_focus_to_list_without_advancing() {
        let mut p = picker(vec![item("a", "x"), item("b", "y")]);
        assert_eq!(p.selected, 0);
        p.move_down();
        assert_eq!(p.focus, Focus::List);
        assert_eq!(p.selected, 0); // first item is now actively selected
        p.move_down();
        assert_eq!(p.selected, 1);
        p.move_down();
        assert_eq!(p.selected, 0); // wraps within list
    }

    #[test]
    fn up_at_list_top_returns_focus_to_search() {
        let mut p = picker(vec![item("a", "x"), item("b", "y")]);
        p.move_down(); // focus → List
        assert_eq!(p.focus, Focus::List);
        p.move_up(); // selected is 0 → focus → Search
        assert_eq!(p.focus, Focus::Search);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn up_in_search_is_a_noop() {
        let mut p = picker(vec![item("a", "x"), item("b", "y")]);
        assert_eq!(p.focus, Focus::Search);
        p.move_up();
        assert_eq!(p.focus, Focus::Search);
    }

    #[test]
    fn typing_returns_focus_to_search() {
        let mut p = picker(vec![item("a", "alpha"), item("b", "beta")]);
        p.move_down(); // focus → List
        assert_eq!(p.focus, Focus::List);
        p.push_query_char('a');
        assert_eq!(p.focus, Focus::Search);
    }

    #[test]
    fn paging_returns_focus_to_search() {
        let mut p = picker(many_items(12));
        p.move_down(); // focus → List
        p.next_page();
        assert_eq!(p.focus, Focus::Search);
    }

    #[test]
    fn picker_preselects_key() {
        let p = ListPicker::new(
            ListPickerKind::Session,
            "Sessions",
            vec![item("aaa", "first"), item("bbb", "second")],
            Some("bbb"),
            "no sessions",
        );
        assert_eq!(p.selected, 1);
        assert_eq!(p.current().unwrap().key, "bbb");
    }

    #[test]
    fn empty_picker_has_no_selection() {
        let p = ListPicker::new(
            ListPickerKind::Personality,
            "Personality",
            vec![],
            None,
            "no presets",
        );
        assert!(p.current().is_none());
    }

    #[test]
    fn query_filters_by_primary_text() {
        let mut p = picker(vec![
            item("a", "fix login bug"),
            item("b", "refactor payments"),
            item("c", "ship login redesign"),
        ]);
        p.push_query_char('l');
        p.push_query_char('o');
        p.push_query_char('g');
        let visible = p.filtered_indices();
        assert_eq!(visible, vec![0, 2]);
        assert_eq!(p.selected, 0);
        assert_eq!(p.current().unwrap().key, "a");
    }

    #[test]
    fn query_filters_by_secondary_text() {
        let mut p = picker(vec![
            ListPickerItem {
                key: "a".into(),
                primary: "session-1".into(),
                secondary: "yesterday · 5 msgs".into(),
            },
            ListPickerItem {
                key: "b".into(),
                primary: "session-2".into(),
                secondary: "today · 12 msgs".into(),
            },
        ]);
        p.push_query_char('1');
        p.push_query_char('2');
        let visible = p.filtered_indices();
        assert_eq!(visible, vec![1]);
        assert_eq!(p.current().unwrap().key, "b");
    }

    #[test]
    fn query_is_case_insensitive() {
        let mut p = picker(vec![item("a", "Fix Login Bug")]);
        p.push_query_char('l');
        p.push_query_char('O');
        p.push_query_char('g');
        assert_eq!(p.filtered_indices(), vec![0]);
    }

    #[test]
    fn pop_query_char_restores_visibility() {
        let mut p = picker(vec![item("a", "alpha"), item("b", "beta")]);
        p.push_query_char('a');
        assert_eq!(p.filtered_indices(), vec![0, 1]); // both contain 'a'
        p.push_query_char('l');
        assert_eq!(p.filtered_indices(), vec![0]);
        p.pop_query_char();
        assert_eq!(p.filtered_indices(), vec![0, 1]);
        p.pop_query_char();
        assert_eq!(p.query, "");
    }

    #[test]
    fn navigation_wraps_within_filtered_view() {
        let mut p = picker(vec![
            item("a", "alpha"),
            item("b", "beta"),
            item("c", "gamma"),
        ]);
        p.push_query_char('a'); // matches alpha, beta, gamma (all have 'a')
        assert_eq!(p.visible_len(), 3);
        // narrow further
        p.push_query_char('m'); // alpha no, beta no, gamma yes
        assert_eq!(p.visible_len(), 1);
        p.move_down();
        assert_eq!(p.selected, 0); // wraps within 1 visible
        assert_eq!(p.current().unwrap().key, "c");
    }

    #[test]
    fn no_matches_means_no_current_item() {
        let mut p = picker(vec![item("a", "alpha")]);
        p.push_query_char('z');
        assert_eq!(p.visible_len(), 0);
        assert!(p.current().is_none());
    }

    fn many_items(n: usize) -> Vec<ListPickerItem> {
        (0..n)
            .map(|i| item(&format!("k{i}"), &format!("item-{i:02}")))
            .collect()
    }

    #[test]
    fn pagination_splits_visible_into_pages_of_five() {
        let p = picker(many_items(12));
        assert_eq!(p.page_count(), 3);
        assert_eq!(p.page, 0);
        assert_eq!(p.page_indices().len(), 5);
        assert_eq!(p.page_indices(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn next_page_advances_and_caps_at_last() {
        let mut p = picker(many_items(12));
        p.next_page();
        assert_eq!(p.page, 1);
        assert_eq!(p.page_indices(), vec![5, 6, 7, 8, 9]);
        p.next_page();
        assert_eq!(p.page, 2);
        assert_eq!(p.page_indices(), vec![10, 11]);
        // Stays on last page on further next_page calls.
        p.next_page();
        assert_eq!(p.page, 2);
    }

    #[test]
    fn prev_page_caps_at_first() {
        let mut p = picker(many_items(12));
        p.next_page();
        p.next_page();
        p.prev_page();
        assert_eq!(p.page, 1);
        p.prev_page();
        assert_eq!(p.page, 0);
        p.prev_page();
        assert_eq!(p.page, 0);
    }

    #[test]
    fn navigation_wraps_within_current_page_only() {
        let mut p = picker(many_items(12));
        // First Down moves focus from Search→List (selected stays 0).
        // Subsequent Downs advance: 0→1→2→3→4 (4 advances after entry).
        p.move_down(); // enter list
        assert_eq!(p.selected, 0);
        for _ in 0..4 {
            p.move_down();
        }
        assert_eq!(p.selected, 4);
        p.move_down();
        assert_eq!(p.selected, 0); // wrapped within page, not advanced
        p.move_up(); // selected=0 → focus back to Search, selected stays 0
        assert_eq!(p.focus, Focus::Search);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn query_resets_page_to_zero() {
        let mut p = picker(many_items(12));
        p.next_page();
        p.next_page();
        assert_eq!(p.page, 2);
        p.push_query_char('1'); // matches item-10, item-11 (and item-01)
        assert_eq!(p.page, 0);
    }

    #[test]
    fn current_resolves_through_page_offset() {
        let mut p = picker(many_items(12));
        p.next_page(); // page 1: items 5..10; focus reset to Search
        assert_eq!(p.selected, 0);
        assert_eq!(p.current().unwrap().key, "k5");
        p.move_down(); // first Down: enter list at selected=0
        assert_eq!(p.current().unwrap().key, "k5");
        p.move_down(); // advance to second item
        assert_eq!(p.current().unwrap().key, "k6");
    }

    #[test]
    fn preselect_lands_on_correct_page() {
        let p = ListPicker::new(
            ListPickerKind::Model,
            "Test",
            many_items(12),
            Some("k7"),
            "empty",
        );
        // k7 is at absolute index 7 → page 1, selected 2.
        assert_eq!(p.page, 1);
        assert_eq!(p.selected, 2);
        assert_eq!(p.current().unwrap().key, "k7");
    }
}
