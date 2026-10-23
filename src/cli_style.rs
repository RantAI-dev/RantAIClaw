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

/// Dim uppercase heading string (no surrounding blank line). Used by `section`
/// and by String-building renderers that can't call `println!` directly.
pub(crate) fn heading(name: &str) -> String {
    style(name.to_uppercase()).dim().bold().to_string()
}

/// Dim uppercase section header, preceded by a blank line.
pub(crate) fn section(name: &str) {
    println!("\n{}", heading(name));
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

/// The approval preset the profile has, read from the on-disk marker
/// (`<profile>/policy/autonomy.toml`).
///
/// One helper for every surface, so `status` and `autonomy` cannot
/// disagree. The config cannot tell "no preset was chosen" from "the
/// default", so a config-derived value puts the surfaces back to
/// disagreeing: a fresh profile with the shipped `always_ask` reads as
/// Manual even though nothing chose Manual.
pub(crate) fn autonomy_preset_text(policy_dir: &std::path::Path) -> String {
    match crate::approval::policy_writer::read_active_preset(policy_dir) {
        Some(preset) => format!("{} ({})", preset.label(), preset.id()),
        None => "not provisioned — run `rantaiclaw setup approvals`".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A profile that never chose a preset has none. The shipped default
    /// `always_ask` is non-empty, so a value derived from the config calls
    /// that profile Manual — which is why the marker, not the config, is the
    /// signal that a preset was chosen at all.
    #[test]
    fn a_profile_without_the_marker_has_no_preset() {
        let dir = tempfile::tempdir().expect("temp policy dir");
        let config = crate::config::Config::default();
        assert_eq!(
            config.autonomy.level,
            crate::security::AutonomyLevel::Supervised,
            "the shipped default level"
        );
        assert!(
            !config.autonomy.always_ask.is_empty(),
            "the shipped default `always_ask` is what made a fresh profile look Manual"
        );
        assert_eq!(
            crate::approval::policy_writer::preset_for_autonomy(&config.autonomy),
            crate::approval::policy_writer::PolicyPreset::Manual,
            "the config-derived value — the one a surface must not report"
        );
        assert_eq!(
            autonomy_preset_text(dir.path()),
            "not provisioned — run `rantaiclaw setup approvals`",
            "no marker means no preset, whatever the config says"
        );
    }

    /// A marker names the preset it holds, in the same words every CLI
    /// surface prints.
    #[test]
    fn a_marker_names_the_preset_it_holds() {
        let dir = tempfile::tempdir().expect("temp policy dir");
        std::fs::write(
            dir.path().join("autonomy.toml"),
            "[autonomy]\npreset = \"strict\"\n",
        )
        .expect("write marker");
        assert_eq!(autonomy_preset_text(dir.path()), "Strict (strict)");
    }

    /// The two CLI surfaces must read the preset from the marker. A
    /// config-derived value cannot tell "no preset chosen" from "the
    /// default", so it puts `status` and `autonomy` back to disagreeing.
    #[test]
    fn the_cli_reads_the_preset_from_the_marker_not_the_config() {
        // The production half only: the test module may name the functions.
        let src = include_str!("main.rs");
        const TEST_MODULE_MARKER: &str = "\n#[cfg(test)]\nmod tests";
        let cut = src
            .match_indices(TEST_MODULE_MARKER)
            .map(|(at, _)| at)
            .min()
            .unwrap_or(src.len());
        let production = &src[..cut];
        assert!(
            !production.contains("preset_for_autonomy"),
            "src/main.rs derives the preset from the config again: the config cannot \
             tell \"no preset chosen\" from \"the default\", so `status` and `autonomy` \
             would disagree. Read the marker through cli_style::autonomy_preset_text."
        );
        let call_sites = production
            .matches("cli_style::autonomy_preset_text")
            .count();
        assert!(
            call_sites >= 2,
            "src/main.rs must print the preset through the shared helper on both \
             `status` and `autonomy` (found {call_sites} call sites)"
        );
    }
}
