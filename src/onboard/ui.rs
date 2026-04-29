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
    eprintln!("{} {}", yellow().apply_to("⚠"), msg);
}

/// Print a red error line with the `✗` glyph.
pub fn error(msg: &str) {
    eprintln!("{} {}", red().apply_to("✗"), msg);
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
///
/// Uses the brand splash (figlet banner + logo + framed pane) so the
/// setup wizard reads as the same product as `rantaiclaw agent`.
pub fn render_welcome_banner() -> String {
    let inv = super::branding::Inventory {
        title: concat!("RantaiClaw v", env!("CARGO_PKG_VERSION"), " · Setup Wizard"),
        subtitle: Some("Press Ctrl-C to abort at any time."),
        tools: &[],
        skills: &[],
        footer: Some("Let's get you configured."),
    };
    let mut out = super::branding::render_splash(&inv);
    out.push('\n');
    out
}

/// Print the wizard welcome banner. Called once at the top of `run_wizard`.
pub fn print_welcome_banner() {
    print!("{}", render_welcome_banner());
}

/// Render (without printing) the wizard completion banner with optional
/// next-step bullets.
pub fn render_completion_banner(next_steps: &[&str]) -> String {
    let frame = green().bold();
    let arrow = cyan().apply_to("→");
    let mut out = String::new();
    out.push('\n');
    writeln!(out, "{}", frame.apply_to(format!("┌{BANNER_INNER}┐"))).unwrap();
    writeln!(
        out,
        "{}",
        frame.apply_to("│            ✓ Setup Complete!                            │")
    )
    .unwrap();
    writeln!(out, "{}", frame.apply_to(format!("└{BANNER_INNER}┘"))).unwrap();
    if !next_steps.is_empty() {
        writeln!(out, "\n{} Next steps:", arrow).unwrap();
        for line in next_steps {
            writeln!(out, "  {} {}", cyan().apply_to("•"), line).unwrap();
        }
    }
    out.push('\n');
    out
}

/// Print the wizard completion banner. Called at the success path of
/// `run_wizard`, `run_quick_setup_with_home`, and `run_channels_repair_wizard`.
pub fn print_completion_banner(next_steps: &[&str]) {
    print!("{}", render_completion_banner(next_steps));
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
            stripped.contains("Setup Wizard"),
            "should advertise the setup wizard: {stripped:?}"
        );
        assert!(
            stripped.contains("Ctrl-C"),
            "should mention abort hint: {stripped:?}"
        );
        assert!(stripped.contains("┌"));
        assert!(stripped.contains("└"));
    }

    #[test]
    fn completion_banner_includes_all_next_steps() {
        let rendered = render_completion_banner(&[
            "rantaiclaw chat — start a session",
            "rantaiclaw status — verify installation",
        ]);
        let stripped = strip_ansi_codes(&rendered);
        assert!(stripped.contains("Setup Complete"), "got {stripped:?}");
        assert!(stripped.contains("rantaiclaw chat"));
        assert!(stripped.contains("rantaiclaw status"));
        assert!(stripped.contains("Next steps"));
    }

    #[test]
    fn completion_banner_handles_empty_next_steps() {
        let rendered = render_completion_banner(&[]);
        let stripped = strip_ansi_codes(&rendered);
        assert!(stripped.contains("Setup Complete"));
        // No "Next steps" header when list is empty.
        assert!(!stripped.contains("Next steps"));
    }
}
