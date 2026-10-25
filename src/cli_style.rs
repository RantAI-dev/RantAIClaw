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

/// The drift note an operator surface prints when the preset marker implies a
/// different autonomy level than the one the runtime enforces, `None` when
/// they agree (or no marker was provisioned).
///
/// One helper for `status` and `autonomy`, so the two surfaces print the same
/// words for the same state. The model never sees this text — model-facing
/// code goes through `preset_for_prompt`, which resolves the same drift by
/// naming no preset at all.
pub(crate) fn autonomy_drift_note(
    policy_dir: &std::path::Path,
    enforced: crate::security::AutonomyLevel,
) -> Option<String> {
    let preset = crate::approval::policy_writer::read_active_preset(policy_dir)?;
    let implied = preset.autonomy_level();
    (implied != enforced).then(|| {
        format!(
            "⚠ preset marker implies {implied:?} but the runtime enforces {enforced:?}; run `rantaiclaw autonomy {}` to resync.",
            preset.id()
        )
    })
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

    /// The drift note fires exactly when the marker implies another level
    /// than the one enforced — the state an operator must be told about — and
    /// is silent when they agree or nothing was provisioned.
    #[test]
    fn the_drift_note_fires_only_on_a_marker_gate_disagreement() {
        use crate::approval::policy_writer::{write_active_preset, PolicyPreset};
        use crate::security::AutonomyLevel;

        let dir = tempfile::tempdir().expect("temp policy dir");
        // Nothing provisioned: no marker, no note.
        assert_eq!(
            autonomy_drift_note(dir.path(), AutonomyLevel::Supervised),
            None
        );

        // Marker and gate agree (Manual implies Supervised): no note.
        write_active_preset(dir.path(), PolicyPreset::Manual).expect("write marker");
        assert_eq!(
            autonomy_drift_note(dir.path(), AutonomyLevel::Supervised),
            None
        );

        // Marker implies another level than the gate runs: the note names
        // both levels and the resync command.
        for (marker, enforced) in [
            (PolicyPreset::Off, AutonomyLevel::ReadOnly),
            (PolicyPreset::Strict, AutonomyLevel::Supervised),
            (PolicyPreset::Manual, AutonomyLevel::Full),
        ] {
            write_active_preset(dir.path(), marker).expect("rewrite marker");
            let note = autonomy_drift_note(dir.path(), enforced)
                .unwrap_or_else(|| panic!("marker {} vs {enforced:?} must drift", marker.id()));
            assert!(
                note.contains(&format!("{:?}", marker.autonomy_level())),
                "the note names the level the marker implies: {note}"
            );
            assert!(
                note.contains(&format!("{enforced:?}")),
                "the note names the level the runtime enforces: {note}"
            );
            assert!(
                note.contains("rantaiclaw autonomy"),
                "the note says how to resync: {note}"
            );
        }
    }

    /// `status` must print the same drift note `autonomy` prints: one helper
    /// produces the words, and the source scan here pins that the `Status`
    /// command arm consults it (the note's own unit test above is the
    /// behaviour half).
    #[test]
    fn status_prints_the_drift_note_through_the_shared_helper() {
        let src = include_str!("main.rs");
        const TEST_MODULE_MARKER: &str = "\n#[cfg(test)]\nmod tests";
        let cut = src
            .match_indices(TEST_MODULE_MARKER)
            .map(|(at, _)| at)
            .min()
            .unwrap_or(src.len());
        let production = &src[..cut];
        let status_arm = production
            .split("Some(Commands::Status)")
            .nth(1)
            .expect("the Status command arm");
        assert!(
            status_arm.contains("cli_style::autonomy_drift_note"),
            "the `status` command must consult the shared drift-note helper, \
             so it warns in the same words `autonomy` does"
        );
    }
}
