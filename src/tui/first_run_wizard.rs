//! First-run wizard state machine + render.
//!
//! Visual direction: research-instrument minimalism. Asymmetric layout
//! with a persistent step rail on the left, generous negative space,
//! sharp coral/emerald accents on a frame_color/muted base. Sentence-case
//! display headings, small-caps section labels, Unicode glyph hierarchy.
//!
//! Phase flow:
//!   Welcome
//!     → RunningProvisioner ("provider")           required
//!     → RunningProvisioner ("approvals")          quick, skippable
//!     → RunningProvisioner ("persona")            quick, skippable
//!     → RunningProvisioner ("skills")             quick, skippable
//!     → PickChannels                              multi-select over channels
//!     → RunningProvisioner (each chosen channel)
//!     → PickIntegrations                          multi-select over mcp / web-search / memory
//!     → RunningProvisioner (each chosen integration)
//!     → Complete

use crate::profile::Profile;
use crate::tui::widgets::setup_overlay::ActiveChoose;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WizardPhase {
    Welcome,
    RunningProvisioner { name: String },
    PickChannels,
    PickIntegrations,
    Complete,
}

#[derive(Debug)]
pub struct FirstRunWizard {
    pub phase: WizardPhase,
    pub queue: Vec<String>,
    pub picker: Option<ActiveChoose>,
    pub picker_names: Vec<String>,
    /// First visible option row, and the option-area height the layout last
    /// granted. `ActiveChoose` is shared with `SetupOverlay`, but the scroll
    /// state lives on the owner, not the struct — the overlay has
    /// `choose_scroll`/`last_choose_viewport` and the wizard had neither, so
    /// its option list rendered from row 0 forever. `channel_options()`
    /// returns all 16 channel provisioners (18 lines) into a
    /// `Constraint::Min(4)` chunk: on a 24-row terminal most of them were
    /// invisible and the cursor walked into rows that had been clipped away.
    choose_scroll: usize,
    last_choose_viewport: u16,
    pub profile: Profile,
    /// Phase history for the back button. Each completed phase transition
    /// pushes the previous phase here; `back()` pops one and restores it.
    /// v0.6.4 covers the safe cases (PickChannels ↔ PickIntegrations and
    /// PickChannels → previous required provisioner). RunningProvisioner
    /// rewind is forward-only for now — the running task can't be
    /// surgically rewound without leaking partial state.
    pub history: Vec<WizardPhase>,
}

const REQUIRED_PROVISIONERS: &[&str] = &["provider", "approvals", "login", "persona", "skills"];
const INTEGRATION_OPTIONS: &[(&str, &str)] = &[
    ("mcp", "MCP servers (curated tool plugins)"),
    ("web-search", "Web search backend"),
    ("memory", "Memory backend (sqlite / postgres / markdown)"),
];

/// Abstract steps shown in the left rail. Stays fixed across the
/// session so the user has a stable map of where they are. Real
/// provisioner names map to one of these via `phase_to_rail_idx`.
const RAIL: &[(&str, &str)] = &[
    ("01", "Provider"),
    ("02", "Approvals"),
    ("03", "Login"),
    ("04", "Persona"),
    ("05", "Skills"),
    ("06", "Channels"),
    ("07", "Integrations"),
    ("08", "Complete"),
];

impl FirstRunWizard {
    pub fn new(profile: Profile) -> Self {
        let queue: Vec<String> = REQUIRED_PROVISIONERS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        Self {
            phase: WizardPhase::Welcome,
            queue,
            picker: None,
            picker_names: Vec::new(),
            choose_scroll: 0,
            last_choose_viewport: 0,
            profile,
            history: Vec::new(),
        }
    }

    /// Go back one step. Returns `true` if the phase changed, `false` if
    /// already at the earliest restorable point.
    ///
    /// When the popped history entry is another `RunningProvisioner`, the
    /// user wants to redo that section — we re-queue both the prior
    /// provisioner and (if currently running another one) the current one,
    /// so the redo replays cleanly and flow continues with what's left.
    /// Required-provisioner config writes are overwritten by the redo, which
    /// is exactly the desired effect for "previous page was wrong".
    ///
    /// When the popped entry is a Picker, restore it and clear the queue so
    /// the user's re-selection starts from a clean slate — otherwise stale
    /// items from the previous picker selection would replay before the new
    /// ones.
    pub fn back(&mut self) -> bool {
        while let Some(prev) = self.history.pop() {
            match prev {
                WizardPhase::RunningProvisioner { name: prior } => {
                    // Capture currently-running provisioner (if any) so it
                    // resumes after the redo.
                    let current_running = match &self.phase {
                        WizardPhase::RunningProvisioner { name } => Some(name.clone()),
                        _ => None,
                    };
                    if let Some(current) = current_running {
                        self.queue.insert(0, current);
                    }
                    self.queue.insert(0, prior);
                    // Welcome is the only sentinel that lets advance() pop
                    // from a non-empty queue without falling into the
                    // "queue empty, pick next phase" branch.
                    self.phase = WizardPhase::Welcome;
                    self.advance_to_next_in_queue_or_picker();
                    return true;
                }
                phase => {
                    if matches!(
                        phase,
                        WizardPhase::PickChannels | WizardPhase::PickIntegrations
                    ) {
                        // Clear leftover picker-selection queue so the
                        // user's re-selection isn't shadowed by old picks.
                        self.queue.clear();
                    }
                    self.phase = phase;
                    return true;
                }
            }
        }
        false
    }

    pub fn current_provisioner_name(&self) -> Option<&str> {
        match &self.phase {
            WizardPhase::RunningProvisioner { name } => Some(name.as_str()),
            _ => None,
        }
    }

    pub fn is_provisioner_running(&self) -> bool {
        matches!(self.phase, WizardPhase::RunningProvisioner { .. })
    }

    pub fn is_picker_active(&self) -> bool {
        matches!(
            self.phase,
            WizardPhase::PickChannels | WizardPhase::PickIntegrations
        )
    }

    pub fn start_provisioners(&mut self) {
        self.advance_to_next_in_queue_or_picker();
    }

    pub fn advance_to_next_in_queue_or_picker(&mut self) {
        // Record the prior phase for back-navigation before mutating.
        let prev = self.phase.clone();
        if let Some(next) = self.queue_pop_front() {
            self.phase = WizardPhase::RunningProvisioner { name: next };
        } else {
            match self.phase {
                WizardPhase::Welcome | WizardPhase::RunningProvisioner { .. } => {
                    if matches!(self.phase, WizardPhase::Welcome)
                        || matches!(
                            self.phase,
                            WizardPhase::RunningProvisioner { ref name }
                            if !is_channel_name(name) && !is_integration_name(name)
                        )
                    {
                        self.phase = WizardPhase::PickChannels;
                    } else if self.phase_provisioner_was_channel().unwrap_or(false) {
                        self.phase = WizardPhase::PickIntegrations;
                    } else {
                        self.phase = WizardPhase::Complete;
                    }
                }
                WizardPhase::PickChannels => {
                    self.phase = WizardPhase::PickIntegrations;
                }
                WizardPhase::PickIntegrations => {
                    self.phase = WizardPhase::Complete;
                }
                WizardPhase::Complete => {}
            }
        }
        if prev != self.phase {
            self.history.push(prev);
        }
    }

