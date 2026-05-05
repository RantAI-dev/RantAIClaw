//! First-run wizard state machine.
//!
//! Drives the user through every registered provisioner in
//! `wizard_provisioner_order()` in sequence, then lands on a Complete
//! screen. The wizard itself only tracks phase + progress; per-step
//! UI lives in the setup_overlay (which the app opens for each
//! provisioner via `open_setup_overlay`).
//!
//! Phase flow:
//!   Welcome → RunningProvisioner{0} → … → RunningProvisioner{N-1} → Complete
//!
//! On Done → wizard auto-advances to the next provisioner.
//! On Failed → wizard halts in the current RunningProvisioner phase;
//! the overlay shows the error and the user picks a path forward
//! (Esc to abort the wizard, or — future — a retry action).

use crate::profile::Profile;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardPhase {
    Welcome,
    RunningProvisioner { idx: usize },
    Complete,
}

#[derive(Debug)]
pub struct FirstRunWizard {
    pub phase: WizardPhase,
    pub provisioner_idx: usize,
    pub provisioner_total: usize,
    pub profile: Profile,
}

impl FirstRunWizard {
    pub fn new(profile: Profile) -> Self {
        let provisioner_total = wizard_provisioner_order().len();
        Self {
            phase: WizardPhase::Welcome,
            provisioner_idx: 0,
            provisioner_total,
            profile,
        }
    }

    pub fn current_provisioner_name(&self) -> Option<&'static str> {
        wizard_provisioner_order()
            .get(self.provisioner_idx)
            .copied()
    }

    pub fn is_provisioner_running(&self) -> bool {
        matches!(self.phase, WizardPhase::RunningProvisioner { .. })
    }

    /// Step number (1-based) for the header label.
    /// Welcome = 1, each provisioner = 2 + idx, Complete = 2 + total.
    pub fn step(&self) -> usize {
        match self.phase {
            WizardPhase::Welcome => 1,
            WizardPhase::RunningProvisioner { idx } => 2 + idx,
            WizardPhase::Complete => 2 + self.provisioner_total,
        }
    }

    pub fn total_steps(&self) -> usize {
        // Welcome + N provisioners + Complete
        2 + self.provisioner_total
    }

    /// Call when the active provisioner emits Done. Advances to the next
    /// provisioner, or to Complete if this was the last one.
    pub fn advance_after_success(&mut self) {
        self.provisioner_idx += 1;
        if self.provisioner_idx < self.provisioner_total {
            self.phase = WizardPhase::RunningProvisioner {
                idx: self.provisioner_idx,
            };
        } else {
            self.phase = WizardPhase::Complete;
        }
    }

    /// Begin the provisioner sequence from the Welcome screen.
    pub fn start_provisioners(&mut self) {
        self.provisioner_idx = 0;
        self.phase = WizardPhase::RunningProvisioner { idx: 0 };
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

        // Outer 2-col / 1-row margin so the wizard breathes — same
        // breathing room the list_picker fullscreen render uses.
        let outer = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };

        // Vertical layout: title row, spacer, content area, spacer, footer.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // 0: title row
                Constraint::Length(1), // 1: spacer
                Constraint::Min(3),    // 2: content
                Constraint::Length(1), // 3: spacer
                Constraint::Length(1), // 4: footer
            ])
            .split(outer);

        // ── Title row ──────────────────────────────────────────────
        let title_text = match self.phase {
            WizardPhase::Welcome => "First-Run Setup".to_string(),
            WizardPhase::RunningProvisioner { idx } => format!(
                "Setup: {}",
                self.current_provisioner_name().unwrap_or("?"),
            )
            .replace("Setup: ", &format!("Setup: ({}/{}) ", idx + 1, self.provisioner_total)),
            WizardPhase::Complete => "Setup Complete!".to_string(),
        };
        let title_color = match self.phase {
            WizardPhase::Complete => emerald,
            _ => coral,
        };
        let step_label = format!(
            "Step {}/{}",
            self.step(),
            self.total_steps(),
        );
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
                        "Welcome to RantaiClaw. This wizard will walk you through:",
                        Style::default().fg(sky).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    bullet("Provider, model, and API key"),
                    bullet("Approval / autonomy tier"),
                    bullet("Persona, skills, and MCP servers"),
                    bullet("Memory, runtime, proxy, tunnel, gateway"),
                    bullet("Browser, web search, integrations"),
                    bullet("Sub-agents and routing"),
                    bullet("Secrets, multimodal, hardware"),
                    Line::from(""),
                    Line::from(Span::styled(
                        format!(
                            "{} provisioners total · skip any with Esc · resume later via /setup full",
                            self.provisioner_total,
                        ),
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
                // Intentionally empty — the active setup_overlay covers
                // this whole frame. The alt-screen render dispatch in
                // app.rs draws the overlay before the wizard during
                // RunningProvisioner; this render is a fallback only,
                // shown briefly between provisioners while the next
                // overlay is being spawned.
                let placeholder = Paragraph::new(Line::from(Span::styled(
                    "  Loading next step…",
                    Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                )))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(frame_color)),
                );
                frame.render_widget(placeholder, chunks[2]);
            }

            WizardPhase::Complete => {
                let bullet = |key: &str, text: &str| {
                    Line::from(vec![
                        Span::styled("  · ", Style::default().fg(emerald)),
                        Span::styled(
                            key.to_string(),
                            Style::default().fg(sky).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" — {text}"),
                            Style::default().fg(muted),
                        ),
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
                    bullet("/setup", "reconfigure any topic from inside the TUI"),
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
                Span::styled("Enter", Style::default().fg(emerald).add_modifier(Modifier::BOLD)),
                Span::styled(" begin · ", Style::default().fg(muted)),
                Span::styled("Esc", Style::default().fg(coral).add_modifier(Modifier::BOLD)),
                Span::styled(" exit", Style::default().fg(muted)),
            ],
            WizardPhase::RunningProvisioner { .. } => vec![
                Span::styled(
                    "(provisioner overlay active)",
                    Style::default().fg(muted).add_modifier(Modifier::ITALIC),
                ),
            ],
            WizardPhase::Complete => vec![
                Span::styled("Enter", Style::default().fg(emerald).add_modifier(Modifier::BOLD)),
                Span::styled(" close · ", Style::default().fg(muted)),
                Span::styled("Esc", Style::default().fg(coral).add_modifier(Modifier::BOLD)),
                Span::styled(" close", Style::default().fg(muted)),
            ],
        };
        frame.render_widget(Paragraph::new(Line::from(footer_spans)), chunks[4]);
    }
}

/// Order of provisioners run by `setup full`. Append-only: any
/// reordering or insertion changes the wizard sequence as seen by
/// existing users mid-onboarding.
pub fn wizard_provisioner_order() -> Vec<&'static str> {
    vec![
        "provider",
        "approvals",
        "persona",
        "skills",
        "mcp",
        "memory",
        "runtime",
        "proxy",
        "tunnel",
        "gateway",
        "browser",
        "web-search",
        "composio",
        "agents",
        "model-routes",
        "embedding-routes",
        "secrets",
        "multimodal",
        "hardware",
    ]
}
