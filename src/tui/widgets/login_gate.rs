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
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph},
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

        let user = self.username.as_deref().unwrap_or("operator");
        let masked: String = "•".repeat(self.input.chars().count());

        // Card content. Field labels are padded to a fixed width so the
        // values line up as a form; a caret marks the active password field.
        let mut lines = vec![
            Line::from(Span::styled(
                "🔐  Console login",
                Style::default().fg(coral).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("User      ", Style::default().fg(muted)),
                Span::raw(user.to_string()),
            ]),
            Line::from(vec![
                Span::styled("Password  ", Style::default().fg(muted)),
                Span::raw(masked),
                Span::styled("▏", Style::default().fg(coral)),
            ]),
        ];
        if let Some(err) = &self.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("⚠ {err}"),
                Style::default().fg(red),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Enter to unlock · Ctrl+C to quit",
            Style::default().fg(muted),
        )));

        // Size the card to its content plus a 1×2 inner padding, then centre
        // it on the screen. `+2` per axis accounts for the rounded border.
        const PAD_X: u16 = 2;
        const PAD_Y: u16 = 1;
        const INNER_W: u16 = 38;
        let content_h = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let card_w = (INNER_W + PAD_X * 2 + 2).min(area.width);
        let card_h = (content_h + PAD_Y * 2 + 2).min(area.height);
        let x = area.x + area.width.saturating_sub(card_w) / 2;
        let y = area.y + area.height.saturating_sub(card_h) / 2;
        let card = Rect {
            x,
            y,
            width: card_w,
            height: card_h,
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(coral))
            .padding(Padding::new(PAD_X, PAD_X, PAD_Y, PAD_Y))
            .title(" RantaiClaw ")
            .title_alignment(Alignment::Center);

        frame.render_widget(Paragraph::new(lines).block(block), card);
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