    fn phase_provisioner_was_channel(&self) -> Option<bool> {
        match &self.phase {
            WizardPhase::RunningProvisioner { name } => Some(is_channel_name(name)),
            _ => None,
        }
    }

    pub fn apply_picker_selection(&mut self) {
        let indices = self.picker_submit().unwrap_or_default();
        for i in &indices {
            if let Some(n) = self.picker_names.get(*i) {
                self.queue.push(n.clone());
            }
        }
        self.picker_names.clear();
        self.advance_to_next_in_queue_or_picker();
    }

    fn queue_pop_front(&mut self) -> Option<String> {
        if self.queue.is_empty() {
            None
        } else {
            Some(self.queue.remove(0))
        }
    }

    pub fn open_picker(&mut self, options: Vec<(String, String)>) {
        let n = options.len();
        self.open_picker_with_disabled(options, None, vec![false; n]);
    }

    /// Open the picker with a per-row disabled flag and an optional
    /// section heading. The wizard's channel step uses this to dim the
    /// locked rows under a `Under development` heading, matching the
    /// model `/setup channels` already ships.
    pub fn open_picker_with_disabled(
        &mut self,
        options: Vec<(String, String)>,
        heading: Option<String>,
        disabled: Vec<bool>,
    ) {
        self.picker_names = options.iter().map(|(name, _)| name.clone()).collect();
        let labels: Vec<String> = options.into_iter().map(|(_, label)| label).collect();
        // Pad a too-short `disabled` so callers that pass `vec![]` for a
        // fully-enabled picker do not need to repeat the option count.
        let mut disabled = disabled;
        if disabled.len() < labels.len() {
            disabled.resize(labels.len(), false);
        }
        self.choose_scroll = 0;
        self.picker = Some(ActiveChoose {
            id: match self.phase {
                WizardPhase::PickChannels => "channels".into(),
                WizardPhase::PickIntegrations => "integrations".into(),
                _ => "unknown".into(),
            },
            label: match self.phase {
                WizardPhase::PickChannels => "Add channels".into(),
                WizardPhase::PickIntegrations => "Set up integrations".into(),
                _ => "Choose".into(),
            },
            options: labels,
            heading,
            disabled,
            multi: true,
            cursor: 0,
            selected: Vec::new(),
        });
    }

