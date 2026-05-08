//! Generic interactive list picker — modal overlay navigated via
//! Up/Down/Enter/Esc, with a built-in search row at the top: any
//! printable character types into the query and filters the list live;
//! Backspace removes a character; Up/Down move within the filtered
//! view; Enter selects the highlighted match; Esc dismisses. Used by
//! `/model`, `/sessions`, `/resume`, `/personality`, and any future
//! picker. Each picker carries a `kind` tag so the app's key handler
//! can dispatch the right action when the user presses Enter.
//!
//! Collapsible category headers are supported: `→` or `Enter` on a
//! category header toggles its collapsed state. Collapsed categories
//! show only the header; expanded categories show the header + their
//! items. Search filters across all visible text.

use std::collections::HashSet;

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
    /// Top-level setup category picker — Persona, Channels, etc.
    SetupTopic,
    /// Channel-type picker opened from the Channels category.
    SetupChannel,
    /// ClawHub install browser — opens via `/install` and `/skills install`.
    /// Selecting a row installs that skill via `clawhub::install_one`.
    ClawhubInstall,
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

/// An entry in the list picker — either a regular item or a collapsible
/// category header.
#[derive(Debug, Clone)]
pub enum ListPickerEntry {
    Item(ListPickerItem),
    CategoryHeader {
        id: String,
        label: String,
        item_count: usize,
        collapsed: bool,
    },
}

impl ListPickerEntry {
    pub fn category_header(
        id: impl Into<String>,
        label: impl Into<String>,
        item_count: usize,
    ) -> Self {
        Self::CategoryHeader {
            id: id.into(),
            label: label.into(),
            item_count,
            collapsed: false,
        }
    }

    pub fn as_item(&self) -> Option<&ListPickerItem> {
        match self {
            Self::Item(i) => Some(i),
            Self::CategoryHeader { .. } => None,
        }
    }

    pub fn as_header(&self) -> Option<(&str, &str, usize, bool)> {
        match self {
            Self::CategoryHeader {
                id,
                label,
                item_count,
                collapsed,
            } => Some((id.as_str(), label.as_str(), *item_count, *collapsed)),
            Self::Item(_) => None,
        }
    }

    fn primary_text(&self) -> String {
        match self {
            Self::Item(i) => i.primary.clone(),
            Self::CategoryHeader { label, .. } => label.clone(),
        }
    }

    fn secondary_text(&self) -> String {
        match self {
            Self::Item(i) => i.secondary.clone(),
            Self::CategoryHeader { item_count, .. } => format!(
                "{} item{}",
                item_count,
                if *item_count == 1 { "" } else { "s" }
            ),
        }
    }
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
    /// Cursor is on a category header — Enter/→ toggles collapse.
    Category,
}

#[derive(Debug)]
pub struct ListPicker {
    pub kind: ListPickerKind,
    pub title: String,
    /// Flat list of entries (items + category headers). Items belonging
    /// to a collapsed category are still present but skipped by
    /// `filtered_indices()` so they don't appear in navigation.
    entries: Vec<ListPickerEntry>,
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
    /// Category IDs that are currently collapsed. Navigation skips items
    /// in collapsed categories unless a search query is active.
    collapsed_categories: HashSet<String>,
}

impl ListPicker {
    /// Returns the underlying entries (items + category headers).
    pub fn entries(&self) -> &[ListPickerEntry] {
        &self.entries
    }

