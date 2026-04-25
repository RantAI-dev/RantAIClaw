//! Onboarding UX helpers — palette, glyphs, banners, section headers.
//!
//! Mirrors `scripts/lib/ui.sh` palette and glyph choices so the visual handoff
//! between the bash bootstrap phase and the Rust wizard phase reads as one
//! product. Pure presentation; no side effects.

use console::Style;

/// Cyan style used for `info` glyphs and step counters.
pub fn cyan() -> Style {
    Style::new().cyan()
}

/// Green style used for `success` glyphs and the completion banner.
pub fn green() -> Style {
    Style::new().green()
}

/// Yellow style used for `warn` glyphs.
pub fn yellow() -> Style {
    Style::new().yellow()
}

/// Red style used for `error` glyphs.
pub fn red() -> Style {
    Style::new().red()
}

/// Magenta style used for the welcome banner.
pub fn magenta() -> Style {
    Style::new().magenta()
}

/// Bold style used for banner frames and step labels.
pub fn bold() -> Style {
    Style::new().bold()
}

/// Print a cyan info line with the `→` glyph.
pub fn info(msg: &str) {
    println!("{} {}", cyan().apply_to("→"), msg);
}

/// Print a green success line with the `✓` glyph.
pub fn success(msg: &str) {
    println!("{} {}", green().apply_to("✓"), msg);
}

/// Print a yellow warn line with the `⚠` glyph.
pub fn warn(msg: &str) {
    println!("{} {}", yellow().apply_to("⚠"), msg);
}

/// Print a red error line with the `✗` glyph.
pub fn error(msg: &str) {
    println!("{} {}", red().apply_to("✗"), msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use console::strip_ansi_codes;

    // Log helpers print directly to stdout; we test the styled glyph
    // rendering via `strip_ansi_codes`. Banner/section-header tests in
    // later tasks add `render_*` pure-format helpers that return Strings.

    #[test]
    fn info_glyph_strips_to_arrow() {
        let rendered = format!("{}", cyan().apply_to("→"));
        assert_eq!(strip_ansi_codes(&rendered), "→");
    }

    #[test]
    fn success_glyph_strips_to_check() {
        let rendered = format!("{}", green().apply_to("✓"));
        assert_eq!(strip_ansi_codes(&rendered), "✓");
    }

    #[test]
    fn warn_glyph_strips_to_warning() {
        let rendered = format!("{}", yellow().apply_to("⚠"));
        assert_eq!(strip_ansi_codes(&rendered), "⚠");
    }

    #[test]
    fn error_glyph_strips_to_cross() {
        let rendered = format!("{}", red().apply_to("✗"));
        assert_eq!(strip_ansi_codes(&rendered), "✗");
    }
}