    pub fn picker_move_up(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            let mut next = p.cursor;
            while next > 0 {
                next -= 1;
                if !p.disabled.get(next).copied().unwrap_or(false) {
                    break;
                }
            }
            if !p.disabled.get(next).copied().unwrap_or(false) {
                p.cursor = next;
            }
        }
        self.clamp_choose_scroll_to_cursor();
    }

    pub fn picker_move_down(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            let len = p.options.len();
            let mut next = p.cursor;
            while next + 1 < len {
                next += 1;
                if !p.disabled.get(next).copied().unwrap_or(false) {
                    break;
                }
            }
            if !p.disabled.get(next).copied().unwrap_or(false) {
                p.cursor = next;
            }
        }
        self.clamp_choose_scroll_to_cursor();
    }

    /// Keep the cursor inside the rendered option viewport.
    ///
    /// Mirrors `SetupOverlayState::clamp_choose_scroll_to_cursor`, whose
    /// comment records why the viewport must be the height the layout
    /// *granted*: a requested height leaves the clamp disengaged and the
    /// cursor walks into clipped rows. Bug-hunt round 2 reported exactly that
    /// against the overlay's ClawHub chooser; the wizard never got the fix.
    fn clamp_choose_scroll_to_cursor(&mut self) {
        let Some(p) = self.picker.as_ref() else {
            return;
        };
        let viewport = self.last_choose_viewport.max(1) as usize;
        if p.cursor < self.choose_scroll {
            self.choose_scroll = p.cursor;
        }
        let bottom = self.choose_scroll + viewport;
        if p.cursor >= bottom {
            self.choose_scroll = p.cursor.saturating_sub(viewport.saturating_sub(1));
        }
    }

    pub fn picker_toggle(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            // Toggle is inert on a disabled row. By construction the
            // cursor never lands on a disabled row via the move
            // handlers, but the check is cheap and a future caller
            // restoring cursor state directly must not be able to
            // silently schedule a locked channel.
            if p.disabled.get(p.cursor).copied().unwrap_or(false) {
                return;
            }
            let pos = p.cursor;
            if let Some(idx) = p.selected.iter().position(|&i| i == pos) {
                p.selected.remove(idx);
            } else {
                p.selected.push(pos);
                p.selected.sort_unstable();
            }
        }
    }

    pub fn picker_submit(&mut self) -> Option<Vec<usize>> {
        self.picker.take().map(|p| {
            // Defensive: drop any selected index whose row is disabled.
            // By construction the cursor never lands on a disabled row
            // and toggle is inert on disabled rows, but a programmatic
            // caller could have placed one in `selected`; this filter
            // keeps that from sneaking a locked channel past the gate.
            let disabled = &p.disabled;
            p.selected
                .into_iter()
                .filter(|&i| !disabled.get(i).copied().unwrap_or(false))
                .collect()
        })
    }

    /// Map current phase to an index into RAIL.
    fn rail_index(&self) -> Option<usize> {
        match &self.phase {
            WizardPhase::Welcome => None,
            WizardPhase::RunningProvisioner { name } => match name.as_str() {
                "provider" => Some(0),
                "approvals" => Some(1),
                "login" => Some(2),
                "persona" => Some(3),
                "skills" => Some(4),
                n if is_channel_name(n) => Some(5),
                n if is_integration_name(n) => Some(6),
                _ => None,
            },
            WizardPhase::PickChannels => Some(5),
            WizardPhase::PickIntegrations => Some(6),
            WizardPhase::Complete => Some(7),
        }
    }

    /// `&mut self` because the option viewport is recorded during render —
    /// the same shape as `ListPicker::render_fullscreen`. Only the render
    /// knows the height the layout granted, and the clamp needs it.
    pub fn render_fullscreen(&mut self, frame: &mut Frame, area: Rect) {
        if area.height < 16 || area.width < 64 {
            return self.render_compact(frame, area);
        }

        // ── Palette ──────────────────────────────────────────────
        let coral = Color::Rgb(255, 138, 101);
        let sky = Color::Rgb(94, 184, 255);
        let muted = Color::Rgb(107, 114, 128);
        let frame_color = Color::Rgb(40, 70, 140);
        let emerald = Color::Rgb(52, 211, 153);
        let dim = Color::Rgb(60, 70, 90);

        frame.render_widget(Clear, area);

        // Outer breathing room.
        let outer = Rect {
            x: area.x.saturating_add(2),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // 0: top brand bar
                Constraint::Length(1), // 1: separator rule
                Constraint::Length(1), // 2: spacer
                Constraint::Min(8),    // 3: body (rail + content)
                Constraint::Length(1), // 4: spacer
                Constraint::Length(1), // 5: separator rule
                Constraint::Length(1), // 6: footer
            ])
            .split(outer);

        // ── Top brand bar ─────────────────────────────────────────
        self.render_brand_bar(frame, chunks[0], coral, sky, muted, dim);

        // ── Top rule ──────────────────────────────────────────────
        render_horizontal_rule(frame, chunks[1], dim);

        // ── Body: rail + content ──────────────────────────────────
        let body_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(24), // rail
                Constraint::Length(2),  // gutter
                Constraint::Min(20),    // content
            ])
            .split(chunks[3]);

        self.render_rail(frame, body_chunks[0], coral, sky, emerald, muted, dim);
        self.render_content(
            frame,
            body_chunks[2],
            coral,
            sky,
            muted,
            frame_color,
            emerald,
            dim,
        );

        // ── Bottom rule ───────────────────────────────────────────
        render_horizontal_rule(frame, chunks[5], dim);

        // ── Footer ────────────────────────────────────────────────
        self.render_footer(frame, chunks[6], coral, sky, emerald, muted);
    }

    fn render_brand_bar(
        &self,
        frame: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        muted: Color,
        dim: Color,
    ) {
        let total = RAIL.len();
        let current_idx = self.rail_index();

        let bullet_glyph = "◆";
        let separator = "·";
        let phase_name = match &self.phase {
            WizardPhase::Welcome => "welcome",
            WizardPhase::RunningProvisioner { .. } => "in progress",
            WizardPhase::PickChannels => "select channels",
            WizardPhase::PickIntegrations => "select integrations",
            WizardPhase::Complete => "complete",
        };

        let step_text = match current_idx {
            Some(i) => format!("step {:02} ▸ {:02}", i + 1, total),
            None => format!("step 00 ▸ {total:02}"),
        };

        let line1 = Line::from(vec![
            Span::styled(format!("{bullet_glyph}  "), Style::default().fg(coral)),
            Span::styled(
                "RANTAICLAW",
                Style::default().fg(coral).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(separator, Style::default().fg(dim)),
            Span::styled(
                "  first-run setup",
                Style::default().fg(sky).add_modifier(Modifier::ITALIC),
            ),
        ]);
        let line2 = Line::from(vec![
            Span::styled(
                format!("{:<width$}", step_text, width = 28),
                Style::default().fg(muted),
            ),
            Span::styled(separator, Style::default().fg(dim)),
            Span::styled(
                format!("  {phase_name}"),
                Style::default().fg(muted).add_modifier(Modifier::ITALIC),
            ),
        ]);
        frame.render_widget(Paragraph::new(vec![line1, line2]), area);
    }

    fn render_rail(
        &self,
        frame: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        emerald: Color,
        muted: Color,
        dim: Color,
    ) {
        let cur = self.rail_index();
        let mut lines: Vec<Line> = Vec::new();

        // Section label.
        lines.push(Line::from(Span::styled(
            "  ROUTE  ",
            Style::default().fg(muted).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));

        for (i, (num, label)) in RAIL.iter().enumerate() {
            let state = match cur {
                Some(idx) if i < idx => RailState::Done,
                Some(idx) if i == idx => RailState::Current,
                _ => RailState::Pending,
            };

            // Connector line above each row except the first — gives
            // the rail a continuous spine.
            if i > 0 {
                lines.push(Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(
                        "│",
                        Style::default().fg(match state {
                            RailState::Done => emerald,
                            _ => dim,
                        }),
                    ),
                ]));
            }

            let (glyph, glyph_style, num_style, label_style) = match state {
                RailState::Done => (
                    "●",
                    Style::default().fg(emerald),
                    Style::default().fg(muted),
                    Style::default().fg(muted),
                ),
                RailState::Current => (
                    "◆",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                    Style::default().fg(coral),
                    Style::default().fg(sky).add_modifier(Modifier::BOLD),
                ),
                RailState::Pending => (
                    "○",
                    Style::default().fg(dim),
                    Style::default().fg(dim),
                    Style::default().fg(muted),
                ),
            };

            let arrow = if matches!(state, RailState::Current) {
                Span::styled(
                    " ▸ ",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled("   ", Style::default())
            };

            lines.push(Line::from(vec![
                arrow,
                Span::styled(glyph, glyph_style),
                Span::styled("  ", Style::default()),
                Span::styled(format!("{num}  "), num_style),
                Span::styled(label.to_string(), label_style),
            ]));
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    /// `&mut self`: dispatches to `render_picker`, which records the option
    /// viewport height the layout granted.
    fn render_content(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        muted: Color,
        frame_color: Color,
        emerald: Color,
        dim: Color,
    ) {
        match self.phase {
            WizardPhase::Welcome => {
                self.render_welcome(frame, area, coral, sky, muted, frame_color, emerald, dim);
            }
            WizardPhase::RunningProvisioner { .. } => {
                self.render_loading(frame, area, sky, muted, dim);
            }
            WizardPhase::PickChannels | WizardPhase::PickIntegrations => {
                self.render_picker(frame, area, coral, sky, muted, frame_color, emerald, dim);
            }
            WizardPhase::Complete => {
                self.render_complete(frame, area, coral, sky, muted, frame_color, emerald, dim);
            }
        }
    }

    fn render_welcome(
        &self,
        frame: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        muted: Color,
        _frame_color: Color,
        emerald: Color,
        dim: Color,
    ) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // section label
                Constraint::Length(1), // spacer
                Constraint::Length(2), // huge headline
                Constraint::Length(1), // sub-rule
                Constraint::Length(1), // spacer
                Constraint::Length(1), // subhead
                Constraint::Length(1), // spacer
                Constraint::Min(7),    // bullet body
                Constraint::Length(1), // spacer
                Constraint::Length(1), // hint
            ])
            .split(area);

        // Section label (small caps via Unicode caps).
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("⌗  ", Style::default().fg(coral)),
                Span::styled(
                    "FIRST · RUN",
                    Style::default()
                        .fg(coral)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ),
            ])),
            chunks[0],
        );

        // Display headline — sentence-case, two lines for vertical weight.
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Welcome.",
                    Style::default().fg(sky).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "Let's wire up your agent.",
                    Style::default().fg(muted),
                )),
            ]),
            chunks[2],
        );

        // Sub-rule under headline (short accent rule, not full width).
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─── ⌐",
                Style::default().fg(coral),
            ))),
            chunks[3],
        );

        // Subhead.
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Five required steps,", Style::default().fg(sky)),
                Span::styled("  two optional pickers,", Style::default().fg(muted)),
                Span::styled(
                    "  one polished agent.",
                    Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                ),
            ])),
            chunks[5],
        );

        // Body bullets — two-column, left side = key, right side = label.
        let bullet = |num: &str, key: &str, desc: &str, accent: Color| {
            Line::from(vec![
                Span::styled(format!(" {num}  "), Style::default().fg(dim)),
                Span::styled(
                    format!("{key:<14}"),
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(desc.to_string(), Style::default().fg(muted)),
            ])
        };
        let body = vec![
            bullet("01", "Provider", "model + key", coral),
            bullet("02", "Approvals", "autonomy tier", coral),
            bullet("03", "Login", "console username + password", coral),
            bullet("04", "Persona", "agent name & template", coral),
            bullet("05", "Skills", "bundled + ClawHub", coral),
            bullet("06", "Channels", "telegram, discord, whatsapp, …", emerald),
            bullet("07", "Integrations", "mcp, web-search, memory", emerald),
            bullet("08", "Complete", "ship it", sky),
        ];
        frame.render_widget(Paragraph::new(body), chunks[7]);

        // Hint line.
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Press ", Style::default().fg(muted)),
                Span::styled(
                    "Enter",
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" to begin · ", Style::default().fg(muted)),
                Span::styled(
                    "Esc",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " to cancel · resume later via /setup full",
                    Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                ),
            ])),
            chunks[9],
        );
    }

    fn render_loading(&self, frame: &mut Frame, area: Rect, sky: Color, muted: Color, _dim: Color) {
        // Brief placeholder shown between provisioners while the next
        // overlay is being spawned. The active overlay covers the
        // full screen most of the time; this only flashes briefly.
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  loading next step…",
                Style::default()
                    .fg(sky)
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC),
            )),
            Line::from(Span::styled(
                "  (the provisioner overlay will take over)",
                Style::default().fg(muted),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// `&mut self` for the same reason as `render_fullscreen`: this is where
    /// the option viewport height becomes known.
    fn render_picker(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        muted: Color,
        _frame_color: Color,
        emerald: Color,
        dim: Color,
    ) {
        let Some(p) = &self.picker else {
            return self.render_loading(frame, area, sky, muted, dim);
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // section label
                Constraint::Length(1), // spacer
                Constraint::Length(1), // headline
                Constraint::Length(1), // accent rule
                Constraint::Length(1), // spacer
                Constraint::Length(1), // subhead
                Constraint::Length(1), // spacer
                Constraint::Min(4),    // option list
                Constraint::Length(1), // spacer
                Constraint::Length(1), // hint
            ])
            .split(area);

        let (section, headline, subhead) = match self.phase {
            WizardPhase::PickChannels => (
                "STEP · CHANNELS",
                "Add channels.",
                "Pick the platforms you want this agent to be reachable on.",
            ),
            WizardPhase::PickIntegrations => (
                "STEP · INTEGRATIONS",
                "Set up integrations.",
                "Optional capability layers — each ships with safe defaults.",
            ),
            _ => ("STEP", "—", ""),
        };

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("⌗  ", Style::default().fg(coral)),
                Span::styled(
                    section,
                    Style::default()
                        .fg(coral)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ),
            ])),
            chunks[0],
        );

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                headline,
                Style::default().fg(sky).add_modifier(Modifier::BOLD),
            ))),
            chunks[2],
        );

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─── ⌐",
                Style::default().fg(coral),
            ))),
            chunks[3],
        );

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                subhead,
                Style::default().fg(muted),
            ))),
            chunks[5],
        );

        // Option rows.
        let mut option_lines: Vec<Line> = Vec::new();
        let heading = p.heading.as_deref();
        // The seam between usable and locked is the first index whose
        // `disabled` flag is true; rows past that point are the locked
        // section. The heading is rendered exactly once, right above
        // the first locked row, and only when locked rows exist.
        let usable_count = (0..p.options.len())
            .position(|i| p.disabled.get(i).copied().unwrap_or(false))
            .unwrap_or(p.options.len());
        for (i, opt) in p.options.iter().enumerate() {
            // Section heading at the seam. The literal lives here too so
            // any future heading variant on `ActiveChoose` renders
            // without a second branch.
            if heading.is_some() && i == usable_count {
                option_lines.push(Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(
                        "Under development",
                        Style::default()
                            .fg(muted)
                            .add_modifier(Modifier::ITALIC | Modifier::BOLD),
                    ),
                ]));
                option_lines.push(Line::from(""));
            }
            let is_disabled = p.disabled.get(i).copied().unwrap_or(false);
            // Cursor arrow is suppressed on a disabled row even if the
            // cursor were placed there directly — the row is inert.
            let is_cursor = i == p.cursor && !is_disabled;
            let is_checked = !is_disabled && p.selected.contains(&i);
            let arrow = if is_cursor { "▸" } else { " " };
            let arrow_style = if is_cursor {
                Style::default().fg(coral).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let marker = if is_disabled {
                " "
            } else if is_checked {
                "▣"
            } else {
                "□"
            };
            let marker_style = if is_checked && !is_disabled {
                Style::default().fg(emerald)
            } else if is_cursor {
                Style::default().fg(coral)
            } else {
                Style::default().fg(dim)
            };
            let label_style = if is_disabled {
                Style::default().fg(dim)
            } else if is_cursor {
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
        if p.selected.is_empty() {
            option_lines.push(Line::from(""));
            option_lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(
                    "Nothing selected — Enter to skip this step.",
                    Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                ),
            ]));
        } else {
            option_lines.push(Line::from(""));
            option_lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(
                    format!("{} selected", p.selected.len()),
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  · Enter to confirm", Style::default().fg(muted)),
            ]));
        }
        // Record the height the layout actually granted (chunks[7] is
        // Constraint::Min(4), so it is whatever survives the chrome) and scroll
        // to it. Without this the Paragraph rendered from row 0 forever.
        self.last_choose_viewport = chunks[7].height;
        self.clamp_choose_scroll_to_cursor();
        frame.render_widget(
            Paragraph::new(option_lines)
                .wrap(Wrap { trim: false })
                .scroll((u16::try_from(self.choose_scroll).unwrap_or(u16::MAX), 0)),
            chunks[7],
        );

        // Hint.
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("↑/↓ ", Style::default().fg(sky)),
                Span::styled("navigate · ", Style::default().fg(muted)),
                Span::styled("Space ", Style::default().fg(sky)),
                Span::styled("toggle · ", Style::default().fg(muted)),
                Span::styled(
                    "Enter",
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" confirm · ", Style::default().fg(muted)),
                Span::styled(
                    "Esc",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" skip", Style::default().fg(muted)),
            ])),
            chunks[9],
        );
    }

    fn render_complete(
        &self,
        frame: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        muted: Color,
        _frame_color: Color,
        emerald: Color,
        dim: Color,
    ) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // section label
                Constraint::Length(1), // spacer
                Constraint::Length(2), // headline (2 lines)
                Constraint::Length(1), // accent rule
                Constraint::Length(1), // spacer
                Constraint::Length(1), // subhead
                Constraint::Length(1), // spacer
                Constraint::Min(5),    // next-steps body
                Constraint::Length(1), // spacer
                Constraint::Length(1), // hint
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("✦  ", Style::default().fg(emerald)),
                Span::styled(
                    "STATUS · OPERATIONAL",
                    Style::default()
                        .fg(emerald)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                ),
            ])),
            chunks[0],
        );

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "All wired up.",
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "Your workspace is ready.",
                    Style::default().fg(muted),
                )),
            ]),
            chunks[2],
        );

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─── ⌐",
                Style::default().fg(emerald),
            ))),
            chunks[3],
        );

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Next moves",
                Style::default().fg(coral).add_modifier(Modifier::BOLD),
            ))),
            chunks[5],
        );

        let row = |key: &str, desc: &str| {
            Line::from(vec![
                Span::styled(" ▸  ", Style::default().fg(emerald)),
                Span::styled(
                    format!("{key:<28}"),
                    Style::default().fg(sky).add_modifier(Modifier::BOLD),
                ),
                Span::styled(desc.to_string(), Style::default().fg(muted)),
            ])
        };
        frame.render_widget(
            Paragraph::new(vec![
                row("rantaiclaw", "open the chat TUI"),
                row("/setup", "interactive picker inside the TUI"),
                row(
                    "rantaiclaw setup <topic>",
                    "reconfigure a single topic from a shell",
                ),
                Line::from(""),
                Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(
                        "config saved · agent reloaded · ready",
                        Style::default().fg(dim).add_modifier(Modifier::ITALIC),
                    ),
                ]),
            ]),
            chunks[7],
        );

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Press ", Style::default().fg(muted)),
                Span::styled(
                    "Enter",
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" to enter chat", Style::default().fg(muted)),
            ])),
            chunks[9],
        );
    }

    fn render_footer(
        &self,
        frame: &mut Frame,
        area: Rect,
        coral: Color,
        sky: Color,
        emerald: Color,
        muted: Color,
    ) {
        let spans: Vec<Span> = match self.phase {
            WizardPhase::Welcome => vec![
                Span::styled(
                    "↩ Enter ",
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                ),
                Span::styled("begin    ", Style::default().fg(muted)),
                Span::styled(
                    "⎋ Esc ",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled("exit", Style::default().fg(muted)),
            ],
            WizardPhase::RunningProvisioner { .. } => vec![
                Span::styled(
                    "▣  provisioner overlay active — interact above    ",
                    Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                ),
                Span::styled(
                    "Ctrl+B ",
                    Style::default().fg(sky).add_modifier(Modifier::BOLD),
                ),
                Span::styled("back", Style::default().fg(muted)),
            ],
            WizardPhase::PickChannels | WizardPhase::PickIntegrations => vec![
                Span::styled("↑/↓ ", Style::default().fg(sky)),
                Span::styled("navigate    ", Style::default().fg(muted)),
                Span::styled("Space ", Style::default().fg(sky)),
                Span::styled("toggle    ", Style::default().fg(muted)),
                Span::styled(
                    "↩ Enter ",
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                ),
                Span::styled("confirm    ", Style::default().fg(muted)),
                Span::styled(
                    "Ctrl+B ",
                    Style::default().fg(sky).add_modifier(Modifier::BOLD),
                ),
                Span::styled("back    ", Style::default().fg(muted)),
                Span::styled(
                    "⎋ Esc ",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled("skip", Style::default().fg(muted)),
            ],
            WizardPhase::Complete => vec![
                Span::styled(
                    "↩ Enter ",
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                ),
                Span::styled("close    ", Style::default().fg(muted)),
                Span::styled(
                    "⎋ Esc ",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled("close", Style::default().fg(muted)),
            ],
        };
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Compact fallback for narrow / short terminals — the structured
    /// layout collapses gracefully to a single bordered card.
    fn render_compact(&self, frame: &mut Frame, area: Rect) {
        let coral = Color::Rgb(255, 138, 101);
        let sky = Color::Rgb(94, 184, 255);
        let muted = Color::Rgb(107, 114, 128);
        let frame_color = Color::Rgb(40, 70, 140);

        frame.render_widget(Clear, area);
        let title = match &self.phase {
            WizardPhase::Welcome => "First-Run Setup".to_string(),
            WizardPhase::RunningProvisioner { name } => format!("Setup · {name}"),
            WizardPhase::PickChannels => "Add channels".to_string(),
            WizardPhase::PickIntegrations => "Set up integrations".to_string(),
            WizardPhase::Complete => "Setup Complete".to_string(),
        };
        let body = match self.phase {
            WizardPhase::Welcome => "Press Enter to begin.\nEsc to exit.",
            WizardPhase::RunningProvisioner { .. } => "Provisioner overlay active. Ctrl+B back.",
            WizardPhase::PickChannels => {
                "↑/↓ Space toggle · Enter confirm · Ctrl+B back · Esc skip"
            }
            WizardPhase::PickIntegrations => {
                "↑/↓ Space toggle · Enter confirm · Ctrl+B back · Esc skip"
            }
            WizardPhase::Complete => "Configuration saved. Press Enter to close.",
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(frame_color))
            .title(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(
                    title,
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ", Style::default()),
            ]));
        let para = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(body, Style::default().fg(sky))),
            Line::from(""),
            Line::from(Span::styled(
                "(Window too small for full layout — resize for the full wizard.)",
                Style::default().fg(muted).add_modifier(Modifier::ITALIC),
            )),
        ])
        .block(block)
        .wrap(Wrap { trim: false });
        frame.render_widget(para, area);
    }
}

