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
    /// Vertical scroll offset for the choose picker (independent of the
    /// log panel's `scroll`). Clamped at render time. Auto-adjusted by
    /// `choose_move_up/down` so the cursor stays in view.
    choose_scroll: usize,
    /// Height of the choose options viewport at last render — used by
    /// `choose_scroll_page_*` for full-page jumps.
    last_choose_viewport: u16,
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
            ProvisionEvent::OpenSkillInstallPicker { .. } => {
                // Intercepted upstream by the App's drain_events — should
                // not reach the overlay. Treat any leak as a no-op so the
                // overlay doesn't get confused by an unexpected variant.
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
        self.clamp_choose_scroll_to_cursor();
    }

    pub fn choose_move_down(&mut self) {
        if let Some(c) = self.choose.as_mut() {
            if c.cursor + 1 < c.options.len() {
                c.cursor += 1;
            }
        }
        self.clamp_choose_scroll_to_cursor();
    }

    /// Page up within the choose picker. Moves cursor by viewport height.
    pub fn choose_page_up(&mut self) {
        let step = self.last_choose_viewport.saturating_sub(1).max(1) as usize;
        if let Some(c) = self.choose.as_mut() {
            c.cursor = c.cursor.saturating_sub(step);
        }
        self.clamp_choose_scroll_to_cursor();
    }

    /// Page down within the choose picker. Moves cursor by viewport height,
    /// clamped to the last option.
    pub fn choose_page_down(&mut self) {
        let step = self.last_choose_viewport.saturating_sub(1).max(1) as usize;
        if let Some(c) = self.choose.as_mut() {
            let last = c.options.len().saturating_sub(1);
            c.cursor = (c.cursor + step).min(last);
        }
        self.clamp_choose_scroll_to_cursor();
    }

    /// Jump cursor to the first option.
    pub fn choose_home(&mut self) {
        if let Some(c) = self.choose.as_mut() {
            c.cursor = 0;
        }
        self.choose_scroll = 0;
    }

    /// Jump cursor to the last option.
    pub fn choose_end(&mut self) {
        if let Some(c) = self.choose.as_mut() {
            c.cursor = c.options.len().saturating_sub(1);
        }
        self.clamp_choose_scroll_to_cursor();
    }

    /// Adjust `choose_scroll` so the cursor row is visible. Called after
    /// any cursor movement. With one-row-per-option rendering, this keeps
    /// the cursor anywhere in the viewport [scroll, scroll + viewport).
    fn clamp_choose_scroll_to_cursor(&mut self) {
        let Some(c) = self.choose.as_ref() else {
            return;
        };
        let viewport = self.last_choose_viewport.max(1) as usize;
        // If cursor is above viewport, scroll up to it.
        if c.cursor < self.choose_scroll {
            self.choose_scroll = c.cursor;
        }
        // If cursor is at/below viewport bottom, scroll down so cursor is
        // the last visible row.
        let bottom = self.choose_scroll + viewport;
        if c.cursor >= bottom {
            self.choose_scroll = c.cursor.saturating_sub(viewport.saturating_sub(1));
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
        // Reset scroll so the next choose picker starts at the top.
        self.choose_scroll = 0;
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
        //
        // `choose_h` is capped to the space the layout can actually give
        // it — without the cap, a ClawHub picker with 20+ options would
        // claim 24+ rows of `Length`, the layout would clamp the chunk
        // to whatever the terminal allows, but `last_choose_viewport`
        // (set during render of the inner options area) would still
        // reflect the requested height. `clamp_choose_scroll_to_cursor`
        // then never engages, the user presses Down past the bottom row,
        // and the cursor moves into clipped-but-invisible rows. Bug-hunt
        // round 2 reported exactly this on "Select ClawHub skills".
        //
        // Reserve fixed chrome: brand(2) + top rule(1) + spacer(1) +
        // spacer-after-interactive(1, conditional) + status-log min(3) +
        // bottom rule(1) + footer(1) = 9–10 rows. Anything above that
        // goes to the interactive region.
        let prompt_block_h: u16 = if self.prompt.is_some() { 3 } else { 0 };
        let raw_choose_h: u16 = self
            .choose
            .as_ref()
            .map(|c| (c.options.len() as u16).saturating_add(4)) // section + headline + rule + spacer
            .unwrap_or(0);
        let fixed_chrome: u16 = 2 + 1 + 1 + 1 + 3 + 1 + 1; // see comment above
        let max_interactive_h = outer.height.saturating_sub(fixed_chrome);
        let max_choose_h = max_interactive_h.saturating_sub(prompt_block_h);
        let choose_h: u16 = raw_choose_h.min(max_choose_h);
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
        &mut self,
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

        // Section label with a right-aligned cursor position indicator
        // ("4/12 · 2 selected") so the user always knows where they are
        // in long lists. Without this, ClawHub's 20-skill picker felt
        // bottomless on first-time setup.
        let kind_label = if c.multi { "MULTI · CHOOSE" } else { "CHOOSE" };
        let total = c.options.len();
        let pos_text = if c.multi {
            format!("{}/{} · {} selected", c.cursor + 1, total, c.selected.len())
        } else {
            format!("{}/{}", c.cursor + 1, total)
        };
        let label_w = chunks[0].width as usize;
        let kind_w = kind_label.chars().count() + 3; // glyph + space
        let pos_w = pos_text.chars().count();
        let pad = label_w.saturating_sub(kind_w + pos_w);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("⌗  ", Style::default().fg(coral)),
                Span::styled(
                    kind_label,
                    Style::default()
                        .fg(coral)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ),
                Span::raw(" ".repeat(pad)),
                Span::styled(pos_text, Style::default().fg(muted)),
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

        // Cache the viewport height so cursor-keys can keep the cursor
        // visible (clamp_choose_scroll_to_cursor reads this).
        let opts_area = chunks[3];
        let viewport = opts_area.height as usize;
        self.last_choose_viewport = opts_area.height;

        // Clamp scroll one more time at render — handles initial render
        // before any cursor key has been pressed.
        let total = c.options.len();
        let max_scroll = total.saturating_sub(viewport.max(1));
        if self.choose_scroll > max_scroll {
            self.choose_scroll = max_scroll;
        }
        if c.cursor < self.choose_scroll {
            self.choose_scroll = c.cursor;
        }
        let bottom = self.choose_scroll + viewport;
        if c.cursor >= bottom && viewport > 0 {
            self.choose_scroll = c.cursor + 1 - viewport;
        }

        // Reserve fixed-width prefix: ` ▸  ▣  ` = 7 cols. Truncate the
        // label to the remaining width so each option is exactly one row
        // — no wrap, predictable alignment, scroll math stays correct.
        let prefix_cols: usize = 7;
        let label_width = (opts_area.width as usize).saturating_sub(prefix_cols);

        let visible_count = total.min(viewport);
        let mut option_lines: Vec<Line> = Vec::with_capacity(visible_count);
        for offset in 0..visible_count {
            let i = self.choose_scroll + offset;
            if i >= total {
                break;
            }
            let opt = &c.options[i];
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

            let label = truncate_to_width(opt, label_width);

            option_lines.push(Line::from(vec![
                Span::styled(format!(" {arrow}  "), arrow_style),
                Span::styled(format!("{marker}  "), marker_style),
                Span::styled(label, label_style),
            ]));
        }
        // No wrap — each option is exactly one row. Long labels are
        // already truncated above, so wrap would only hide bugs.
        f.render_widget(Paragraph::new(option_lines), opts_area);
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
                Span::styled("PgUp/PgDn ", Style::default().fg(sky)),
                Span::styled("page    ", Style::default().fg(muted)),
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

/// Truncate `s` to fit within `max_cols` display columns, appending an
/// ellipsis when shortened. Counts unicode characters (not bytes), but
/// treats every char as one column — fine for the Latin-heavy skill /
/// channel labels we render today; revisit if we ever pick up east-Asian
/// option text.
fn truncate_to_width(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_cols {
        return s.to_string();
    }
    let take = max_cols.saturating_sub(1);
    let truncated: String = s.chars().take(take).collect();
    format!("{truncated}…")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::ProvisionEvent;

    fn open_choose(opts: &[&str], multi: bool) -> SetupOverlayState {
        let mut s = SetupOverlayState::new("test");
        s.handle_event(ProvisionEvent::Choose {
            id: "test".into(),
            label: "Pick one".into(),
            options: opts.iter().map(|x| (*x).to_string()).collect(),
            multi,
        });
        s
    }

    #[test]
    fn truncate_to_width_passes_through_short_strings() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("hi", 2), "hi");
    }

    #[test]
    fn truncate_to_width_appends_ellipsis_when_shortened() {
        assert_eq!(truncate_to_width("abcdefgh", 5), "abcd…");
    }

    #[test]
    fn truncate_to_width_handles_zero_width() {
        assert_eq!(truncate_to_width("anything", 0), "");
    }

    #[test]
    fn choose_move_down_advances_cursor() {
        let mut s = open_choose(&["a", "b", "c"], false);
        s.choose_move_down();
        s.choose_move_down();
        assert_eq!(s.active_choose().unwrap().cursor, 2);
    }

    #[test]
    fn choose_move_down_clamps_at_last_option() {
        let mut s = open_choose(&["a", "b"], false);
        s.choose_move_down();
        s.choose_move_down();
        s.choose_move_down();
        assert_eq!(s.active_choose().unwrap().cursor, 1);
    }

    #[test]
    fn choose_end_jumps_to_last_option() {
        let mut s = open_choose(&["a", "b", "c", "d", "e"], true);
        s.choose_end();
        assert_eq!(s.active_choose().unwrap().cursor, 4);
    }

    #[test]
    fn choose_home_jumps_to_first_and_resets_scroll() {
        let mut s = open_choose(&["a", "b", "c"], false);
        s.choose_scroll = 5;
        s.choose_home();
        assert_eq!(s.active_choose().unwrap().cursor, 0);
        assert_eq!(s.choose_scroll, 0);
    }

    #[test]
    fn choose_page_down_jumps_by_viewport_height() {
        let mut s = open_choose(&["a", "b", "c", "d", "e", "f", "g"], false);
        s.last_choose_viewport = 3;
        s.choose_page_down();
        // Page down moves cursor by viewport-1 = 2 rows.
        assert_eq!(s.active_choose().unwrap().cursor, 2);
    }

    #[test]
    fn choose_scroll_clamps_when_cursor_moves_below_viewport() {
        let mut s = open_choose(&["a", "b", "c", "d", "e", "f", "g", "h"], false);
        s.last_choose_viewport = 3;
        // Move down 5 times; cursor=5, viewport=3 → scroll should be 3.
        for _ in 0..5 {
            s.choose_move_down();
        }
        assert_eq!(s.active_choose().unwrap().cursor, 5);
        assert_eq!(s.choose_scroll, 3);
    }

    #[test]
    fn submit_choose_resets_scroll() {
        let mut s = open_choose(&["a", "b", "c"], false);
        s.choose_scroll = 2;
        let _ = s.submit_choose();
        assert_eq!(s.choose_scroll, 0);
    }
}
