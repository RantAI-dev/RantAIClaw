//! First-run wizard state machine.
//!
//! Minimal: just tracks phase and decides what to render.
//! The app drives provisioner execution using `open_setup_overlay`.
//!
//! Phase flow:
//!   Welcome → Provisioner0 → Provisioner1 → ... → ProvisionerN → ProjectContext → ScaffoldFiles → Complete

use crate::profile::Profile;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardPhase {
    Welcome,
    RunningProvisioner { idx: usize },
    ProjectContext,
    ScaffoldFiles,
    Complete,
}

#[derive(Debug)]
pub struct FirstRunWizard {
    pub phase: WizardPhase,
    pub step: usize,
    pub total_steps: usize,
    pub provisioner_idx: usize,
    pub provisioner_total: usize,
    pub profile: Profile,
    pub workspace_dir: Option<PathBuf>,
    pub user_name: String,
    pub timezone: String,
    pub agent_name: String,
    pub communication_style: String,
}

impl FirstRunWizard {
    pub fn new(profile: Profile) -> Self {
        let provisioner_total = wizard_provisioner_order().len();
        let total = 2 + provisioner_total + 4 + 1;
        Self {
            phase: WizardPhase::Welcome,
            step: 1,
            total_steps: total,
            provisioner_idx: 0,
            provisioner_total,
            profile,
            workspace_dir: None,
            user_name: String::new(),
            timezone: String::new(),
            agent_name: String::new(),
            communication_style: String::new(),
        }
    }