#[derive(Debug, Clone, Copy)]
enum RailState {
    Done,
    Current,
    Pending,
}

fn render_horizontal_rule(frame: &mut Frame, area: Rect, dim: Color) {
    let w = area.width as usize;
    if w == 0 {
        return;
    }
    let line: String = std::iter::repeat_n('─', w).collect();
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(line, Style::default().fg(dim)))),
        area,
    );
}

fn is_channel_name(name: &str) -> bool {
    // Derive from the provisioner registry — the same source `channel_options`
    // uses — so the two never drift. A hardcoded array meant the next channel
    // added without editing it here looped the user back to the picker with no
    // way forward.
    use crate::onboard::provision::{provisioner_for, ProvisionerCategory};
    provisioner_for(name).is_some_and(|p| p.category() == ProvisionerCategory::Channel)
}

fn is_integration_name(name: &str) -> bool {
    integration_options().iter().any(|(k, _)| k == name)
}

/// Integration option list — `(name, description)` pairs.
///
/// `knowledge` is only present when the `kb` feature is compiled (its
/// provisioner is kb-gated); offering it without `kb` would surface an
/// option whose provisioner is missing.
pub fn integration_options() -> Vec<(String, String)> {
    #[allow(unused_mut)]
    let mut opts: Vec<(String, String)> = INTEGRATION_OPTIONS
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    #[cfg(feature = "kb")]
    opts.push((
        "knowledge".to_string(),
        "Knowledge Base (document search + optional OCR)".to_string(),
    ));
    opts
}

