//! Full-screen TUI login overlay shown before the app when console login is
//! enabled (`config.gateway.login`). Mirrors the first-run wizard gate: it is
//! rendered over everything and absorbs all input until the password verifies.
//!
//! Honest scope: like the first-run wizard, this gates the *UI*, not process
//! boot — the agent/channels still initialize behind the modal. Anyone who can
//! run the binary can read `config.toml`, so this does not defend against local
//! filesystem access; it is a local unlock (shared terminal / shoulder-surf /
//! defense-in-depth).

use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

#[derive(Default)]
pub struct LoginGateState {
    /// Current password buffer (rendered masked, never as plaintext).
    pub input: String,
    /// Operator username, shown as a label (not required to unlock).
    pub username: Option<String>,
    /// Last error message, if any.
    pub error: Option<String>,
}

impl LoginGateState {
    pub fn new(username: Option<String>) -> Self {
        Self {
            input: String::new(),
            username,
            error: None,
        }
    }

    /// Verify the buffered password against the stored argon2 PHC hash.
    pub fn check(&self, password_hash: &str) -> bool {
        crate::security::login::verify_password(&self.input, password_hash)
    }

    pub fn render_fullscreen(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);

        let coral = Color::Rgb(255, 138, 101);
        let muted = Color::Rgb(107, 114, 128);
        let red = Color::Rgb(239, 68, 68);

        let w = area.width.clamp(20, 60);
        let h = 11u16.min(area.height);
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let card = Rect {
            x,
            y,
            width: w,
            height: h,
        };

        let user = self.username.as_deref().unwrap_or("operator");
        let masked: String = "•".repeat(self.input.chars().count());

        let mut lines = vec![
            Line::from(Span::styled(
                "🔐 Console login",
                Style::default().fg(coral).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(format!("User:     {user}")),
            Line::from(format!("Password: {masked}")),
            Line::from(""),
            Line::from(Span::styled(
                "Enter to unlock · Ctrl+C to quit",
                Style::default().fg(muted),
            )),
        ];
        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(red),
            )));
        }

        let para = Paragraph::new(lines).alignment(Alignment::Left).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" RantaiClaw "),
        );
        frame.render_widget(para, card);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_matches_stored_hash() {
        let hash = crate::security::login::hash_password("pw").unwrap();
        let mut g = LoginGateState::default();
        g.input = "pw".into();
        assert!(g.check(&hash));
        g.input = "nope".into();
        assert!(!g.check(&hash));
    }
}
