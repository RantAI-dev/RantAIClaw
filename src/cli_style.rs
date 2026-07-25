//! Minimal, consistent presentation helpers for CLI command output.
//!
//! Built on the `console` crate, which strips ANSI escapes automatically when
//! stdout is not a terminal (piped or redirected) — so styled output never
//! pollutes machine-readable pipes. The visual language is deliberately plain:
//! dim uppercase section headers, dim labels with default-weight values, and
//! `●`/`○` status dots (green when active, dim when not).

use console::style;

/// Program title line, e.g. `RantaiClaw  v0.10.0-alpha`.
pub(crate) fn title(name: &str, version: &str) {
    println!(
        "{}  {}",
        style(name).bold(),
        style(format!("v{version}")).dim()
    );
}

/// Dim uppercase section header, preceded by a blank line.
pub(crate) fn section(name: &str) {
    println!();
    println!("{}", style(name.to_uppercase()).dim().bold());
}

/// Two-column field row: dim label left-padded to `width`, then the value.
pub(crate) fn field(label: &str, width: usize, value: &str) {
    println!("  {}  {}", style(format!("{label:<width$}")).dim(), value);
}

/// Status dot — green filled `●` when active, dim hollow `○` when not.
pub(crate) fn dot(active: bool) -> String {
    if active {
        style("●").green().to_string()
    } else {
        style("○").dim().to_string()
    }
}

/// Status row: `  ● Name            note` — dot, name padded to `width`, dim note.
pub(crate) fn status_row(active: bool, name: &str, width: usize, note: &str) {
    println!("  {} {:<width$} {}", dot(active), name, style(note).dim());
}

/// Dim secondary text (e.g. hints, timestamps).
pub(crate) fn dim(s: &str) -> String {
    style(s).dim().to_string()
}

/// Yellow warning text.
pub(crate) fn warn(s: &str) -> String {
    style(s).yellow().to_string()
}
