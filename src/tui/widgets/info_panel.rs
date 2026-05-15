//! Read-only info-panel overlay used by `/channels`, `/config`, `/doctor`,
//! `/insights`, `/status`, `/usage`, `/skill` (no args).
//!
//! Pre-v0.6.8 these commands all returned `CommandResult::Message(String)`,
//! which renders as a regular `System:` chat line — same surface as agent
//! replies, channel events, and errors. Tester report (bug-hunt round 2):
//! "Change the shitty on chat ui or infos to proper tui comp ui." Fair.
//!
//! `InfoPanel` is the consistent pattern: a bordered modal with a sky-bold
//! title, sections of typed rows (KeyValue / Status / Bullet / Spacer /
//! Plain), an optional footer hint, and built-in scroll when content
//! overflows the viewport. Visual language matches `list_picker.rs` so the
//! whole TUI reads as one coherent surface.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

/// Visual status icon for a `Status` row. Determines glyph + color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// Green check — health probe passed, channel polling, file present.
    Ok,
    /// Coral warn — soft failure, "configured but not active", deferred work.
    Warn,
    /// Red fail — hard failure that needs attention.
    Fail,
    /// Sky info — neutral state worth surfacing.
    Info,
}

impl StatusKind {
    fn glyph(self) -> &'static str {
        match self {
            StatusKind::Ok => "✓",
            StatusKind::Warn => "⚠",
            StatusKind::Fail => "✗",
            StatusKind::Info => "·",
        }
    }
}

/// One row of content inside an `InfoSection`. Variants are typed so the
/// renderer can apply consistent spacing + color without each callsite
/// re-deriving the pattern.
#[derive(Debug, Clone)]
pub enum InfoRow {
    /// `Key                       value` — left-aligned key in muted, value
    /// right-aligned (or after a fixed gap) in normal weight. Used for
    /// Provider / Model / Profile / Workspace / etc.
    KeyValue { key: String, value: String },
    /// Status icon + label + optional inline detail.
    /// `✓ Telegram          polling`
    Status {
        kind: StatusKind,
        label: String,
        detail: Option<String>,
    },
    /// `· primary` with optional indented secondary line in muted.
    Bullet {
        primary: String,
        secondary: Option<String>,
    },
    /// Comma-separated list rendered in muted, two-space indented.
    /// Used for "Not configured: Discord, Slack, ...".
    InlineList { items: Vec<String> },
    /// Free-form line. Caller controls content; renders in normal style
    /// with no prefix.
    Plain(String),
    /// Vertical whitespace.
    Spacer,
}

/// Group of rows under an optional heading. The heading is rendered in
/// sky color so it pops against the muted body content.
#[derive(Debug, Clone)]
pub struct InfoSection {
    pub heading: Option<String>,
    pub rows: Vec<InfoRow>,
}

impl InfoSection {
    pub fn new<H: Into<String>>(heading: H) -> Self {
        Self {
            heading: Some(heading.into()),
            rows: Vec::new(),
        }
    }

    pub fn unheaded() -> Self {
        Self {
            heading: None,
            rows: Vec::new(),
        }
    }

    pub fn key_value<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.rows.push(InfoRow::KeyValue {
            key: key.into(),
            value: value.into(),
        });
        self
    }

    pub fn status<L: Into<String>>(mut self, kind: StatusKind, label: L) -> Self {
        self.rows.push(InfoRow::Status {
            kind,
            label: label.into(),
            detail: None,
        });
        self
    }

    pub fn status_with<L: Into<String>, D: Into<String>>(
        mut self,
        kind: StatusKind,
        label: L,
        detail: D,
    ) -> Self {
        self.rows.push(InfoRow::Status {
            kind,
            label: label.into(),
            detail: Some(detail.into()),
        });
        self
    }

    pub fn bullet<P: Into<String>>(mut self, primary: P) -> Self {
        self.rows.push(InfoRow::Bullet {
            primary: primary.into(),
            secondary: None,
        });
        self
    }

    pub fn bullet_with<P: Into<String>, S: Into<String>>(
        mut self,
        primary: P,
        secondary: S,
    ) -> Self {
        self.rows.push(InfoRow::Bullet {
            primary: primary.into(),
            secondary: Some(secondary.into()),
        });
        self
    }

    pub fn inline_list<I, S>(mut self, items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rows.push(InfoRow::InlineList {
            items: items.into_iter().map(Into::into).collect(),
        });
        self
    }

    pub fn plain<L: Into<String>>(mut self, line: L) -> Self {
        self.rows.push(InfoRow::Plain(line.into()));
        self
    }

    pub fn spacer(mut self) -> Self {
        self.rows.push(InfoRow::Spacer);
        self
    }
}

