//! Setup overlay widget — renders ProvisionEvent stream and captures user input.

use crate::onboard::provision::{ProvisionEvent, Severity};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

#[derive(Debug, Clone)]
pub struct ActivePrompt {
    pub id: String,
    pub label: String,
    pub default: Option<String>,
    pub secret: bool,
}

#[derive(Debug, Clone)]
pub struct ActiveChoose {
    pub id: String,
    pub label: String,
    pub options: Vec<String>,
    pub multi: bool,
    pub cursor: usize,
    pub selected: Vec<usize>,
}

#[derive(Debug, Default)]
pub struct SetupOverlayState {
    pub title: String,
    log: Vec<String>,
    qr: Option<(String, String)>,
    prompt: Option<ActivePrompt>,
    choose: Option<ActiveChoose>,
    input: String,
    /// Vertical scroll offset (in rows). 0 = top. Clamped at render time
    /// to keep at least one visible row when content exceeds the viewport.
    scroll: u16,
    /// Total rendered-content height observed at last render. Used to
    /// clamp scroll-down so we can't scroll past the last visible line.
    last_content_height: u16,
    /// Last viewport height (frame area) observed at render. Used by
    /// scroll-clamping + page-size calculations.
    last_viewport_height: u16,
    /// True when the provisioner has emitted Done or Failed. Overlay
    /// stays open in this state so the user can read the final
    /// summary; Esc dismisses (handled by the app, not auto-close).
    pub finished: bool,
    /// Legacy alias so older code that reads `closed` keeps compiling.
    /// Will be removed in a follow-up.
    pub closed: bool,
}

