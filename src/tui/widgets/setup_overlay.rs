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
    /// `Some(reason)` when the provisioner ended via `Failed`, `None`
    /// for clean Done. Lets the wizard distinguish success from
    /// failure so it can halt vs auto-advance accordingly.
    pub failure_reason: Option<String>,
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
                self.failure_reason = Some(error);
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
        if area.height < 10 || area.width < 50 {
            return self.render_compact(f, area);
        }

        // ── Palette ──────────────────────────────────────────────
        let coral = Color::Rgb(255, 138, 101);
        let sky = Color::Rgb(94, 184, 255);
        let muted = Color::Rgb(107, 114, 128);
        let emerald = Color::Rgb(52, 211, 153);
        let dim = Color::Rgb(60, 70, 90);

        f.render_widget(Clear, area);

        // Breathing room around the entire overlay.
        let outer = Rect {
            x: area.x.saturating_add(2),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        // Compute the heights of the optional prompt + choose blocks
        // so the surrounding spacers can collapse cleanly when empty.
        let prompt_block_h: u16 = if self.prompt.is_some() { 3 } else { 0 };
        let choose_h: u16 = self
            .choose
            .as_ref()
            .map(|c| (c.options.len() as u16).saturating_add(4)) // section + headline + rule + spacer
            .unwrap_or(0);
        let interactive_h = prompt_block_h.saturating_add(choose_h);
        let interactive_spacer: u16 = if interactive_h > 0 { 1 } else { 0 };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),                  // 0: brand bar
                Constraint::Length(1),                  // 1: top rule
                Constraint::Length(1),                  // 2: spacer
                Constraint::Length(interactive_h),      // 3: prompt + choose region
                Constraint::Length(interactive_spacer), // 4: spacer
                Constraint::Min(3),                     // 5: status log (scrollable)
                Constraint::Length(1),                  // 6: bottom rule
                Constraint::Length(1),                  // 7: footer
            ])
            .split(outer);

        // ── Brand bar ────────────────────────────────────────────
        self.render_brand_bar(f, chunks[0], coral, sky, muted, dim);

        // ── Top rule ─────────────────────────────────────────────
        render_horizontal_rule(f, chunks[1], dim);

        // ── Interactive region (prompt + choose stacked) ─────────
        if interactive_h > 0 {
            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(prompt_block_h),
                    Constraint::Length(choose_h),
                ])
                .split(chunks[3]);

            if self.prompt.is_some() {
                self.render_prompt_block(f, parts[0], coral, sky, muted);
            }
            if self.choose.is_some() {
                self.render_choose_block(f, parts[1], coral, sky, muted, emerald, dim);
            }
        }

        // ── Status log + QR (scrollable) ─────────────────────────
        self.render_status_log(f, chunks[5], coral, sky, muted, emerald, dim);

        // ── Bottom rule ──────────────────────────────────────────
        render_horizontal_rule(f, chunks[6], dim);

        // ── Footer ───────────────────────────────────────────────
        self.render_footer(f, chunks[7], coral, sky, emerald, muted);
    }

    fn render_brand_bar(
        &self,
        f: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        muted: Color,
        dim: Color,
    ) {
        // The provisioner name is the trailing word of the title
        // (titles look like "Setup — provider"); fall back to the
        // full title if the format is unexpected.
        let prov_name = self
            .title
            .rsplit_once("— ")
            .map(|(_, n)| n.trim().to_string())
            .unwrap_or_else(|| self.title.clone());

        let status_word = if self.finished {
            if self.failure_reason.is_some() {
                "failed"
            } else {
                "complete"
            }
        } else if self.choose.is_some() {
            "awaiting choice"
        } else if self.prompt.is_some() {
            "awaiting input"
        } else {
            "in progress"
        };

        let line1 = Line::from(vec![
            Span::styled("◆  ", Style::default().fg(coral)),
            Span::styled(
                "RANTAICLAW",
                Style::default().fg(coral).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled("·", Style::default().fg(dim)),
            Span::styled(
                "  setup",
                Style::default().fg(sky).add_modifier(Modifier::ITALIC),
            ),
        ]);
        let line2 = Line::from(vec![
            Span::styled(
                format!("{:<width$}", format!("module · {prov_name}"), width = 28),
                Style::default().fg(muted),
            ),
            Span::styled("·", Style::default().fg(dim)),
            Span::styled(
                format!("  {status_word}"),
                Style::default().fg(muted).add_modifier(Modifier::ITALIC),
            ),
        ]);
        f.render_widget(Paragraph::new(vec![line1, line2]), area);
    }

    fn render_prompt_block(
        &self,
        f: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        muted: Color,
    ) {
        let Some(p) = self.prompt.as_ref() else {
            return;
        };
        let masked = if p.secret {
            "•".repeat(self.input.len())
        } else {
            self.input.clone()
        };
        let label_span = Span::styled(
            format!("  {}  ", p.label),
            Style::default().fg(sky).add_modifier(Modifier::BOLD),
        );
        let value_span = Span::styled(masked.clone(), Style::default().fg(coral));
        let cursor_span = Span::styled("▎", Style::default().fg(coral));

        let line = if self.input.is_empty() {
            let placeholder = p
                .default
                .as_ref()
                .map(|d| {
                    Span::styled(
                        format!("{d} "),
                        Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                    )
                })
                .unwrap_or_else(|| Span::raw(""));
            Line::from(vec![label_span, placeholder, cursor_span])
        } else {
            Line::from(vec![label_span, value_span, cursor_span])
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(coral));
        f.render_widget(Paragraph::new(line).block(block), area);
    }

    fn render_choose_block(
        &self,
        f: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        muted: Color,
        emerald: Color,
        dim: Color,
    ) {
        let Some(c) = self.choose.as_ref() else {
            return;
        };

        // Layout inside the choose region: section label, headline,
        // accent rule, options.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // section label
                Constraint::Length(1), // headline
                Constraint::Length(1), // accent rule
                Constraint::Min(1),    // options
            ])
            .split(area);

        let kind_label = if c.multi { "MULTI · CHOOSE" } else { "CHOOSE" };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("⌗  ", Style::default().fg(coral)),
                Span::styled(
                    kind_label,
                    Style::default()
                        .fg(coral)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ),
            ])),
            chunks[0],
        );

        // Headline = the choose label, sentence-case-friendly with a
        // period appended if missing.
        let headline = if c.label.ends_with('.') || c.label.ends_with('?') {
            c.label.clone()
        } else {
            format!("{}.", c.label)
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                headline,
                Style::default().fg(sky).add_modifier(Modifier::BOLD),
            ))),
            chunks[1],
        );

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─── ⌐",
                Style::default().fg(coral),
            ))),
            chunks[2],
        );

        // Options — column-aligned with refined glyphs.
        let mut option_lines: Vec<Line> = Vec::with_capacity(c.options.len());
        for (i, opt) in c.options.iter().enumerate() {
            let is_cursor = i == c.cursor;
            let is_checked = c.selected.contains(&i);

            let arrow = if is_cursor { "▸" } else { " " };
            let arrow_style = if is_cursor {
                Style::default().fg(coral).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let marker = if c.multi {
                if is_checked {
                    "▣"
                } else {
                    "□"
                }
            } else if is_cursor {
                "◆"
            } else {
                "◇"
            };
            let marker_style = if is_checked {
                Style::default().fg(emerald)
            } else if is_cursor {
                Style::default().fg(coral).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(dim)
            };

            let label_style = if is_cursor {
                Style::default().fg(sky).add_modifier(Modifier::BOLD)
            } else if is_checked {
                Style::default().fg(sky)
            } else {
                Style::default().fg(muted)
            };

            option_lines.push(Line::from(vec![
                Span::styled(format!(" {arrow}  "), arrow_style),
                Span::styled(format!("{marker}  "), marker_style),
                Span::styled(opt.clone(), label_style),
            ]));
        }
        f.render_widget(
            Paragraph::new(option_lines).wrap(Wrap { trim: false }),
            chunks[3],
        );
    }

    fn render_status_log(
        &mut self,
        f: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        muted: Color,
        emerald: Color,
        dim: Color,
    ) {
        // Layout: section label / spacer / scroll content.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(area);

        // Section label with right-aligned scroll position.
        // Calculate scroll bounds first so the label can show "row N/M".
        let mut content_lines: Vec<Line> = self
            .log
            .iter()
            .map(|l| style_log_line(l, coral, sky, emerald))
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

        self.last_content_height = content_lines.len() as u16;
        self.last_viewport_height = chunks[2].height;
        let max_scroll = self
            .last_content_height
            .saturating_sub(self.last_viewport_height);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }

        let mut header_spans = vec![
            Span::styled("⌗  ", Style::default().fg(dim)),
            Span::styled(
                "RECENT",
                Style::default().fg(muted).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
        ];
        if max_scroll > 0 {
            header_spans.push(Span::styled(
                format!(
                    "row {}/{}",
                    self.scroll.saturating_add(1),
                    self.last_content_height,
                ),
                Style::default().fg(dim),
            ));
            let at_top = self.scroll == 0;
            let at_bot = self.scroll >= max_scroll;
            header_spans.push(Span::styled(
                format!(
                    "  {}{}",
                    if !at_top { "↑" } else { " " },
                    if !at_bot { "↓" } else { " " },
                ),
                Style::default().fg(if at_top && at_bot { dim } else { sky }),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(header_spans)), chunks[0]);

        // Spacer — leave blank.
        let _ = chunks[1];

        // Content — left rail in dim, padded body.
        if content_lines.is_empty() {
            let placeholder = Paragraph::new(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "(no messages yet — provisioner is starting…)",
                    Style::default().fg(dim).add_modifier(Modifier::ITALIC),
                ),
            ]));
            f.render_widget(placeholder, chunks[2]);
        } else {
            let block = Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(dim));
            let content = Paragraph::new(content_lines)
                .block(block)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0));
            f.render_widget(content, chunks[2]);
        }
    }

    fn render_footer(
        &self,
        f: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        emerald: Color,
        muted: Color,
    ) {
        let spans: Vec<Span> = if self.choose.is_some() {
            let multi = self.choose.as_ref().is_some_and(|c| c.multi);
            let mut v = vec![
                Span::styled("↑/↓ ", Style::default().fg(sky)),
                Span::styled("navigate    ", Style::default().fg(muted)),
            ];
            if multi {
                v.push(Span::styled("Space ", Style::default().fg(sky)));
                v.push(Span::styled("toggle    ", Style::default().fg(muted)));
            }
            v.push(Span::styled(
                "↩ Enter ",
                Style::default().fg(emerald).add_modifier(Modifier::BOLD),
            ));
            v.push(Span::styled("confirm    ", Style::default().fg(muted)));
            v.push(Span::styled(
                "⎋ Esc ",
                Style::default().fg(coral).add_modifier(Modifier::BOLD),
            ));
            v.push(Span::styled("cancel", Style::default().fg(muted)));
            v
        } else if self.prompt.is_some() {
            vec![
                Span::styled("type ", Style::default().fg(sky)),
                Span::styled("to enter value    ", Style::default().fg(muted)),
                Span::styled(
                    "↩ Enter ",
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                ),
                Span::styled("submit    ", Style::default().fg(muted)),
                Span::styled(
                    "⎋ Esc ",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled("cancel", Style::default().fg(muted)),
            ]
        } else {
            vec![
                Span::styled("↑/↓ ", Style::default().fg(sky)),
                Span::styled("scroll    ", Style::default().fg(muted)),
                Span::styled("PgUp/PgDn ", Style::default().fg(sky)),
                Span::styled("page    ", Style::default().fg(muted)),
                Span::styled("Home/End ", Style::default().fg(sky)),
                Span::styled("jump    ", Style::default().fg(muted)),
                Span::styled(
                    "⎋ Esc ",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled("close", Style::default().fg(muted)),
            ]
        };
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Compact fallback for narrow / short terminals.
    fn render_compact(&self, f: &mut Frame, area: Rect) {
        let coral = Color::Rgb(255, 138, 101);
        let muted = Color::Rgb(107, 114, 128);
        let frame_color = Color::Rgb(40, 70, 140);

        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(frame_color))
            .title(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(
                    self.title.clone(),
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ", Style::default()),
            ]));
        let lines: Vec<Line> = self
            .log
            .iter()
            .map(|l| Line::from(l.as_str()))
            .collect();
        let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
        f.render_widget(para, area);
        let _ = muted;
    }
}