    /// Replace the picker's items in-place. Used by live server-side
    /// search (e.g. ClawHub install picker): the user keeps typing in
    /// the search bar while results stream in from the network. Resets
    /// page/selection but preserves the query and focus so the user's
    /// typing context isn't disturbed.
    pub fn set_items(&mut self, items: Vec<ListPickerItem>) {
        self.entries = items.into_iter().map(ListPickerEntry::Item).collect();
        self.page = 0;
        self.selected = 0;
        if self.visible_len() == 0 {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(0));
        }
    }

    pub fn new(
        kind: ListPickerKind,
        title: impl Into<String>,
        items: Vec<ListPickerItem>,
        preselect_key: Option<&str>,
        empty_hint: impl Into<String>,
    ) -> Self {
        let entries: Vec<ListPickerEntry> = items.into_iter().map(ListPickerEntry::Item).collect();
        let absolute = preselect_key
            .and_then(|k| {
                entries
                    .iter()
                    .position(|e| e.as_item().is_some_and(|i| i.key == k))
            })
            .unwrap_or(0);
        let page = if entries.is_empty() {
            0
        } else {
            absolute / PAGE_SIZE
        };
        let initial = absolute % PAGE_SIZE;
        let mut list_state = ListState::default();
        if !entries.is_empty() {
            list_state.select(Some(initial));
        }
        Self {
            kind,
            title: title.into(),
            entries,
            selected: initial,
            list_state,
            empty_hint: empty_hint.into(),
            query: String::new(),
            page,
            focus: Focus::Search,
            collapsed_categories: HashSet::new(),
        }
    }

    /// Create a picker from pre-grouped entries (items + category headers).
    /// `preselect_key` matches against `Entry::Item.key` values only.
    pub fn with_entries(
        kind: ListPickerKind,
        title: impl Into<String>,
        entries: Vec<ListPickerEntry>,
        preselect_key: Option<&str>,
        empty_hint: impl Into<String>,
    ) -> Self {
        let absolute = preselect_key
            .and_then(|k| {
                entries
                    .iter()
                    .position(|e| e.as_item().is_some_and(|i| i.key == k))
            })
            .unwrap_or(0);
        let page = if entries.is_empty() {
            0
        } else {
            absolute / PAGE_SIZE
        };
        let initial = absolute % PAGE_SIZE;
        let mut list_state = ListState::default();
        if !entries.is_empty() {
            list_state.select(Some(initial));
        }
        Self {
            kind,
            title: title.into(),
            entries,
            selected: initial,
            list_state,
            empty_hint: empty_hint.into(),
            query: String::new(),
            page,
            focus: Focus::Search,
            collapsed_categories: HashSet::new(),
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

    /// Indices into `self.entries` that are currently visible (accounting
    /// for collapsed categories and optional query filter).
    ///
    /// `ClawhubInstall` opts out of local filtering: the query buffer is
    /// only consulted on Enter (which fires a server-side search). Any
    /// in-flight typing leaves the visible list unchanged so the user
    /// isn't confused by results narrowing without an explicit search.
    pub fn filtered_indices(&self) -> Vec<usize> {
        let server_search_picker = self.kind == ListPickerKind::ClawhubInstall;
        let searching = !server_search_picker && !self.query.is_empty();
        let q = self.query.to_lowercase();

        self.entries
            .iter()
            .enumerate()
            .filter(|(i, entry)| {
                match entry {
                    ListPickerEntry::CategoryHeader { id, collapsed, .. } => {
                        // Always show headers in filtered results so users can
                        // navigate to them. When collapsed, only the header
                        // itself is visible (its items are excluded above).
                        if searching {
                            // When searching, show the header if the query matches
                            // the header label text.
                            let label_matches = id.to_lowercase().contains(&q);
                            if !label_matches && !*collapsed {
                                // Also show if any non-collapsed child item matches.
                                // This requires looking ahead — simpler: include all
                                // headers when searching so items under them are reachable.
                                return true;
                            }
                            return label_matches;
                        }
                        // No query: always include headers (even collapsed ones).
                        true
                    }
                    ListPickerEntry::Item(item) => {
                        // When searching, items in collapsed categories are hidden.
                        if !searching {
                            return true;
                        }
                        item.primary.to_lowercase().contains(&q)
                            || item.secondary.to_lowercase().contains(&q)
                    }
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Number of currently-visible rows after applying the query and
    /// collapsed-category state.
    pub fn visible_len(&self) -> usize {
        self.filtered_indices().len()
    }

    pub fn move_up(&mut self) {
        match self.focus {
            Focus::Search => {}
            Focus::List | Focus::Category => {
                // v0.6.8: at the first item of a page that's not the
                // first page, move to the last item of the previous
                // page (mirror of the cross-page move_down). On page 0
                // the existing "back to search" behavior is preserved.
                if self.selected == 0 && self.page > 0 {
                    self.page -= 1;
                    let prev_len = self.page_indices().len();
                    self.selected = prev_len.saturating_sub(1);
                    self.list_state.select(Some(self.selected));
                    return;
                }
                if self.selected == 0 {
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
            Focus::Search => {
                self.focus = if matches!(
                    self.entries.get(self.selected),
                    Some(ListPickerEntry::CategoryHeader { .. })
                ) {
                    Focus::Category
                } else {
                    Focus::List
                };
                self.list_state.select(Some(self.selected));
            }
            Focus::List | Focus::Category => {
                // v0.6.8: at the last item of the current page, advance
                // to the next page instead of wrapping to top of same
                // page. Tester report (bug-hunt round 2): "user should
                // be able to scroll down" — they pressed ↓ at item 3/3
                // expecting more items, got page-1 row 1 instead of
                // page-2 row 1. Cross-page traversal matches every
                // other list-style TUI dialect.
                if self.selected + 1 >= len {
                    if self.page + 1 < self.page_count() {
                        self.page += 1;
                        self.selected = 0;
                    } else {
                        // Already on last item of last page — keep
                        // wrap-to-top behavior for cyclic browsing.
                        self.selected = 0;
                    }
                } else {
                    self.selected += 1;
                }
                self.list_state.select(Some(self.selected));
            }
        }
    }

    /// Toggle the collapsed state of the category at the current cursor
    /// position. Safe to call even when the current entry is not a
    /// category header (no-op).
    pub fn toggle_current_category(&mut self) {
        let idx = match self.page_indices().get(self.selected).copied() {
            Some(i) => i,
            None => return,
        };
        let Some(ListPickerEntry::CategoryHeader { id, collapsed, .. }) = self.entries.get(idx)
        else {
            return;
        };
        if *collapsed {
            self.collapsed_categories.remove(id);
        } else {
            self.collapsed_categories.insert(id.clone());
        }
        // Keep focus on the header.
        self.focus = Focus::Category;
    }

    /// The currently-highlighted entry, resolving page + selected back to
    /// the underlying `entries` vec.
    pub fn current_entry(&self) -> Option<&ListPickerEntry> {
        let page = self.page_indices();
        let pos = self.selected.min(page.len().saturating_sub(1));
        page.get(pos).and_then(|i| self.entries.get(*i))
    }

    /// The currently-highlighted item, or `None` if a category header
    /// is currently selected.
    pub fn current(&self) -> Option<&ListPickerItem> {
        self.current_entry().and_then(|e| e.as_item())
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
                    format!("  ({}/{}) ", visible_indices.len(), self.entries.len()),
                    Style::default().fg(muted),
                ),
            ])
        };

        let block = Block::default()
            .title(title_line)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(frame_color));

        if self.entries.is_empty() {
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
            self.selected = visible_indices.len().saturating_sub(1);
            self.list_state.select(Some(self.selected));
        }

        let items: Vec<ListItem> = visible_indices
            .iter()
            .enumerate()
            .map(|(filtered_i, original_i)| {
                let entry = &self.entries[*original_i];
                let highlight = filtered_i == self.selected;
                match entry {
                    ListPickerEntry::CategoryHeader {
                        label,
                        item_count,
                        collapsed,
                        ..
                    } => {
                        let arrow = if *collapsed { "▶" } else { "▼" };
                        let primary_style = if highlight {
                            Style::default()
                                .fg(dark_bg)
                                .bg(frame_color)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                                .fg(frame_color)
                                .add_modifier(Modifier::BOLD)
                        };
                        let count_style = if highlight {
                            Style::default().fg(dark_bg).bg(frame_color)
                        } else {
                            Style::default().fg(muted)
                        };
                        let spans = vec![
                            Span::styled(format!(" {} ", arrow), primary_style),
                            Span::styled(label.clone(), primary_style),
                            Span::styled(format!("  ({}) ", item_count), count_style),
                        ];
                        ListItem::new(Line::from(spans))
                    }
                    ListPickerEntry::Item(item) => {
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
                        let mut spans =
                            vec![Span::styled(format!(" {} ", item.primary), primary_style)];
                        if !item.secondary.is_empty() {
                            spans.push(Span::styled("   ", secondary_style));
                            spans.push(Span::styled(item.secondary.clone(), secondary_style));
                        }
                        ListItem::new(Line::from(spans))
                    }
                }
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
                Constraint::Length(1), // title
                Constraint::Length(1), // spacer
                Constraint::Length(3), // search input box (bordered)
                Constraint::Length(1), // spacer
                Constraint::Min(3),    // list
                Constraint::Length(2), // footer (1 line + spacer)
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
                format!("   {}/{}", visible_indices.len(), self.entries.len()),
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
                spans.insert(2, Span::styled("▎ ", Style::default().fg(coral)));
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
        if self.entries.is_empty() {
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
                self.selected = page_indices.len().saturating_sub(1);
                self.list_state.select(Some(self.selected));
            }

            let category_focused = self.focus == Focus::Category;
            let list_focused = self.focus == Focus::List;
            let items: Vec<ListItem> = page_indices
                .iter()
                .enumerate()
                .map(|(page_i, original_i)| {
                    let entry = &self.entries[*original_i];
                    let is_selected = page_i == self.selected;
                    match entry {
                        ListPickerEntry::CategoryHeader {
                            label,
                            item_count,
                            collapsed,
                            ..
                        } => {
                            let highlight = is_selected && category_focused;
                            let arrow = if *collapsed { "▶" } else { "▼" };
                            let primary_style = if highlight {
                                Style::default()
                                    .fg(dark_bg)
                                    .bg(frame_color)
                                    .add_modifier(Modifier::BOLD)
                            } else if is_selected {
                                Style::default()
                                    .fg(frame_color)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(frame_color)
                            };
                            let count_style = if highlight {
                                Style::default().fg(dark_bg).bg(frame_color)
                            } else {
                                Style::default().fg(muted)
                            };
                            let toggle_hint = if highlight { " ◀▶ " } else { "    " };
                            let mut lines = vec![Line::from(vec![
                                Span::styled(toggle_hint, primary_style),
                                Span::styled(format!("{} ", arrow), primary_style),
                                Span::styled(label.clone(), primary_style),
                            ])];
                            lines.push(Line::from(vec![
                                Span::raw("       "),
                                Span::styled(
                                    format!(
                                        "{} item{}",
                                        item_count,
                                        if *item_count == 1 { "" } else { "s" }
                                    ),
                                    count_style,
                                ),
                            ]));
                            lines.push(Line::from(""));
                            ListItem::new(lines)
                        }
                        ListPickerEntry::Item(item) => {
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
                                Span::styled(item.primary.clone(), primary_style),
                            ])];
                            if !item.secondary.is_empty() {
                                lines.push(Line::from(vec![
                                    Span::raw("  "),
                                    Span::styled(item.secondary.clone(), secondary_style),
                                ]));
                            }
                            lines.push(Line::from(""));
                            ListItem::new(lines)
                        }
                    }
                })
                .collect();
            let _ = dark_bg; // intentionally unused in fullscreen variant
            let list = List::new(items);
            frame.render_stateful_widget(list, list_area, &mut self.list_state);
        }

        // Footer with hotkey help. ClawhubInstall has a two-mode Enter
        // (search vs install depending on focus) so its hint differs.
        let footer = if self.kind == ListPickerKind::ClawhubInstall {
            Line::from(vec![
                Span::styled("type + Enter", Style::default().fg(sky)),
                Span::styled(" search · ", Style::default().fg(muted)),
                Span::styled("↑/↓", Style::default().fg(sky)),
                Span::styled(" navigate · ", Style::default().fg(muted)),
                Span::styled("Enter", Style::default().fg(sky)),
                Span::styled(" install · ", Style::default().fg(muted)),
                Span::styled("Esc", Style::default().fg(sky)),
                Span::styled(" close", Style::default().fg(muted)),
            ])
        } else {
            Line::from(vec![
                Span::styled("↑/↓", Style::default().fg(sky)),
                Span::styled(" navigate · ", Style::default().fg(muted)),
                Span::styled("←/→", Style::default().fg(sky)),
                Span::styled(" collapse · ", Style::default().fg(muted)),
                Span::styled("type", Style::default().fg(sky)),
                Span::styled(" to filter · ", Style::default().fg(muted)),
                Span::styled("Enter", Style::default().fg(sky)),
                Span::styled(" select · ", Style::default().fg(muted)),
                Span::styled("Esc", Style::default().fg(sky)),
                Span::styled(" cancel", Style::default().fg(muted)),
            ])
        };
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

    fn clawhub_picker(items: Vec<ListPickerItem>) -> ListPicker {
        ListPicker::new(
            ListPickerKind::ClawhubInstall,
            "Install",
            items,
            None,
            "empty",
        )
    }

    #[test]
    fn clawhub_picker_does_not_filter_locally_when_typing() {
        // Tester contract: typing in the ClawhubInstall picker only
        // updates the query buffer. Visible items must NOT shrink — the
        // server search fires on Enter, and the picker stays put until
        // results actually arrive. Local filtering would confuse users.
        let mut p = clawhub_picker(vec![
            item("github", "github"),
            item("obsidian", "obsidian"),
            item("weather", "weather"),
        ]);
        assert_eq!(p.visible_len(), 3);
        p.push_query_char('g');
        p.push_query_char('h');
        // Query updated, but visible count unchanged — no local filter.
        assert_eq!(p.query, "gh");
        assert_eq!(p.visible_len(), 3);
        p.push_query_char('z'); // garbage chars; still no filter
        assert_eq!(p.visible_len(), 3);
    }

    #[test]
    fn non_clawhub_picker_still_filters_locally() {
        // Sanity: model/session/etc pickers retain local filter
        // semantics — only ClawhubInstall opts out.
        let mut p = picker(vec![
            item("alpha", "alpha"),
            item("beta", "beta"),
            item("gamma", "gamma"),
        ]);
        assert_eq!(p.visible_len(), 3);
        p.push_query_char('a');
        // "alpha" and "gamma" both contain 'a', "beta" does too.
        assert!(p.visible_len() < 4);
        p.push_query_char('l');
        // "al" only matches "alpha".
        assert_eq!(p.visible_len(), 1);
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

    #[test]
    fn setup_topic_kind_distinct_from_channel_kind() {
        assert_ne!(ListPickerKind::SetupTopic, ListPickerKind::SetupChannel);
    }
}