impl SetupOverlayState {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Default::default()
        }
    }

    pub fn handle_event(&mut self, ev: ProvisionEvent) {
        match ev {
            ProvisionEvent::Message { severity, text } => {
                let prefix = match severity {
                    Severity::Info => "·",
                    Severity::Warn => "!",
                    Severity::Error => "✗",
                    Severity::Success => "✓",
                };
                self.log.push(format!("{prefix} {text}"));
            }
            ProvisionEvent::QrCode { payload, caption } => {
                self.qr = Some((render_qr_block(&payload), caption));
            }
            ProvisionEvent::Prompt {
                id,
                label,
                default,
                secret,
            } => {
                self.prompt = Some(ActivePrompt {
                    id,
                    label,
                    default,
                    secret,
                });
                self.input.clear();
            }
            ProvisionEvent::Choose {
                id,
                label,
                options,
                multi,
            } => {
                self.choose = Some(ActiveChoose {
                    id,
                    label,
                    options,
                    multi,
                    cursor: 0,
                    selected: Vec::new(),
                });
            }
            ProvisionEvent::Done { summary } => {
                self.log.push(format!("✓ {summary}"));
                self.log.push(String::new());
                self.log.push("All done. Press Esc to close.".to_string());
                self.finished = true;
            }
            ProvisionEvent::Failed { error } => {
                self.log.push(format!("✗ {error}"));
                self.log.push(String::new());
                self.log.push("Press Esc to close.".to_string());
                self.finished = true;
            }
        }
    }

    pub fn log_lines(&self) -> &[String] {
        &self.log
    }

    pub fn active_prompt(&self) -> Option<&ActivePrompt> {
        self.prompt.as_ref()
    }

    pub fn active_choose(&self) -> Option<&ActiveChoose> {
        self.choose.as_ref()
    }

    pub fn choose_move_up(&mut self) {
        if let Some(c) = self.choose.as_mut() {
            if c.cursor > 0 {
                c.cursor -= 1;
            }
        }
    }

    pub fn choose_move_down(&mut self) {
        if let Some(c) = self.choose.as_mut() {
            if c.cursor + 1 < c.options.len() {
                c.cursor += 1;
            }
        }
    }

    pub fn choose_toggle(&mut self) {
        if let Some(c) = self.choose.as_mut() {
            if !c.multi {
                return;
            }
            let pos = c.cursor;
            if let Some(idx) = c.selected.iter().position(|&i| i == pos) {
                c.selected.remove(idx);
            } else {
                c.selected.push(pos);
                c.selected.sort_unstable();
            }
        }
    }

    pub fn submit_choose(&mut self) -> Option<(String, Vec<usize>)> {
        let c = self.choose.take()?;
        let sel = if c.multi { c.selected } else { vec![c.cursor] };
        Some((c.id, sel))
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
    }

    pub fn pop_char(&mut self) {
        self.input.pop();
    }

    /// Scroll up by one row (towards the top of the content). No-op at top.
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    /// Scroll down by one row, clamped so we don't scroll past content.
    pub fn scroll_down(&mut self) {
        let max = self
            .last_content_height
            .saturating_sub(self.last_viewport_height);
        if self.scroll < max {
            self.scroll += 1;
        }
    }

    /// Page up — moves by viewport height minus one row of overlap.
    pub fn scroll_page_up(&mut self) {
        let step = self.last_viewport_height.saturating_sub(1).max(1);
        self.scroll = self.scroll.saturating_sub(step);
    }

    /// Page down — moves by viewport height minus one row of overlap.
    pub fn scroll_page_down(&mut self) {
        let step = self.last_viewport_height.saturating_sub(1).max(1);
        let max = self
            .last_content_height
            .saturating_sub(self.last_viewport_height);
        self.scroll = (self.scroll + step).min(max);
    }

    /// Jump to top.
    pub fn scroll_home(&mut self) {
        self.scroll = 0;
    }

    /// Jump to bottom.
    pub fn scroll_end(&mut self) {
        self.scroll = self
            .last_content_height
            .saturating_sub(self.last_viewport_height);
    }

    pub fn submit_prompt(&mut self) -> Option<(String, String)> {
        let p = self.prompt.take()?;
        let value = if self.input.is_empty() {
            p.default.clone().unwrap_or_default()
        } else {
            std::mem::take(&mut self.input)
        };
        Some((p.id, value))
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        if area.height < 6 || area.width < 30 {
            return;
        }

        let sky = Color::Rgb(94, 184, 255);
        let muted = Color::Rgb(107, 114, 128);
        let frame_color = Color::Rgb(40, 70, 140);
        let coral = Color::Rgb(255, 138, 101);
        let emerald = Color::Rgb(52, 211, 153);

        f.render_widget(Clear, area);

        // Outer 1-col / 1-row margin so the overlay doesn't kiss the
        // terminal edges — same breathing room as `/sessions`.
        let outer = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        // Reserve fixed rows for prompt and choose blocks if they're
        // active; remaining space is the scrollable content area.
        let prompt_h: u16 = if self.prompt.is_some() { 3 } else { 0 };
        let choose_h: u16 = self
            .choose
            .as_ref()
            .map(|c| (c.options.len() as u16).saturating_add(2)) // label + options
            .unwrap_or(0);
        let prompt_spacer: u16 = if prompt_h > 0 { 1 } else { 0 };
        let choose_spacer: u16 = if choose_h > 0 { 1 } else { 0 };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),             // 0: title row
                Constraint::Length(1),             // 1: spacer
                Constraint::Length(prompt_h),      // 2: prompt input box
                Constraint::Length(prompt_spacer), // 3: spacer
                Constraint::Length(choose_h),      // 4: choose options
                Constraint::Length(choose_spacer), // 5: spacer
                Constraint::Min(3),                // 6: scrollable content
                Constraint::Length(1),             // 7: footer
            ])
            .split(outer);

        // Build scrollable content (status log + QR caption + QR rows).
        let mut content_lines: Vec<Line> = self
            .log
            .iter()
            .map(|l| {
                // Color the prefix glyphs by severity for a quick visual scan.
                if let Some(rest) = l.strip_prefix("· ") {
                    Line::from(vec![
                        Span::styled("· ", Style::default().fg(sky)),
                        Span::raw(rest.to_string()),
                    ])
                } else if let Some(rest) = l.strip_prefix("✓ ") {
                    Line::from(vec![
                        Span::styled(
                            "✓ ",
                            Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(rest.to_string()),
                    ])
                } else if let Some(rest) = l.strip_prefix("✗ ") {
                    Line::from(vec![
                        Span::styled(
                            "✗ ",
                            Style::default().fg(coral).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(rest.to_string()),
                    ])
                } else if let Some(rest) = l.strip_prefix("! ") {
                    Line::from(vec![
                        Span::styled("! ", Style::default().fg(Color::Yellow)),
                        Span::raw(rest.to_string()),
                    ])
                } else {
                    Line::from(l.as_str())
                }
            })
            .collect();

        if let Some((qr, cap)) = &self.qr {
            content_lines.push(Line::from(""));
            content_lines.push(Line::from(Span::styled(
                cap.clone(),
                Style::default().fg(coral).add_modifier(Modifier::BOLD),
            )));
            content_lines.push(Line::from(""));
            for qrl in qr.lines() {
                content_lines.push(Line::from(Span::styled(
                    qrl.to_string(),
                    Style::default().fg(sky),
                )));
            }
        }

        // Capture heights for scroll-bound calculations. Note we record
        // the LINE count (not visual rows after wrapping) — it's a tight
        // estimate for our content (QR rows are fixed-width, logs are
        // typically one line each).
        self.last_content_height = content_lines.len() as u16;
        self.last_viewport_height = chunks[6].height;

        // Title row — name on the left, scroll status on the right when
        // content overflows.
        let inner_h = self.last_viewport_height;
        let max_scroll = self.last_content_height.saturating_sub(inner_h);
        let mut title_spans: Vec<Span> = vec![Span::styled(
            self.title.clone(),
            Style::default().fg(coral).add_modifier(Modifier::BOLD),
        )];
        if max_scroll > 0 {
            title_spans.push(Span::styled(
                format!(
                    "   row {}/{}",
                    self.scroll.saturating_add(1),
                    self.last_content_height,
                ),
                Style::default().fg(muted),
            ));
            // Position bar arrows
            let at_top = self.scroll == 0;
            let at_bot = self.scroll >= max_scroll;
            title_spans.push(Span::styled(
                format!(
                    "  · {}{}",
                    if !at_top { "↑" } else { " " },
                    if !at_bot { "↓" } else { " " },
                ),
                Style::default().fg(if at_top && at_bot { muted } else { sky }),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(title_spans)), chunks[0]);

        // Prompt input box — bordered, focus-coloured, search-style.
        if let Some(p) = self.prompt.as_ref() {
            let masked = if p.secret {
                "•".repeat(self.input.len())
            } else {
                self.input.clone()
            };
            let label = Span::styled(
                format!("  {} ", p.label),
                Style::default().fg(sky).add_modifier(Modifier::BOLD),
            );
            let value = Span::styled(masked, Style::default().fg(coral));
            let cursor = Span::styled("▎", Style::default().fg(coral));
            let placeholder = if self.input.is_empty() {
                p.default
                    .as_ref()
                    .map(|d| {
                        Span::styled(
                            format!("{} ", d),
                            Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                        )
                    })
                    .unwrap_or_else(|| Span::raw(""))
            } else {
                Span::raw("")
            };
            let line = if self.input.is_empty() {
                Line::from(vec![label, placeholder, cursor])
            } else {
                Line::from(vec![label, value, cursor])
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(coral));
            f.render_widget(Paragraph::new(line).block(block), chunks[2]);
        }

        // Choose options — list-picker-style rows with cursor highlight.
        if let Some(c) = self.choose.as_ref() {
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                c.label.clone(),
                Style::default().fg(sky).add_modifier(Modifier::BOLD),
            ))];
            for (i, opt) in c.options.iter().enumerate() {
                let is_cursor = i == c.cursor;
                let is_checked = c.selected.contains(&i);
                let arrow = if is_cursor { "▸ " } else { "  " };
                let marker = if c.multi {
                    if is_checked {
                        "[x] "
                    } else {
                        "[ ] "
                    }
                } else if is_cursor {
                    "(•) "
                } else {
                    "( ) "
                };
                let style = if is_cursor {
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD)
                } else if is_checked {
                    Style::default().fg(sky)
                } else {
                    Style::default().fg(muted)
                };
                lines.push(Line::from(vec![
                    Span::styled(arrow, style),
                    Span::styled(marker, style),
                    Span::styled(opt.clone(), style),
                ]));
            }
            f.render_widget(Paragraph::new(lines), chunks[4]);
        }

        // Re-clamp scroll in case content shrank.
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }

        // Scrollable content area.
        let content_block = Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(frame_color));
        let content = Paragraph::new(content_lines)
            .block(content_block)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        f.render_widget(content, chunks[6]);

        // Footer with hotkeys — context-dependent.
        let footer_spans: Vec<Span> = if self.choose.is_some() {
            let multi = self.choose.as_ref().is_some_and(|c| c.multi);
            let mut v = vec![
                Span::styled("↑/↓", Style::default().fg(sky)),
                Span::styled(" navigate", Style::default().fg(muted)),
            ];
            if multi {
                v.push(Span::styled(" · ", Style::default().fg(muted)));
                v.push(Span::styled("Space", Style::default().fg(sky)));
                v.push(Span::styled(" toggle", Style::default().fg(muted)));
            }
            v.push(Span::styled(" · ", Style::default().fg(muted)));
            v.push(Span::styled("Enter", Style::default().fg(sky)));
            v.push(Span::styled(" confirm · ", Style::default().fg(muted)));
            v.push(Span::styled("Esc", Style::default().fg(sky)));
            v.push(Span::styled(" cancel", Style::default().fg(muted)));
            v
        } else if self.prompt.is_some() {
            vec![
                Span::styled("type", Style::default().fg(sky)),
                Span::styled(" to enter value · ", Style::default().fg(muted)),
                Span::styled("Enter", Style::default().fg(sky)),
                Span::styled(" submit · ", Style::default().fg(muted)),
                Span::styled("Esc", Style::default().fg(sky)),
                Span::styled(" cancel", Style::default().fg(muted)),
            ]
        } else {
            vec![
                Span::styled("↑/↓", Style::default().fg(sky)),
                Span::styled(" scroll · ", Style::default().fg(muted)),
                Span::styled("PgUp/PgDn", Style::default().fg(sky)),
                Span::styled(" page · ", Style::default().fg(muted)),
                Span::styled("Home/End", Style::default().fg(sky)),
                Span::styled(" jump · ", Style::default().fg(muted)),
                Span::styled("Esc", Style::default().fg(sky)),
                Span::styled(" close", Style::default().fg(muted)),
            ]
        };
        f.render_widget(Paragraph::new(Line::from(footer_spans)), chunks[7]);
    }
}

fn render_qr_block(payload: &str) -> String {
    use qrcode::{render::unicode, QrCode};
    match QrCode::new(payload.as_bytes()) {
        Ok(qr) => qr.render::<unicode::Dense1x2>().build(),
        Err(_) => format!("[QR render failed; raw payload: {payload}]"),
    }
}
