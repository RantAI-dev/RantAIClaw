//! Onboarding UX helpers — palette, glyphs, banners, section headers.
//!
//! Mirrors `scripts/lib/ui.sh` palette and glyph choices so the visual handoff
//! between the bash bootstrap phase and the Rust wizard phase reads as one
//! product. Pure presentation; no side effects.

use std::fmt::Write as FmtWrite;

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

const BANNER_INNER: &str = "─────────────────────────────────────────────────────────";

/// Render (without printing) a framed section header. Used in tests; production
/// callers use `print_section_header`.
pub fn render_section_header(current: u8, total: u8, title: &str) -> String {
    let label = format!("Step {current}/{total}: {title}");
    let inner_width = BANNER_INNER.chars().count();
    let pad = inner_width.saturating_sub(label.chars().count() + 4);
    let top = format!(
        "{}{}",
        cyan().apply_to(format!("┌─ {label} ")),
        cyan().apply_to(format!("{}┐", "─".repeat(pad))),
    );
    let bottom = format!("{}", cyan().apply_to(format!("└{BANNER_INNER}┘")));
    format!("{top}\n{bottom}\n")
}

/// Print a framed cyan section header announcing the current step.
pub fn print_section_header(current: u8, total: u8, title: &str) {
    println!();
    print!("{}", render_section_header(current, total, title));
}

/// Render (without printing) the wizard welcome banner.
pub fn render_welcome_banner() -> String {
    let style = magenta().bold();
    let mut out = String::new();
    out.push('\n');
    let _ = writeln!(out, "{}", style.apply_to(format!("┌{BANNER_INNER}┐")));
    let _ = writeln!(
        out,
        "{}",
        style.apply_to("│            ⚙ RantaiClaw Setup Wizard                    │")
    );
    let _ = writeln!(out, "{}", style.apply_to(format!("└{BANNER_INNER}┘")));
    out.push_str("  Let's get you configured. Press Ctrl-C to abort at any time.\n\n");
    out
}

/// Print the wizard welcome banner. Called once at the top of `run_wizard`.
pub fn print_welcome_banner() {
    print!("{}", render_welcome_banner());
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

    #[test]
    fn section_header_format_includes_step_and_title() {
        let rendered = render_section_header(3, 7, "Provider Selection");
        let stripped = strip_ansi_codes(&rendered);
        assert!(
            stripped.contains("Step 3/7: Provider Selection"),
            "got {stripped:?}"
        );
        assert!(stripped.contains("┌"), "missing top frame: {stripped:?}");
        assert!(
            stripped.contains("┐"),
            "missing top-right frame: {stripped:?}"
        );
        assert!(stripped.contains("└"), "missing bottom frame: {stripped:?}");
        assert!(
            stripped.contains("┘"),
            "missing bottom-right frame: {stripped:?}"
        );
    }

    #[test]
    fn welcome_banner_contains_brand() {
        let rendered = render_welcome_banner();
        let stripped = strip_ansi_codes(&rendered);
        assert!(
            stripped.contains("RantaiClaw Setup Wizard"),
            "got {stripped:?}"
        );
        assert!(
            stripped.contains("Ctrl-C"),
            "should mention abort hint: {stripped:?}"
        );
        assert!(stripped.contains("┌"));
        assert!(stripped.contains("└"));
    }
}