fn style_log_line<'a>(l: &'a str, coral: Color, sky: Color, emerald: Color) -> Line<'a> {
    if let Some(rest) = l.strip_prefix("· ") {
        Line::from(vec![
            Span::styled("· ", Style::default().fg(sky)),
            Span::raw(rest),
        ])
    } else if let Some(rest) = l.strip_prefix("✓ ") {
        Line::from(vec![
            Span::styled(
                "✓ ",
                Style::default().fg(emerald).add_modifier(Modifier::BOLD),
            ),
            Span::raw(rest),
        ])
    } else if let Some(rest) = l.strip_prefix("✗ ") {
        Line::from(vec![
            Span::styled(
                "✗ ",
                Style::default().fg(coral).add_modifier(Modifier::BOLD),
            ),
            Span::raw(rest),
        ])
    } else if let Some(rest) = l.strip_prefix("! ") {
        Line::from(vec![
            Span::styled("! ", Style::default().fg(Color::Yellow)),
            Span::raw(rest),
        ])
    } else {
        Line::from(l)
    }
}

fn render_horizontal_rule(f: &mut Frame, area: Rect, dim: Color) {
    let w = area.width as usize;
    if w == 0 {
        return;
    }
    let line: String = std::iter::repeat('─').take(w).collect();
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(line, Style::default().fg(dim)))),
        area,
    );
}

fn render_qr_block(payload: &str) -> String {
    use qrcode::{render::unicode, QrCode};
    match QrCode::new(payload.as_bytes()) {
        Ok(qr) => qr.render::<unicode::Dense1x2>().build(),
        Err(_) => format!("[QR render failed; raw payload: {payload}]"),
    }
}