    pub fn current_provisioner_name(&self) -> Option<&'static str> {
        let order = wizard_provisioner_order();
        order.get(self.provisioner_idx).copied()
    }

    pub fn is_provisioner_done(&self) -> bool {
        matches!(self.phase, WizardPhase::RunningProvisioner { .. })
    }

    /// Call after a provisioner finishes. Advances to next step.
    /// Returns true if wizard is complete.
    pub fn advance(&mut self) {
        self.provisioner_idx += 1;
        self.step += 1;

        if self.provisioner_idx < self.provisioner_total {
            self.phase = WizardPhase::RunningProvisioner {
                idx: self.provisioner_idx,
            };
        } else {
            self.phase = WizardPhase::ProjectContext;
        }
    }

    /// Call after project context is collected.
    pub fn finish_project_context(&mut self) {
        self.step += 1;
        self.phase = WizardPhase::ScaffoldFiles;
    }

    /// Call after scaffolding is done.
    pub fn finish(&mut self) {
        self.step += 1;
        self.phase = WizardPhase::Complete;
    }

    pub fn handle_provisioner_event(&mut self, ev: crate::onboard::provision::ProvisionEvent) {
        if let crate::onboard::provision::ProvisionEvent::Done { .. }
        | crate::onboard::provision::ProvisionEvent::Failed { .. } = ev
        {
            self.advance();
        }
    }

    pub fn render_fullscreen(&self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        use ratatui::{
            layout::{Constraint, Direction, Layout, Rect},
            style::{Color, Modifier, Style},
            text::{Line, Span},
            widgets::{Block, Borders, Clear, Paragraph, Wrap},
        };

        let coral = Color::Rgb(255, 138, 101);
        let sky = Color::Rgb(94, 184, 255);
        let muted = Color::Rgb(107, 114, 128);
        let emerald = Color::Rgb(52, 211, 153);

        frame.render_widget(Clear, area);

        let outer = Rect {
            x: area.x + 3,
            y: area.y + 2,
            width: area.width.saturating_sub(6),
            height: area.height.saturating_sub(4),
        };

        match self.phase {
            WizardPhase::Welcome => {
                let heading = Line::from(Span::styled(
                    "⚡ RantaiClaw First-Run Setup ⚡",
                    Style::default().fg(coral).add_modifier(Modifier::BOLD),
                ));
                let lines: Vec<Line> = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "Let's get you set up. This wizard will:",
                        Style::default().fg(sky),
                    )),
                    Line::from(Span::styled(
                        "  · Configure your AI provider and API key",
                        Style::default().fg(muted),
                    )),
                    Line::from(Span::styled(
                        "  · Set up messaging channels (optional)",
                        Style::default().fg(muted),
                    )),
                    Line::from(Span::styled(
                        "  · Personalize your agent's personality",
                        Style::default().fg(muted),
                    )),
                    Line::from(Span::styled(
                        "  · Configure memory, tools, and security",
                        Style::default().fg(muted),
                    )),
                    Line::from(Span::styled(
                        "  · Create your workspace files",
                        Style::default().fg(muted),
                    )),
                    Line::from(""),
                    Line::from(Span::raw("")),
                    Line::from(
                        Span::styled("Press ", Style::default().fg(muted))
                            + Span::styled(
                                "Enter",
                                Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                            )
                            + Span::styled(" to begin setup", Style::default().fg(muted)),
                    ),
                    Line::from(
                        Span::styled("Press ", Style::default().fg(muted))
                            + Span::styled(
                                "Esc",
                                Style::default().fg(coral).add_modifier(Modifier::BOLD),
                            )
                            + Span::styled(" to exit", Style::default().fg(muted)),
                    ),
                ];
                let all_lines: Vec<Line> = std::iter::once(heading)
                    .chain(std::iter::once(Line::from("")))
                    .chain(lines)
                    .collect();
                let content = Paragraph::new(all_lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(sky)),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(content, outer);
            }
            WizardPhase::RunningProvisioner { .. } => {
                // Provisioner overlay handles its own rendering
            }
            WizardPhase::ProjectContext => {
                let heading = Line::from(vec![
                    Span::styled("Step ", Style::default().fg(muted)),
                    Span::styled(
                        format!("{}/{} ", self.step, self.total_steps),
                        Style::default().fg(sky).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "— Project Context",
                        Style::default().fg(coral).add_modifier(Modifier::BOLD),
                    ),
                ]);
                let lines: Vec<Line> = vec![
                    Line::from(""),
                    Line::from(Span::raw("  This step is in progress.")),
                    Line::from(Span::raw("")),
                    Line::from(
                        Span::styled("Press ", Style::default().fg(muted))
                            + Span::styled(
                                "Enter",
                                Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                            )
                            + Span::styled(" to continue", Style::default().fg(muted)),
                    ),
                ];
                let all_lines: Vec<Line> = std::iter::once(heading).chain(lines).collect();
                let content = Paragraph::new(all_lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(sky)),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(content, outer);
            }
            WizardPhase::ScaffoldFiles => {
                let heading = Line::from(vec![
                    Span::styled("Step ", Style::default().fg(Color::Rgb(107, 114, 128))),
                    Span::styled(
                        format!("{}/{} ", self.step, self.total_steps),
                        Style::default().fg(sky).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "— Creating Workspace Files",
                        Style::default().fg(coral).add_modifier(Modifier::BOLD),
                    ),
                ]);
                let lines: Vec<Line> = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "Creating workspace files...",
                        Style::default().fg(sky),
                    )),
                    Line::from(Span::styled("✓ Done", Style::default().fg(emerald))),
                    Line::from(Span::raw("")),
                    Line::from(
                        Span::styled("Press ", Style::default().fg(muted))
                            + Span::styled(
                                "Enter",
                                Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                            )
                            + Span::styled(" to continue", Style::default().fg(muted)),
                    ),
                ];
                let all_lines: Vec<Line> = std::iter::once(heading).chain(lines).collect();
                let content = Paragraph::new(all_lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(sky)),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(content, outer);
            }
            WizardPhase::Complete => {
                let heading = Line::from(Span::styled(
                    "⚡ Setup Complete! ⚡",
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                ));
                let lines: Vec<Line> = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "Your RantaiClaw workspace is ready.",
                        Style::default().fg(sky),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Next steps:",
                        Style::default().fg(coral).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(
                        "  · rantaiclaw chat  — start a conversation",
                        Style::default().fg(muted),
                    )),
                    Line::from(Span::styled(
                        "  · rantaiclaw agent — run the autonomous agent",
                        Style::default().fg(muted),
                    )),
                    Line::from(Span::styled(
                        "  · rantaiclaw setup — reconfigure any topic",
                        Style::default().fg(muted),
                    )),
                    Line::from(""),
                    Line::from(
                        Span::styled("Press ", Style::default().fg(muted))
                            + Span::styled(
                                "Enter",
                                Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                            )
                            + Span::styled(" to start chatting", Style::default().fg(muted)),
                    ),
                ];
                let all_lines: Vec<Line> = std::iter::once(heading).chain(lines).collect();
                let content = Paragraph::new(all_lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(emerald)),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(content, outer);
            }
        }
    }

    pub fn current_step_display(&self) -> String {
        match self.phase {
            WizardPhase::Welcome => format!("Step {}/{} — Welcome", self.step, self.total_steps),
            WizardPhase::RunningProvisioner { idx } => {
                let name = self.current_provisioner_name().unwrap_or("?");
                format!(
                    "Step {}/{} — Setup: {} ({}/{})",
                    self.step,
                    self.total_steps,
                    name,
                    idx + 1,
                    self.provisioner_total
                )
            }
            WizardPhase::ProjectContext => {
                format!("Step {}/{} — Project Context", self.step, self.total_steps)
            }
            WizardPhase::ScaffoldFiles => format!(
                "Step {}/{} — Creating Workspace Files",
                self.step, self.total_steps
            ),
            WizardPhase::Complete => "Setup Complete!".to_string(),
        }
    }
}

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