/// A read-only modal overlay rendered above the chat scrollback. Built
/// by the command handler, returned via `CommandResult::OpenInfoPanel`,
/// owned by `TuiApp::info_panel`. Closed with `Esc`.
#[derive(Debug, Clone)]
pub struct InfoPanel {
    pub title: String,
    pub subtitle: Option<String>,
    pub sections: Vec<InfoSection>,
    pub footer_hint: Option<String>,
    /// First visible line index in the rendered body. Caller mutates via
    /// `scroll_up` / `scroll_down` in response to ↑/↓ when no chat input
    /// has focus.
    pub scroll_offset: u16,
}

impl InfoPanel {
    pub fn new<T: Into<String>>(title: T) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            sections: Vec::new(),
            footer_hint: None,
            scroll_offset: 0,
        }
    }

    pub fn with_subtitle<S: Into<String>>(mut self, subtitle: S) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    pub fn with_footer<F: Into<String>>(mut self, footer: F) -> Self {
        self.footer_hint = Some(footer.into());
        self
    }

    pub fn section(mut self, section: InfoSection) -> Self {
        self.sections.push(section);
        self
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    pub fn page_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(10);
    }

    pub fn page_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(10);
    }

    /// Render the panel into `area`. The widget owns its own clear so the
    /// chat scrollback underneath is hidden cleanly.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.width < 12 || area.height < 4 {
            return;
        }
        let panel = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        frame.render_widget(Clear, panel);

        // Brand colors — must stay in sync with list_picker.rs and
        // setup_overlay.rs so the surfaces feel like one app.
        let sky = Color::Rgb(94, 184, 255);
        let muted = Color::Rgb(107, 114, 128);
        let frame_color = Color::Rgb(40, 70, 140);
        let coral = Color::Rgb(255, 138, 101);
        let emerald = Color::Rgb(52, 211, 153);
        let red = Color::Rgb(248, 113, 113);

        // Title bar: title + optional subtitle + close hint, mirroring
        // list_picker's header structure.
        let mut title_spans: Vec<Span<'static>> = vec![
            Span::raw(" "),
            Span::styled(
                self.title.clone(),
                Style::default().fg(sky).add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(ref sub) = self.subtitle {
            title_spans.push(Span::styled(" · ", Style::default().fg(muted)));
            title_spans.push(Span::styled(sub.clone(), Style::default().fg(muted)));
        }
        title_spans.push(Span::raw("   "));
        title_spans.push(Span::styled("↑/↓", Style::default().fg(sky)));
        title_spans.push(Span::styled(" scroll · ", Style::default().fg(muted)));
        title_spans.push(Span::styled("Esc ", Style::default().fg(sky)));
        title_spans.push(Span::styled("close", Style::default().fg(muted)));
        let title_line = Line::from(title_spans);

        let footer_spans: Vec<Span<'static>> = if let Some(ref hint) = self.footer_hint {
            vec![
                Span::raw(" "),
                Span::styled(
                    hint.clone(),
                    Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                ),
                Span::raw(" "),
            ]
        } else {
            vec![Span::raw(" ")]
        };

        let block = Block::default()
            .title(title_line)
            .title_bottom(Line::from(footer_spans))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(frame_color));

        // Build rendered lines from sections. Each row becomes 1+ lines.
        let mut lines: Vec<Line<'static>> = Vec::new();
        let key_col = self.compute_key_col();

        let last_section_idx = self.sections.len().saturating_sub(1);
        for (i, section) in self.sections.iter().enumerate() {
            if let Some(ref h) = section.heading {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        h.clone(),
                        Style::default().fg(sky).add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            for row in &section.rows {
                match row {
                    InfoRow::KeyValue { key, value } => {
                        let pad = key_col.saturating_sub(key.chars().count());
                        lines.push(Line::from(vec![
                            Span::raw("    "),
                            Span::styled(key.clone(), Style::default().fg(muted)),
                            Span::raw(" ".repeat(pad + 2)),
                            Span::styled(value.clone(), Style::default()),
                        ]));
                    }
                    InfoRow::Status {
                        kind,
                        label,
                        detail,
                    } => {
                        let glyph_color = match kind {
                            StatusKind::Ok => emerald,
                            StatusKind::Warn => coral,
                            StatusKind::Fail => red,
                            StatusKind::Info => sky,
                        };
                        let pad = key_col.saturating_sub(label.chars().count());
                        let mut spans = vec![
                            Span::raw("    "),
                            Span::styled(
                                kind.glyph(),
                                Style::default()
                                    .fg(glyph_color)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(" "),
                            Span::styled(label.clone(), Style::default()),
                        ];
                        if let Some(d) = detail {
                            spans.push(Span::raw(" ".repeat(pad + 2)));
                            spans.push(Span::styled(d.clone(), Style::default().fg(muted)));
                        }
                        lines.push(Line::from(spans));
                    }
                    InfoRow::Bullet { primary, secondary } => {
                        lines.push(Line::from(vec![
                            Span::raw("    · "),
                            Span::styled(primary.clone(), Style::default()),
                        ]));
                        if let Some(s) = secondary {
                            lines.push(Line::from(vec![
                                Span::raw("      "),
                                Span::styled(s.clone(), Style::default().fg(muted)),
                            ]));
                        }
                    }
                    InfoRow::InlineList { items } => {
                        let joined = items.join(", ");
                        lines.push(Line::from(vec![
                            Span::raw("    "),
                            Span::styled(joined, Style::default().fg(muted)),
                        ]));
                    }
                    InfoRow::Plain(text) => {
                        lines.push(Line::from(vec![
                            Span::raw("    "),
                            Span::styled(text.clone(), Style::default()),
                        ]));
                    }
                    InfoRow::Spacer => {
                        lines.push(Line::from(""));
                    }
                }
            }
            if i < last_section_idx {
                lines.push(Line::from(""));
            }
        }
        // One blank line at top + bottom for breathing room.
        let mut framed: Vec<Line<'static>> = Vec::with_capacity(lines.len() + 2);
        framed.push(Line::from(""));
        framed.extend(lines);
        framed.push(Line::from(""));

        let body = Paragraph::new(framed)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll_offset, 0));
        frame.render_widget(body, panel);
    }

    /// Width of the longest key in any KeyValue row, capped to keep the
    /// gutter sensible. Used to align values into a single column.
    fn compute_key_col(&self) -> usize {
        let mut max = 0usize;
        for s in &self.sections {
            for r in &s.rows {
                if let InfoRow::KeyValue { key, .. } | InfoRow::Status { label: key, .. } = r {
                    let n = key.chars().count();
                    if n > max {
                        max = n;
                    }
                }
            }
        }
        max.min(28).max(10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_col_caps_at_28() {
        let p = InfoPanel::new("t").section(
            InfoSection::unheaded().key_value("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "v"),
        );
        assert_eq!(p.compute_key_col(), 28);
    }

    #[test]
    fn key_col_floor_10() {
        let p = InfoPanel::new("t").section(InfoSection::unheaded().key_value("ab", "v"));
        assert_eq!(p.compute_key_col(), 10);
    }

    #[test]
    fn scroll_clamped_at_zero() {
        let mut p = InfoPanel::new("t");
        p.scroll_up();
        assert_eq!(p.scroll_offset, 0);
    }

    #[test]
    fn page_down_advances_ten() {
        let mut p = InfoPanel::new("t");
        p.page_down();
        assert_eq!(p.scroll_offset, 10);
    }

    #[test]
    fn builder_chains() {
        let p = InfoPanel::new("Channels")
            .with_subtitle("13 transports")
            .with_footer("Esc close")
            .section(InfoSection::new("Always available").status(StatusKind::Ok, "CLI / TUI"))
            .section(InfoSection::new("Configured").status_with(
                StatusKind::Ok,
                "Telegram",
                "polling",
            ));
        assert_eq!(p.sections.len(), 2);
        assert_eq!(p.title, "Channels");
        assert_eq!(p.subtitle.as_deref(), Some("13 transports"));
    }
}
