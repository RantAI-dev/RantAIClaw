//! Setup overlay widget — renders ProvisionEvent stream and captures user input.

use crate::onboard::provision::{ProvisionEvent, Severity};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

#[derive(Debug, Clone)]
pub struct ActivePrompt {
    pub id: String,
    pub label: String,
    pub default: Option<String>,
    pub secret: bool,
}

#[derive(Debug, Default)]
pub struct SetupOverlayState {
    pub title: String,
    log: Vec<String>,
    qr: Option<(String, String)>,
    prompt: Option<ActivePrompt>,
    input: String,
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
            ProvisionEvent::Choose { .. } => {
                self.log.push("(choose UI not yet wired)".into());
            }
            ProvisionEvent::Done { summary } => {
                self.log.push(format!("✓ {summary}"));
                self.closed = true;
            }
            ProvisionEvent::Failed { error } => {
                self.log.push(format!("✗ {error}"));
                self.closed = true;
            }
        }
    }

    pub fn log_lines(&self) -> &[String] {
        &self.log
    }

    pub fn active_prompt(&self) -> Option<&ActivePrompt> {
        self.prompt.as_ref()
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

    pub fn submit_prompt(&mut self) -> Option<(String, String)> {
        let p = self.prompt.take()?;
        let value = if self.input.is_empty() {
            p.default.clone().unwrap_or_default()
        } else {
            std::mem::take(&mut self.input)
        };
        Some((p.id, value))
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.title.as_str())
            .title_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .style(Style::default().fg(Color::Cyan));
        let mut lines: Vec<Line> = self.log.iter().map(|l| Line::from(l.as_str())).collect();
        if let Some((qr, cap)) = &self.qr {
            lines.push(Line::from(""));
            lines.push(
                Line::from(cap.as_str()).style(Style::default().add_modifier(Modifier::BOLD)),
            );
            for qrl in qr.lines() {
                lines.push(Line::from(qrl));
            }
        }
        if let Some(p) = &self.prompt {
            lines.push(Line::from(""));
            let masked = if p.secret {
                "•".repeat(self.input.len())
            } else {
                self.input.clone()
            };
            lines.push(Line::from(format!("{}: {}_", p.label, masked)));
        }
        let para = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        f.render_widget(para, area);
    }
}

fn render_qr_block(payload: &str) -> String {
    use qrcode::{render::unicode, QrCode};
    match QrCode::new(payload.as_bytes()) {
        Ok(qr) => qr.render::<unicode::Dense1x2>().build(),
        Err(_) => format!("[QR render failed; raw payload: {payload}]"),
    }
}