/// Channel option list — pulled live from the provisioner registry,
/// filtered by `ProvisionerCategory::Channel`.
pub fn channel_options() -> Vec<(String, String)> {
    use crate::onboard::provision::{available, provisioner_for, ProvisionerCategory};
    available()
        .into_iter()
        .filter_map(|(name, desc)| {
            let p = provisioner_for(name)?;
            if p.category() == ProvisionerCategory::Channel {
                Some((name.to_string(), desc.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// The same derivation `channel_picker_entries` uses for `/setup channels`,
/// shaped to feed `FirstRunWizard::open_picker_with_disabled`: usable
/// channels first, then — only if any locked channels exist — a heading
/// `Under development`, then the locked channels. The `disabled` flag
/// matches the order of `options`; `disabled[i] == true` means the row
/// at `options[i]` is locked.
///
/// The catalog has 6 usable channel provisioners (telegram, discord,
/// slack, whatsapp, whatsapp_web, lark) and ≥10 locked ones today, so
/// the heading branch is always taken. The `locked.is_empty()`
/// short-circuit is kept so a future catalog trim drops the heading
/// instead of rendering an empty section.
pub fn channel_options_full() -> (Vec<(String, String)>, Option<String>, Vec<bool>) {
    use crate::onboard::provision::{available, provisioner_for, ProvisionerCategory};

    let mut usable = Vec::new();
    let mut locked = Vec::new();
    for (name, desc) in available() {
        let Some(p) = provisioner_for(name) else {
            continue;
        };
        if p.category() != ProvisionerCategory::Channel {
            continue;
        }
        let catalog_key = crate::channels::catalog_key_for_provisioner(name);
        let is_usable = crate::channels::channel_is_usable(catalog_key);
        let row = (name.to_string(), desc.to_string());
        if is_usable {
            usable.push(row);
        } else {
            locked.push(row);
        }
    }
    if locked.is_empty() {
        // No locked rows → no heading, all rows enabled.
        let n = usable.len();
        return (usable, None, vec![false; n]);
    }
    let total = usable.len() + locked.len();
    let mut disabled = vec![false; total];
    for (i, _) in locked.iter().enumerate() {
        disabled[usable.len() + i] = true;
    }
    let mut options = usable;
    options.extend(locked);
    (options, Some("Under development".to_string()), disabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_profile() -> Profile {
        Profile {
            name: "test".into(),
            root: PathBuf::from("/tmp/rantaiclaw-test"),
        }
    }

    #[cfg(feature = "kb")]
    #[test]
    fn integration_options_includes_knowledge_with_kb() {
        assert!(integration_options().iter().any(|(k, _)| k == "knowledge"));
        assert!(is_integration_name("knowledge"));
    }

    #[test]
    fn channel_name_matches_registry() {
        use crate::onboard::provision::available;
        let channel_names: std::collections::HashSet<String> =
            channel_options().into_iter().map(|(n, _)| n).collect();
        // `is_channel_name` and `channel_options` must agree over the whole
        // registry — a hardcoded array used to drift on the next channel added.
        for (name, _) in available() {
            assert_eq!(
                is_channel_name(name),
                channel_names.contains(name),
                "is_channel_name disagreed with channel_options for {name}"
            );
        }
        assert!(!is_channel_name("not-a-real-provisioner"));
    }

    #[test]
    fn back_from_welcome_returns_false() {
        let mut w = FirstRunWizard::new(test_profile());
        assert!(!w.back(), "no history at Welcome — back must be a no-op");
    }

    #[test]
    fn back_from_picker_restores_prior_running_provisioner() {
        let mut w = FirstRunWizard::new(test_profile());
        // Simulate Welcome → Running{provider}
        w.start_provisioners();
        assert!(matches!(w.phase, WizardPhase::RunningProvisioner { .. }));
        // Manually advance through required provisioners
        for _ in 0..REQUIRED_PROVISIONERS.len() {
            w.advance_to_next_in_queue_or_picker();
        }
        // After all required provisioners, we should land at PickChannels
        assert!(matches!(w.phase, WizardPhase::PickChannels));
        // Back from picker → previous phase was Running{skills} (last required) →
        // re-queue it so user redoes it.
        assert!(w.back());
        assert!(matches!(w.phase, WizardPhase::RunningProvisioner { .. }));
    }

    #[test]
    fn back_from_running_re_queues_prior_running() {
        let mut w = FirstRunWizard::new(test_profile());
        w.start_provisioners();
        // Now in Running{provider}; advance to Running{approvals}.
        w.advance_to_next_in_queue_or_picker();
        let in_approvals = matches!(
            &w.phase,
            WizardPhase::RunningProvisioner { name } if name == "approvals"
        );
        assert!(in_approvals, "expected to be in approvals provisioner");
        // Back: should re-queue provider (prior) AND approvals (current),
        // landing back in Running{provider}.
        assert!(w.back());
        let in_provider = matches!(
            &w.phase,
            WizardPhase::RunningProvisioner { name } if name == "provider"
        );
        assert!(in_provider, "back should land us back in provider");
        // Approvals should be the next queued item so the flow continues.
        assert_eq!(w.queue.first().map(|s| s.as_str()), Some("approvals"));
    }

    #[test]
    fn back_from_picker_clears_stale_queue_items() {
        let mut w = FirstRunWizard::new(test_profile());
        // Synthetic state: at PickChannels with a stale queue from a prior pick.
        w.phase = WizardPhase::PickChannels;
        w.queue = vec!["telegram".into(), "matrix".into()];
        w.history.push(WizardPhase::PickChannels);
        // Pop the picker phase from history → restore + clear queue.
        assert!(w.back());
        assert!(matches!(w.phase, WizardPhase::PickChannels));
        assert!(
            w.queue.is_empty(),
            "stale picker selections must be cleared on restore"
        );
    }

    // ── locked channel rows are dimmed and skipped ──────

    /// `channel_options_full` mirrors `/setup channels`: usable rows first,
    /// then — only if any locked channels exist — a heading `Under
    /// development`, then the locked rows. The disabled flag matches row
    /// order. Catalog has 6 usable and ≥10 locked channel provisioners
    /// today; that shape is what `is_channel_name` /
    /// `channel_options` already agree on (see `channel_name_matches_registry`).
    #[test]
    fn channel_options_full_orders_usable_before_locked_with_heading() {
        let (options, heading, disabled) = channel_options_full();
        assert!(!options.is_empty(), "channel_options_full returned no rows");
        assert_eq!(
            disabled.len(),
            options.len(),
            "disabled[] length must match options"
        );

        // First row is a known usable channel provisioner. `telegram` is
        // the first Channel-category entry in the registry; using a literal
        // (not a recomputation) so a registry reorder that swaps telegram
        // and discord still trips the assertion.
        assert_eq!(
            options[0].0, "telegram",
            "first row should be the first usable channel"
        );

        // Locate the seam between usable and locked. If every channel were
        // usable the helper short-circuits (heading == None, disabled all
        // false); today that branch is unreachable because the catalog
        // has locked entries, but the seam-pos / count assertions cover
        // both shapes when the catalog is later trimmed.
        let locked_start = disabled.iter().position(|&d| d).unwrap_or(options.len());
        let locked_count = options.len() - locked_start;
        assert!(
            locked_count >= 1,
            "catalog today has ≥1 locked channel; if this fails the catalog was trimmed — \
             drop the locked-rows section of this test"
        );
        assert!(
            locked_start >= 6,
            "at least the 6 supported channels must precede the locked section"
        );

        // Every row flagged disabled is unusable per the catalog.
        for i in locked_start..options.len() {
            assert!(
                disabled[i],
                "row {i} ({}) in the locked section must be flagged disabled",
                options[i].0
            );
            let catalog_key = crate::channels::catalog_key_for_provisioner(&options[i].0);
            assert!(
                !crate::channels::channel_is_usable(catalog_key),
                "row {i} ({}) flagged disabled but catalog says usable",
                options[i].0
            );
        }

        // Every row in the usable section is flagged enabled.
        for i in 0..locked_start {
            assert!(
                !disabled[i],
                "row {i} ({}) in the usable section must be enabled",
                options[i].0
            );
        }

        // Heading is set when at least one locked channel exists. With
        // the current catalog that branch is always taken; an empty
        // locked set would yield `None`, and that path is covered by the
        // helper's short-circuit (see also the comment on the test
        // `channel_options_full_has_no_heading_when_all_channels_usable`,
        // which the catalog cannot satisfy today and is intentionally
        // omitted so the test set does not depend on a temporary
        // catalog mutation).
        assert_eq!(
            heading.as_deref(),
            Some("Under development"),
            "heading must be Some(\"Under development\") when locked rows exist"
        );
    }

    /// Build a wizard at PickChannels whose picker has `n` rows with the
    /// supplied per-row `disabled` flags. The cursor starts at 0; the
    /// picker is set up via `open_picker_with_disabled` so the test
    /// exercises the same construction the call site uses.
    fn wizard_with_options_disabled(
        n: usize,
        heading: Option<String>,
        disabled: Vec<bool>,
    ) -> FirstRunWizard {
        let mut w = FirstRunWizard::new(test_profile());
        w.phase = WizardPhase::PickChannels;
        let options = (0..n)
            .map(|i| (format!("ch{i}"), format!("Channel {i}")))
            .collect();
        w.open_picker_with_disabled(options, heading, disabled);
        w
    }

    /// Pressing `Down` repeatedly on a picker with a trailing block of
    /// disabled rows must never let the cursor land on a disabled row.
    /// The cursor should walk through every enabled row and stop on the
    /// last one. Without the cursor-skip logic the cursor would land on
    /// the first disabled row at index 6 and stay there.
    #[test]
    fn picker_cursor_never_lands_on_disabled_row_walking_down() {
        let mut w = wizard_with_options_disabled(10, None, {
            let mut v = vec![false; 6];
            v.extend(vec![true; 4]);
            v
        });
        for _ in 0..20 {
            w.picker_move_down();
            let p = w.picker.as_ref().unwrap();
            assert!(
                !p.disabled[p.cursor],
                "cursor at {} landed on disabled row (disabled = {:?})",
                p.cursor, p.disabled
            );
        }
        // The cursor must have stopped at the last enabled row.
        assert_eq!(w.picker.as_ref().unwrap().cursor, 5);
    }

    /// Pressing `Up` from a position below the disabled block must skip
    /// the disabled rows on its way up.
    #[test]
    fn picker_cursor_never_lands_on_disabled_row_walking_up() {
        let mut w = wizard_with_options_disabled(10, None, {
            let mut v = vec![false; 6];
            v.extend(vec![true; 4]);
            v
        });
        // Move the cursor past the disabled block first.
        w.picker.as_mut().unwrap().cursor = 9;
        for _ in 0..20 {
            w.picker_move_up();
            let p = w.picker.as_ref().unwrap();
            assert!(
                !p.disabled[p.cursor],
                "cursor at {} landed on disabled row walking up",
                p.cursor
            );
        }
        assert_eq!(w.picker.as_ref().unwrap().cursor, 0);
    }

    /// Space (toggle) on a disabled row must be inert. The cursor can
    /// only reach a disabled row via direct mutation — by construction
    /// `picker_move_*` keeps it off — but the toggle handler must still
    /// refuse to flip `selected` for such a row, so a future caller that
    /// lands the cursor on a disabled row (e.g. via a programmatic
    /// restore) cannot silently schedule a locked channel.
    #[test]
    fn picker_toggle_is_inert_on_disabled_row() {
        let mut w = wizard_with_options_disabled(10, None, {
            let mut v = vec![false; 6];
            v.extend(vec![true; 4]);
            v
        });
        // Place the cursor on a disabled row directly.
        w.picker.as_mut().unwrap().cursor = 7;
        w.picker_toggle();
        assert!(
            w.picker.as_ref().unwrap().selected.is_empty(),
            "toggle on a disabled row must not add it to `selected`"
        );
        // And the same on a different disabled index, to confirm the
        // check is positional, not a one-off cursor==7 special case.
        w.picker.as_mut().unwrap().cursor = 9;
        w.picker_toggle();
        assert!(
            w.picker.as_ref().unwrap().selected.is_empty(),
            "toggle on disabled index 9 must also be inert"
        );
    }

    /// A defensive filter on `picker_submit`: even if a disabled index
    /// were somehow placed in `selected`, `picker_submit` must drop it.
    /// The toggle / cursor-skip tests cover the construction paths; this
    /// one is the safety net.
    #[test]
    fn picker_submit_filters_disabled_indices() {
        let mut w = wizard_with_options_disabled(10, None, {
            let mut v = vec![false; 6];
            v.extend(vec![true; 4]);
            v
        });
        // Pre-seed `selected` with a mix of usable and disabled indices.
        w.picker.as_mut().unwrap().selected = vec![0, 2, 7, 9];
        let submitted = w
            .picker_submit()
            .expect("picker_submit returns Some while a picker exists");
        assert_eq!(
            submitted,
            vec![0, 2],
            "picker_submit must drop any selected index whose row is disabled"
        );
    }

    /// Sanity: `open_picker_with_disabled` propagates the `heading` and
    /// `disabled` fields the call site passes in.
    #[test]
    fn open_picker_with_disabled_propagates_heading_and_disabled() {
        let mut w = FirstRunWizard::new(test_profile());
        w.phase = WizardPhase::PickChannels;
        w.open_picker_with_disabled(
            vec![
                ("telegram".into(), "Telegram".into()),
                ("matrix".into(), "Matrix".into()),
            ],
            Some("Under development".into()),
            vec![false, true],
        );
        let p = w.picker.as_ref().expect("picker should be open");
        assert_eq!(p.heading.as_deref(), Some("Under development"));
        assert_eq!(p.disabled, vec![false, true]);
    }

    // ── render-level checks: heading and dim only when expected ──────

    /// Render the wizard's picker pane to a `TestBackend` and hand back
    /// the joined row text. The pane is sized to fit the full-screen
    /// layout (the compact fallback collapses the option list, so a
    /// smaller area would not exercise `render_picker`).
    fn render_pane_text(w: &mut FirstRunWizard, width: u16, height: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        use ratatui::Terminal;
        let mut term =
            Terminal::new(TestBackend::new(width, height)).expect("TestBackend allocation");
        term.draw(|f| w.render_fullscreen(f, Rect::new(0, 0, width, height)))
            .expect("draw");
        let buf = term.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `render_picker` must insert the `Under development` heading line
    /// when the picker has both a heading and at least one disabled
    /// row. This is the visual counterpart of the helper's logic: the
    /// helper says "here is a heading" and the renderer says "yes, draw
    /// it once at the seam". A mutation that drops the `heading.is_some()`
    /// guard in the renderer would still draw the heading even when the
    /// caller passed `None`, leaving a stray heading over a picker that
    /// has no locked section.
    #[test]
    fn render_picker_inserts_heading_when_set_and_disabled_present() {
        let mut w = wizard_with_options_disabled(8, Some("Under development".to_string()), {
            let mut v = vec![false; 4];
            v.extend(vec![true; 4]);
            v
        });
        // 16 rows is the minimum for the full-screen layout, and we
        // need enough vertical space for the option list to render the
        // heading above the locked rows.
        let text = render_pane_text(&mut w, 80, 24);
        assert!(
            text.contains("Under development"),
            "rendered pane must contain the heading text when both heading \
             and disabled rows are set; pane was:\n{text}"
        );
    }

    /// The renderer must NOT draw the heading when no heading was set,
    /// even if `disabled` is non-empty. The wizard passes `heading = None`
    /// for non-channel pickers (e.g. integrations); an unconditional
    /// heading line would leak into those panes. A mutation that removes
    /// the `heading.is_some()` guard would make this test fail.
    #[test]
    fn render_picker_does_not_insert_heading_when_none() {
        let mut w = wizard_with_options_disabled(8, None, {
            // Disabled rows present but no heading: the renderer
            // should still suppress the heading line.
            let mut v = vec![false; 4];
            v.extend(vec![true; 4]);
            v
        });
        let text = render_pane_text(&mut w, 80, 24);
        assert!(
            !text.contains("Under development"),
            "rendered pane must NOT contain the heading text when heading \
             is None; pane was:\n{text}"
        );
    }

    /// The renderer must dim (visually de-emphasize) the disabled rows.
    /// Concretely, a disabled label must not pick up the `sky` cursor
    /// or the `emerald` checked marker. We assert the simpler invariant
    /// that the disabled label never carries the coral cursor arrow —
    /// the renderer emits `▸` only on the cursor row, and the cursor
    /// arrow style on a disabled row is suppressed.
    ///
    /// We construct a picker with a single disabled row and place the
    /// cursor directly on it (bypassing the move handlers). If the
    /// renderer accidentally applied the cursor arrow on a disabled row
    /// the test would see `▸` next to the disabled label.
    #[test]
    fn render_picker_suppresses_cursor_arrow_on_disabled_row() {
        let mut w = wizard_with_options_disabled(4, None, vec![false, true, false, false]);
        // Place cursor on the disabled row.
        w.picker.as_mut().unwrap().cursor = 1;
        let text = render_pane_text(&mut w, 80, 24);
        // Find the row that contains the disabled label "Channel 1".
        let label_row = text
            .lines()
            .find(|l| l.contains("Channel 1"))
            .expect("disabled label must render somewhere in the pane");
        assert!(
            !label_row.contains('▸'),
            "cursor arrow must be suppressed on a disabled row; row was: {label_row:?}"
        );
    }
}

#[cfg(test)]
mod choose_scroll_tests {
    use super::*;
    use crate::profile::Profile;
    use std::path::PathBuf;

    fn test_profile() -> Profile {
        Profile {
            name: "test".into(),
            root: PathBuf::from("/tmp/rantaiclaw-test"),
        }
    }

    fn wizard_with_options(n: usize) -> FirstRunWizard {
        let mut w = FirstRunWizard::new(test_profile());
        w.phase = WizardPhase::PickChannels;
        w.open_picker(
            (0..n)
                .map(|i| (format!("ch{i}"), format!("Channel {i}")))
                .collect(),
        );
        w
    }

    /// The bug: `ActiveChoose` is shared with `SetupOverlay`, but the scroll
    /// state lives on the owner — the overlay has `choose_scroll`, the wizard
    /// had nothing. `channel_options()` returns all 16 channels into a
    /// `Constraint::Min(4)` chunk, so on a 24-row terminal the cursor walked
    /// into rows that had been clipped away.
    #[test]
    fn cursor_stays_inside_the_viewport_walking_all_the_way_down() {
        let mut w = wizard_with_options(16);
        w.last_choose_viewport = 4;
        for _ in 0..20 {
            w.picker_move_down();
            let cursor = w.picker.as_ref().unwrap().cursor;
            assert!(
                cursor >= w.choose_scroll && cursor < w.choose_scroll + 4,
                "cursor {cursor} outside window [{}, {})",
                w.choose_scroll,
                w.choose_scroll + 4
            );
        }
    }

    #[test]
    fn cursor_stays_inside_the_viewport_walking_back_up() {
        let mut w = wizard_with_options(16);
        w.last_choose_viewport = 4;
        for _ in 0..15 {
            w.picker_move_down();
        }
        for _ in 0..20 {
            w.picker_move_up();
            let cursor = w.picker.as_ref().unwrap().cursor;
            assert!(
                cursor >= w.choose_scroll && cursor < w.choose_scroll + 4,
                "cursor {cursor} outside window [{}, {})",
                w.choose_scroll,
                w.choose_scroll + 4
            );
        }
        assert_eq!(w.choose_scroll, 0, "back at the top the view must be too");
    }

    /// The view must sit still while the cursor is inside it — scrolling on
    /// every keypress would make a short list jitter.
    #[test]
    fn no_scroll_while_the_cursor_fits() {
        let mut w = wizard_with_options(16);
        w.last_choose_viewport = 8;
        for _ in 0..7 {
            w.picker_move_down();
            assert_eq!(w.choose_scroll, 0);
        }
    }

    /// A viewport of 0 (layout gave nothing) must not divide-by-zero or hang.
    #[test]
    fn a_zero_viewport_is_treated_as_one_row() {
        let mut w = wizard_with_options(16);
        w.last_choose_viewport = 0;
        for _ in 0..5 {
            w.picker_move_down();
        }
        let cursor = w.picker.as_ref().unwrap().cursor;
        assert_eq!(
            w.choose_scroll, cursor,
            "1-row window pins scroll to cursor"
        );
    }

    /// Opening a fresh picker must not inherit the previous one's scroll.
    #[test]
    fn opening_a_picker_resets_the_scroll() {
        let mut w = wizard_with_options(16);
        w.last_choose_viewport = 4;
        for _ in 0..12 {
            w.picker_move_down();
        }
        assert!(w.choose_scroll > 0);
        w.open_picker(vec![("a".into(), "A".into())]);
        assert_eq!(w.choose_scroll, 0);
    }
}

#[cfg(test)]
mod forward_state_tests {
    use super::*;
    use crate::profile::Profile;
    use std::path::PathBuf;

    fn test_profile() -> Profile {
        Profile {
            name: "test".into(),
            root: PathBuf::from("/tmp/rantaiclaw-test"),
        }
    }

    fn phase_label(phase: &WizardPhase) -> String {
        match phase {
            WizardPhase::RunningProvisioner { name } => name.clone(),
            WizardPhase::Welcome => "Welcome".into(),
            WizardPhase::PickChannels => "PickChannels".into(),
            WizardPhase::PickIntegrations => "PickIntegrations".into(),
            WizardPhase::Complete => "Complete".into(),
        }
    }

    // Only back()/scroll were covered; the forward machine (start_provisioners →
    // advance_to_next_in_queue_or_picker) was not. This pins the whole forward
    // sequence: every required provisioner in canonical order, then the two
    // pickers, then Complete.
    #[test]
    fn forward_walk_runs_required_provisioners_then_pickers_in_order() {
        let mut w = FirstRunWizard::new(test_profile());
        assert_eq!(w.phase, WizardPhase::Welcome);

        let mut trace: Vec<String> = Vec::new();
        w.start_provisioners();
        for _ in 0..20 {
            trace.push(phase_label(&w.phase));
            if w.phase == WizardPhase::Complete {
                break;
            }
            w.advance_to_next_in_queue_or_picker();
        }

        assert_eq!(
            trace,
            [
                "provider",
                "approvals",
                "login",
                "persona",
                "skills",
                "PickChannels",
                "PickIntegrations",
                "Complete",
            ]
            .map(String::from)
            .to_vec(),
        );
    }

    // The picker path: selected options are queued in INDEX order (selection is
    // stored sorted), regardless of the order they were toggled, and the first
    // becomes the running phase.
    #[test]
    fn picker_selection_queues_in_index_order_and_runs_the_first() {
        let mut w = FirstRunWizard::new(test_profile());
        // Drain the required queue so the picker drives what runs next.
        w.queue.clear();
        w.phase = WizardPhase::PickChannels;
        w.open_picker(vec![
            ("telegram".into(), "Telegram".into()),
            ("discord".into(), "Discord".into()),
            ("slack".into(), "Slack".into()),
        ]);

        // Toggle slack (idx 2) FIRST, then telegram (idx 0).
        w.picker_move_down();
        w.picker_move_down();
        w.picker_toggle();
        w.picker_move_up();
        w.picker_move_up();
        w.picker_toggle();

        w.apply_picker_selection();

        // Index order (telegram=0, slack=2) wins over toggle order: telegram
        // runs first, slack is queued next.
        assert_eq!(w.current_provisioner_name(), Some("telegram"));
        assert_eq!(w.queue, vec!["slack".to_string()]);
    }
}
