//! First-run wizard state machine.
//!
//! Curated flow — only the four high-impact provisioners run by default,
//! plus two opt-in multi-select gates for channels and integrations.
//! Everything else is reachable via `/setup <topic>` later.
//!
//! Phase flow:
//!   Welcome
//!     → RunningProvisioner ("provider")           required
//!     → RunningProvisioner ("approvals")          quick, skippable
//!     → RunningProvisioner ("persona")            quick, skippable
//!     → RunningProvisioner ("skills")             quick, skippable
//!     → PickChannels                              multi-select over 16 channels
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
    /// Provisioners still pending. The head is the next one to run.
    /// The queue is mutated as the wizard advances or as the user
    /// makes selections in the picker phases.
    pub queue: Vec<String>,
    /// Active multi-select picker (only Some during PickChannels /
    /// PickIntegrations). Reuses the ActiveChoose state shape from
    /// setup_overlay so the render code is consistent.
    pub picker: Option<ActiveChoose>,
    /// Parallel to `picker.options`: provisioner names to push onto
    /// the queue for selected indices. Populated alongside
    /// `open_picker`; cleared on submit.
    pub picker_names: Vec<String>,
    pub profile: Profile,
    /// 1-based step counter shown in the header. We can only know the
    /// upper bound at run-time once channel/integration picks are
    /// made, so this is "best effort": before pickers run, it's the
    /// guaranteed-mandatory count + already-chosen optional count;
    /// after Complete, it's the final number.
    pub step: usize,
    pub total_estimate: usize,
}

const REQUIRED_PROVISIONERS: &[&str] = &["provider", "approvals", "persona", "skills"];
const INTEGRATION_OPTIONS: &[(&str, &str)] = &[
    ("mcp", "MCP servers (curated tool plugins)"),
    ("web-search", "Web search backend"),
    ("memory", "Memory backend (sqlite / postgres / markdown)"),
];

