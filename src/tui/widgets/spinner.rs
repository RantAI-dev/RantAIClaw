use ratatui::{
    style::{Color, Style},
    text::Span,
};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Spinner {
    frame: usize,
    style: Style,
}

impl Spinner {
    pub fn new() -> Self {
        Self {
            frame: 0,
            style: Style::default().fg(Color::Cyan),
        }
    }

    pub fn with_style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    pub fn tick(&mut self) {
        self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
    }

    pub fn reset(&mut self) {
        self.frame = 0;
    }

    pub fn render(&self) -> Span<'static> {
        Span::styled(SPINNER_FRAMES[self.frame].to_string(), self.style)
    }

    pub fn render_with_text(&self, text: &str) -> Vec<Span<'static>> {
        vec![self.render(), Span::raw(" "), Span::raw(text.to_string())]
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_cycles_through_frames() {
        let mut spinner = Spinner::new();
        let first = spinner.render().content.to_string();
        spinner.tick();
        let second = spinner.render().content.to_string();
        assert_ne!(first, second);
    }

    #[test]
    fn spinner_wraps_around() {
        let mut spinner = Spinner::new();
        for _ in 0..SPINNER_FRAMES.len() {
            spinner.tick();
        }
        let after_wrap = spinner.render().content.to_string();
        assert_eq!(after_wrap, SPINNER_FRAMES[0]);
    }

    #[test]
    fn spinner_reset_goes_to_first_frame() {
        let mut spinner = Spinner::new();
        spinner.tick();
        spinner.tick();
        spinner.reset();
        let frame = spinner.render().content.to_string();
        assert_eq!(frame, SPINNER_FRAMES[0]);
    }
}
