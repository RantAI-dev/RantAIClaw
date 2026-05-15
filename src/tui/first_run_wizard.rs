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
    pub profile: Profile,
    /// Phase history for the back button. Each completed phase transition
    /// pushes the previous phase here; `back()` pops one and restores it.
    /// v0.6.4 covers the safe cases (PickChannels ↔ PickIntegrations and
    /// PickChannels → previous required provisioner). RunningProvisioner
    /// rewind is forward-only for now — the running task can't be
    /// surgically rewound without leaking partial state.
    pub history: Vec<WizardPhase>,
}

const REQUIRED_PROVISIONERS: &[&str] = &["provider", "approvals", "persona", "skills"];
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
    ("03", "Persona"),
    ("04", "Skills"),
    ("05", "Channels"),
    ("06", "Integrations"),
    ("07", "Complete"),
];

const CHANNEL_PROVISIONER_NAMES: &[&str] = &[
    "telegram",
    "discord",
    "slack",
    "whatsapp-web",
    "whatsapp-cloud",
    "signal",
    "matrix",
    "mattermost",
    "imessage",
    "lark",
    "dingtalk",
    "nextcloud-talk",
    "qq",
    "email",
    "irc",
    "linq",
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
        self.picker_names = options.iter().map(|(name, _)| name.clone()).collect();
        let labels: Vec<String> = options.into_iter().map(|(_, label)| label).collect();
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
            multi: true,
            cursor: 0,
            selected: Vec::new(),
        });
    }

    pub fn picker_move_up(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            if p.cursor > 0 {
                p.cursor -= 1;
            }
        }
    }

    pub fn picker_move_down(&mut self) {
        if let Some(p) = self.picker.as_mut() {
            if p.cursor + 1 < p.options.len() {
                p.cursor += 1;
            }
        }
    }

    pub fn picker_toggle(&mut self) {
        if let Some(p) = self.picker.as_mut() {
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
        self.picker.take().map(|p| p.selected)
    }

    /// Map current phase to an index into RAIL.
    fn rail_index(&self) -> Option<usize> {
        match &self.phase {
            WizardPhase::Welcome => None,
            WizardPhase::RunningProvisioner { name } => match name.as_str() {
                "provider" => Some(0),
                "approvals" => Some(1),
                "persona" => Some(2),
                "skills" => Some(3),
                n if is_channel_name(n) => Some(4),
                n if is_integration_name(n) => Some(5),
                _ => None,
            },
            WizardPhase::PickChannels => Some(4),
            WizardPhase::PickIntegrations => Some(5),
            WizardPhase::Complete => Some(6),
        }
    }

    pub fn render_fullscreen(&self, frame: &mut Frame, area: Rect) {
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

    fn render_content(
        &self,
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
                Span::styled("Four required steps,", Style::default().fg(sky)),
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
            bullet("03", "Persona", "agent name & template", coral),
            bullet("04", "Skills", "bundled + ClawHub", coral),
            bullet("05", "Channels", "telegram, discord, whatsapp, …", emerald),
            bullet("06", "Integrations", "mcp, web-search, memory", emerald),
            bullet("07", "Complete", "ship it", sky),
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

    fn render_picker(
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
        for (i, opt) in p.options.iter().enumerate() {
            let is_cursor = i == p.cursor;
            let is_checked = p.selected.contains(&i);
            let arrow = if is_cursor { "▸" } else { " " };
            let arrow_style = if is_cursor {
                Style::default().fg(coral).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let marker = if is_checked { "▣" } else { "□" };
            let marker_style = if is_checked {
                Style::default().fg(emerald)
            } else if is_cursor {
                Style::default().fg(coral)
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
        frame.render_widget(
            Paragraph::new(option_lines).wrap(Wrap { trim: false }),
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
    CHANNEL_PROVISIONER_NAMES.contains(&name)
}

fn is_integration_name(name: &str) -> bool {
    INTEGRATION_OPTIONS.iter().any(|(k, _)| *k == name)
}

/// Integration option list — `(name, description)` pairs.
pub fn integration_options() -> Vec<(String, String)> {
    INTEGRATION_OPTIONS
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
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
}