impl FirstRunWizard {
    pub fn new(profile: Profile) -> Self {
        let queue: Vec<String> = REQUIRED_PROVISIONERS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        // Estimate: Welcome + 4 required + PickChannels + PickIntegrations + Complete
        // = 8. Real total grows when the user picks channels/integrations.
        let total_estimate = REQUIRED_PROVISIONERS.len() + 4;
        Self {
            phase: WizardPhase::Welcome,
            queue,
            picker: None,
            picker_names: Vec::new(),
            profile,
            step: 1,
            total_estimate,
        }
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

    pub fn step(&self) -> usize {
        self.step
    }

    pub fn total_steps(&self) -> usize {
        self.total_estimate.max(self.step)
    }

    /// Begin the wizard's provisioner sequence from the Welcome screen.
    pub fn start_provisioners(&mut self) {
        self.advance_to_next_in_queue_or_picker();
    }

    /// Pop the next provisioner off the queue. If empty, transition to
    /// the appropriate picker phase based on what's already happened.
    /// Called both when starting from Welcome and after each successful
    /// provisioner completion.
    pub fn advance_to_next_in_queue_or_picker(&mut self) {
        self.step += 1;
        if let Some(next) = self.queue_pop_front() {
            self.phase = WizardPhase::RunningProvisioner { name: next };
        } else {
            // Queue drained — figure out which picker (if any) comes next.
            match self.phase {
                WizardPhase::Welcome
                | WizardPhase::RunningProvisioner { .. } => {
                    // First time queue drained: show channel picker.
                    self.phase = WizardPhase::PickChannels;
                }
                WizardPhase::PickChannels => {
                    // Channel picker done + all chosen channels run:
                    // show integration picker.
                    self.phase = WizardPhase::PickIntegrations;
                }
                WizardPhase::PickIntegrations => {
                    // All done.
                    self.phase = WizardPhase::Complete;
                }
                WizardPhase::Complete => {} // already done
            }
        }
    }

    /// Called by the app when the user's multi-select is submitted.
    /// Reads the picker's selection indices, maps them to names via
    /// `picker_names`, pushes onto the queue, and advances.
    pub fn apply_picker_selection(&mut self) {
        let indices = self.picker_submit().unwrap_or_default();
        for i in &indices {
            if let Some(n) = self.picker_names.get(*i) {
                self.queue.push(n.clone());
                self.total_estimate += 1;
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

    /// Initialize the picker for the current phase. `options` is a
    /// parallel list of `(name, label)` pairs — names go to
    /// `picker_names` for later index→name mapping; labels are what
    /// the picker renders.
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
                WizardPhase::PickChannels => "Add channels (Space toggles, Enter confirms)".into(),
                WizardPhase::PickIntegrations => {
                    "Set up integrations now (Space toggles, Enter confirms)".into()
                }
                _ => "Choose options".into(),
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

    /// Take the picker selection (clears the picker) and return the
    /// indices the user toggled on. Caller then maps indices back to
    /// provisioner names via the parallel options vec passed to
    /// `open_picker`.
    pub fn picker_submit(&mut self) -> Option<Vec<usize>> {
        self.picker.take().map(|p| p.selected)
    }

    pub fn render_fullscreen(&self, frame: &mut Frame, area: Rect) {
        if area.height < 8 || area.width < 30 {
            return;
        }

        let coral = Color::Rgb(255, 138, 101);
        let sky = Color::Rgb(94, 184, 255);
        let muted = Color::Rgb(107, 114, 128);
        let frame_color = Color::Rgb(40, 70, 140);
        let emerald = Color::Rgb(52, 211, 153);

        frame.render_widget(Clear, area);

        let outer = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // 0: title
                Constraint::Length(1), // 1: spacer
                Constraint::Min(3),    // 2: content
                Constraint::Length(1), // 3: spacer
                Constraint::Length(1), // 4: footer
            ])
            .split(outer);

        // ── Title ──────────────────────────────────────────────────
        let title_text = match &self.phase {
            WizardPhase::Welcome => "First-Run Setup".to_string(),
            WizardPhase::RunningProvisioner { name } => format!("Setup: {name}"),
            WizardPhase::PickChannels => "Add channels".to_string(),
            WizardPhase::PickIntegrations => "Set up integrations".to_string(),
            WizardPhase::Complete => "Setup Complete!".to_string(),
        };
        let title_color = match self.phase {
            WizardPhase::Complete => emerald,
            _ => coral,
        };
        let step_label = format!("Step {}/{}", self.step(), self.total_steps());
        let title_line = Line::from(vec![
            Span::styled("⚡ ", Style::default().fg(title_color)),
            Span::styled(
                title_text,
                Style::default()
                    .fg(title_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("   ", Style::default()),
            Span::styled(step_label, Style::default().fg(muted)),
        ]);
        frame.render_widget(Paragraph::new(title_line), chunks[0]);

        // ── Content ────────────────────────────────────────────────
        match self.phase {
            WizardPhase::Welcome => {
                let bullet = |text: &str| {
                    Line::from(vec![
                        Span::styled("  · ", Style::default().fg(sky)),
                        Span::styled(text.to_string(), Style::default().fg(muted)),
                    ])
                };
                let lines: Vec<Line> = vec![
                    Line::from(Span::styled(
                        "Welcome. Quick first-run setup walks through:",
                        Style::default().fg(sky).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    bullet("Provider, model, and API key"),
                    bullet("Approval / autonomy tier"),
                    bullet("Agent persona"),
                    bullet("Skills (bundled + ClawHub)"),
                    bullet("Channels you want to enable (optional)"),
                    bullet("Extra integrations: MCP, web-search, memory (optional)"),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Skip any step with Esc. Configure anything later via /setup <topic>.",
                        Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                    )),
                ];
                let body = Paragraph::new(lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(frame_color)),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(body, chunks[2]);
            }

            WizardPhase::RunningProvisioner { .. } => {
                // Active provisioner overlay covers this frame; this is
                // a brief loading placeholder shown between provisioners.
                let body = Paragraph::new(Line::from(Span::styled(
                    "  Loading next step…",
                    Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                )))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(frame_color)),
                );
                frame.render_widget(body, chunks[2]);
            }

            WizardPhase::PickChannels | WizardPhase::PickIntegrations => {
                self.render_picker(frame, chunks[2], sky, muted, frame_color, emerald);
            }

            WizardPhase::Complete => {
                let bullet = |key: &str, text: &str| {
                    Line::from(vec![
                        Span::styled("  · ", Style::default().fg(emerald)),
                        Span::styled(
                            key.to_string(),
                            Style::default().fg(sky).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!(" — {text}"), Style::default().fg(muted)),
                    ])
                };
                let lines: Vec<Line> = vec![
                    Line::from(Span::styled(
                        "Your RantaiClaw workspace is ready.",
                        Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Next steps:",
                        Style::default().fg(coral).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    bullet("rantaiclaw", "open the chat TUI"),
                    bullet("/setup", "interactive setup picker inside the TUI"),
                    bullet("rantaiclaw setup <topic>", "reconfigure a single topic from a shell"),
                ];
                let body = Paragraph::new(lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(emerald)),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(body, chunks[2]);
            }
        }

        // ── Footer ─────────────────────────────────────────────────
        let footer_spans: Vec<Span> = match self.phase {
            WizardPhase::Welcome => vec![
                Span::styled(
                    "Enter",
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" begin · ", Style::default().fg(muted)),
                Span::styled(
                    "Esc",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" exit", Style::default().fg(muted)),
            ],
            WizardPhase::RunningProvisioner { .. } => vec![Span::styled(
                "(provisioner overlay active)",
                Style::default().fg(muted).add_modifier(Modifier::ITALIC),
            )],
            WizardPhase::PickChannels | WizardPhase::PickIntegrations => vec![
                Span::styled("↑/↓", Style::default().fg(sky)),
                Span::styled(" navigate · ", Style::default().fg(muted)),
                Span::styled("Space", Style::default().fg(sky)),
                Span::styled(" toggle · ", Style::default().fg(muted)),
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
            ],
            WizardPhase::Complete => vec![
                Span::styled(
                    "Enter",
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" close · ", Style::default().fg(muted)),
                Span::styled(
                    "Esc",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" close", Style::default().fg(muted)),
            ],
        };
        frame.render_widget(Paragraph::new(Line::from(footer_spans)), chunks[4]);
    }

    fn render_picker(
        &self,
        frame: &mut Frame,
        area: Rect,
        sky: Color,
        muted: Color,
        frame_color: Color,
        emerald: Color,
    ) {
        let Some(p) = &self.picker else {
            // Picker phase but no picker built yet — show placeholder.
            let body = Paragraph::new(Line::from(Span::styled(
                "  Loading…",
                Style::default().fg(muted).add_modifier(Modifier::ITALIC),
            )))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(frame_color)),
            );
            frame.render_widget(body, area);
            return;
        };

        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(
                p.label.clone(),
                Style::default().fg(sky).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        for (i, opt) in p.options.iter().enumerate() {
            let is_cursor = i == p.cursor;
            let is_checked = p.selected.contains(&i);
            let arrow = if is_cursor { "▸ " } else { "  " };
            let marker = if is_checked { "[x] " } else { "[ ] " };
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
        if p.selected.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Nothing selected — Enter to skip this step.",
                Style::default().fg(muted).add_modifier(Modifier::ITALIC),
            )));
        }
        let body = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(frame_color)),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(body, area);
    }
}

/// List of integration names + display labels for the picker.
pub fn integration_options() -> Vec<(String, String)> {
    INTEGRATION_OPTIONS
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// Channel options pulled from the provisioner registry, filtered to
/// `ProvisionerCategory::Channel`. Returns `(name, description)` pairs
/// — the wizard renders descriptions and maps the chosen indices back
/// to names.
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
