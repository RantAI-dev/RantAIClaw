//! Approval policy bootstrap — materialises the three on-disk policy
//! files (`autonomy.toml`, `command_allowlist.toml`, `forbidden_paths.toml`)
//! under `<profile>/policy/` from a chosen `PolicyPreset`.
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §6 "Approval runtime" + §"Preset bundles".
//!
//! The four preset bundles ship as `include_str!` resources under
//! `presets/`. Each bundle is a single TOML document containing the
//! sections that fan out to the three output files; `write_policy_files`
//! parses the bundle and re-emits each section into its destination.
//!
//! Idempotence: by default the writer skips any file that already exists
//! on disk so user-edited policy survives re-running the wizard. Pass
//! `force = true` to overwrite from the preset.
//!
//! The `Off` preset prints a stern stderr warning whenever it is
//! selected — the spec is explicit that `Off` is for trusted CI only.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::profile::Profile;

// ── Preset bundles (compiled into the binary) ───────────────────

const MANUAL_BUNDLE: &str = include_str!("presets/policy_manual.toml");
const SMART_BUNDLE: &str = include_str!("presets/policy_smart.toml");
const STRICT_BUNDLE: &str = include_str!("presets/policy_strict.toml");
const OFF_BUNDLE: &str = include_str!("presets/policy_off.toml");

// ── PolicyPreset enum ───────────────────────────────────────────

/// One of the four canonical approval-policy presets.
///
/// Variants are spelt out so callers can pattern-match exhaustively.
/// Legacy `L1`/`L2`/`L3`/`L4` ids remain parseable for backward
/// compatibility with on-disk configs written by earlier releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyPreset {
    /// Manual — paranoid mode. Empty allowlist, every tool prompts.
    Manual,
    /// Smart — safe-by-default. Read-only commands pre-allowed.
    Smart,
    /// Strict — deny-by-default. For unattended agents.
    Strict,
    /// Off — gating disabled. Trusted CI environments only.
    Off,
}

impl PolicyPreset {
    /// Stable identifier (`"manual"` … `"off"`) used in TOML and CLI flags.
    pub fn id(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Smart => "smart",
            Self::Strict => "strict",
            Self::Off => "off",
        }
    }

    /// Parse a case-insensitive preset id. Accepts both new verbal forms
    /// (`"manual"`, `"smart"`, `"strict"`, `"off"`) and legacy `L1`–`L4`
    /// ids so configs written by pre-v0.6.40 releases keep working.
    ///
    /// Also accepts `"full"` as an alias for `Off` so users who reach
    /// for the autonomy-level vocabulary (`AutonomyLevel::Full`) land on
    /// the preset that disables gating.
    pub fn from_str_ci(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "manual" => Ok(Self::Manual),
            "smart" => Ok(Self::Smart),
            "strict" => Ok(Self::Strict),
            "off" | "full" => Ok(Self::Off),
            "l1" => Ok(Self::Manual),
            "l2" => Ok(Self::Smart),
            "l3" => Ok(Self::Strict),
            "l4" => Ok(Self::Off),
            other => Err(anyhow!(
                "unknown policy preset '{other}' (valid: manual, smart, strict, off)"
            )),
        }
    }

    /// Cycle order for the Shift+Tab keybinding and `rantaiclaw autonomy`
    /// CLI: Manual → Smart → Strict → Off → Manual. Picked so a casual
    /// tap walks from paranoid → default → CI → autonomous and back.
    pub fn next(self) -> Self {
        match self {
            Self::Manual => Self::Smart,
            Self::Smart => Self::Strict,
            Self::Strict => Self::Off,
            Self::Off => Self::Manual,
        }
    }

    /// All four presets in cycle order. Used by `/autonomy` to render the
    /// picker list when no argument is supplied.
    pub const ALL: [Self; 4] = [Self::Manual, Self::Smart, Self::Strict, Self::Off];

    /// Map preset to the runtime [`crate::security::AutonomyLevel`].
    ///
    /// `Off` short-circuits to `Full` so `SecurityPolicy::is_command_allowed`
    /// skips every approval check. The other three presets all run under
    /// `Supervised` — they differ in their `command_allowlist.toml` /
    /// `forbidden_paths.toml` contents (which the shell tool reads),
    /// not in their autonomy level.
    ///
    /// This is the bridge between the preset bundle (a human-facing
    /// configuration knob) and the `Config.autonomy.level` field that
    /// `SecurityPolicy::from_config` actually consumes. Without this
    /// mapping the preset switcher updates `<policy_dir>/autonomy.toml`
    /// but the live gate keeps using whatever `config.toml` shipped
    /// with — the v0.6.49 bug.
    pub fn autonomy_level(self) -> crate::security::AutonomyLevel {
        use crate::security::AutonomyLevel;
        match self {
            Self::Manual | Self::Smart | Self::Strict => AutonomyLevel::Supervised,
            Self::Off => AutonomyLevel::Full,
        }
    }

    /// Human-facing label for menus / system messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "Manual",
            Self::Smart => "Smart",
            Self::Strict => "Strict",
            Self::Off => "Off",
        }
    }

    /// Raw bundle TOML for this preset (parsed by `write_policy_files`).
    fn bundle(self) -> &'static str {
        match self {
            Self::Manual => MANUAL_BUNDLE,
            Self::Smart => SMART_BUNDLE,
            Self::Strict => STRICT_BUNDLE,
            Self::Off => OFF_BUNDLE,
        }
    }
}

/// Apply `preset` to the in-memory `Config`. Updates two fields:
///
/// 1. `config.autonomy.level` — drives `SecurityPolicy.autonomy`
///    (Manual/Smart/Strict → Supervised, Off → Full).
/// 2. `config.autonomy.allowed_commands` — basenames extracted from
///    the preset bundle's `[command_allowlist].patterns`. This bridges
///    the bundle (a write-only TOML file before v0.6.51) into the
///    list the runtime gate actually consults. Without this step, the
///    `cd`/`echo`/`which`/etc. patterns I added to the Smart bundle
///    were dead weight — the gate kept using the hardcoded default
///    (`git`, `npm`, `cargo`, `ls`, …) and prompted for everything else.
///
/// Caller is responsible for persisting the change (`config.save().await`)
/// and, in the TUI, triggering `reload_config` so the running agent
/// rebuilds its `SecurityPolicy` with the new lists.
pub fn apply_preset_to_config(config: &mut crate::config::Config, preset: PolicyPreset) {
    config.autonomy.level = preset.autonomy_level();
    if let Ok(bundle) = toml::from_str::<PolicyBundle>(preset.bundle()) {
        let mut basenames: Vec<String> = bundle
            .command_allowlist
            .patterns
            .iter()
            .filter_map(|pat| {
                // The bundle uses `<command> <args>` glob patterns
                // (e.g. `"git status"`, `"curl --head *"`). Strip the
                // arg suffix — the runtime gate matches on basename
                // only, not the glob — and any absolute-path prefix.
                let first = pat.split_whitespace().next()?;
                let base = first.rsplit('/').next()?;
                if base.is_empty() {
                    None
                } else {
                    Some(base.to_string())
                }
            })
            .collect();
        basenames.sort();
        basenames.dedup();
        config.autonomy.allowed_commands = basenames;
    }
}

/// Read the currently-active preset from `<policy_dir>/autonomy.toml`.
///
/// Returns `None` if the file is missing, unparseable, or the
/// `[autonomy].preset` field is absent — all of which mean "the policy
/// dir hasn't been provisioned yet" (the user is pre-onboarding) rather
/// than an error worth surfacing. Callers should treat `None` as
/// "Smart-equivalent unknown" for display purposes.
pub fn read_active_preset(policy_dir: &Path) -> Option<PolicyPreset> {
    let raw = fs::read_to_string(policy_dir.join("autonomy.toml")).ok()?;
    let table: toml::value::Table = toml::from_str(&raw).ok()?;
    let preset_str = table.get("autonomy")?.as_table()?.get("preset")?.as_str()?;
    PolicyPreset::from_str_ci(preset_str).ok()
}

// ── Bundle deserialisation ──────────────────────────────────────

/// In-memory shape of a preset bundle. Mirrors the TOML files under
/// `presets/` exactly; missing sections become defaults.
#[derive(Debug, Deserialize, Default)]
struct PolicyBundle {
    #[serde(default)]
    autonomy: toml::Table,
    #[serde(default)]
    approvals: toml::Table,
    #[serde(default)]
    command_allowlist: SectionPatterns,
    #[serde(default)]
    forbidden_paths: SectionPatterns,
}

#[derive(Debug, Deserialize, Default)]
struct SectionPatterns {
    #[serde(default)]
    patterns: Vec<String>,
}

// ── Public API ──────────────────────────────────────────────────

/// Stern warning string surfaced when the `Off` preset is selected.
/// Kept as a `const` so every caller (CLI eprintln, TUI system message,
/// setup wizard overlay) emits identical wording without having to
/// duplicate the text. Returned from [`write_policy_files`] as
/// `Some(_)` only when the freshly-written preset is `Off`.
pub const OFF_WARNING: &str = "⚠️  approval policy preset Off selected — gating is OFF. \
Every tool call will execute without prompts. Use this only in trusted CI \
environments. To revert: `rantaiclaw setup approvals --force`.";

/// Write the three policy files for `preset` into `profile.policy_dir()`.
///
/// Returns `Some(OFF_WARNING)` when the `Off` preset is written so the
/// caller can route the stern warning to the right surface (stderr for
/// CLI, system message for TUI). All other presets return `None`.
///
/// * `force = false` (the default for a fresh wizard run) — any file that
///   already exists is left untouched. Idempotent.
/// * `force = true` — every file is rewritten from the bundle, clobbering
///   user edits. Used by `rantaiclaw setup approvals --force`.
///
/// Files written:
///   * `autonomy.toml`          — `[autonomy]` + `[approvals]` from bundle
///   * `command_allowlist.toml` — patterns array
///   * `forbidden_paths.toml`   — patterns array
pub fn write_policy_files(
    profile: &Profile,
    preset: PolicyPreset,
    force: bool,
) -> Result<Option<&'static str>> {
    let dir = profile.policy_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create policy dir {}", dir.display()))?;

    let bundle: PolicyBundle = toml::from_str(preset.bundle()).with_context(|| {
        format!(
            "parse bundled preset {} (this is a build-time error)",
            preset.id()
        )
    })?;

    let wrote_autonomy = write_autonomy(&dir, &bundle, preset, force)?;
    let wrote_allowlist = write_patterns(
        &dir.join("command_allowlist.toml"),
        "command_allowlist",
        &bundle.command_allowlist.patterns,
        ALLOWLIST_HEADER,
        force,
    )?;
    let wrote_forbidden = write_patterns(
        &dir.join("forbidden_paths.toml"),
        "forbidden_paths",
        &bundle.forbidden_paths.patterns,
        FORBIDDEN_HEADER,
        force,
    )?;

    let warning = if matches!(preset, PolicyPreset::Off) {
        Some(OFF_WARNING)
    } else {
        None
    };

    // Round-trip self-check: parse what we just wrote. Catches schema drift
    // between the bundled preset and the writer (e.g. someone adds a new
    // required field to PolicyBundle but forgets to update the templates),
    // bundled preset syntax bugs, and writer-side encoding mistakes.
    //
    // Only verify the files this call actually wrote — when `force=false`
    // and a file already exists on disk we leave the user's edits alone,
    // and that file is no longer the writer's responsibility to validate.
    verify_written_policy(&dir, wrote_autonomy, wrote_allowlist, wrote_forbidden).with_context(
        || {
            format!(
                "approval preset {} wrote policy files but they failed parse-back \
             — preset bundles or writer drift",
                preset.id()
            )
        },
    )?;

    Ok(warning)
}

/// Re-read each of `autonomy.toml`, `command_allowlist.toml`,
/// `forbidden_paths.toml` and confirm they deserialize into the shapes
/// the approval gate consumer code expects. Files that this call did
/// NOT freshly write (the `force=false` no-op path with pre-existing
/// content) are skipped — the user's edits are not the writer's
/// concern.
fn verify_written_policy(
    dir: &Path,
    check_autonomy: bool,
    check_allowlist: bool,
    check_forbidden: bool,
) -> Result<()> {
    if check_autonomy {
        let autonomy = dir.join("autonomy.toml");
        let raw = fs::read_to_string(&autonomy)
            .with_context(|| format!("read {}", autonomy.display()))?;
        let parsed: toml::value::Table =
            toml::from_str(&raw).with_context(|| format!("parse {}", autonomy.display()))?;
        if !parsed.contains_key("autonomy") {
            anyhow::bail!("{} missing required [autonomy] block", autonomy.display());
        }
        if !parsed.contains_key("approvals") {
            anyhow::bail!("{} missing required [approvals] block", autonomy.display());
        }
    }

    let pattern_files: &[(bool, &str, &str)] = &[
        (
            check_allowlist,
            "command_allowlist.toml",
            "command_allowlist",
        ),
        (check_forbidden, "forbidden_paths.toml", "forbidden_paths"),
    ];
    for &(should_check, name, key) in pattern_files {
        if !should_check {
            continue;
        }
        let path = dir.join(name);
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let parsed: toml::value::Table =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        let block = parsed
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("{} missing required [{key}] block", path.display()))?;
        let table = block
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("{} [{key}] is not a table", path.display()))?;
        let patterns = table
            .get("patterns")
            .ok_or_else(|| anyhow::anyhow!("{} [{key}].patterns missing", path.display()))?;
        let arr = patterns.as_array().ok_or_else(|| {
            anyhow::anyhow!("{} [{key}].patterns is not an array", path.display())
        })?;
        for (i, item) in arr.iter().enumerate() {
            if !item.is_str() {
                anyhow::bail!(
                    "{} [{key}].patterns[{i}] is not a string ({:?})",
                    path.display(),
                    item
                );
            }
        }
    }

    Ok(())
}

// ── Internals ───────────────────────────────────────────────────

const AUTONOMY_HEADER: &str = "\
# Autonomy + approval policy for this profile. Generated from a
# RantaiClaw preset (Manual / Smart / Strict / Off); safe to hand-edit.
# Re-running `rantaiclaw setup approvals` without --force will leave
# this file alone.
";

const ALLOWLIST_HEADER: &str = "\
# Command allowlist — globs of `<tool> <args>` shape that the approval
# gate auto-approves. Accreted entries from `[a]lways` prompts append
# here over time. Comments are preserved on round-trip via toml_edit.
";

const FORBIDDEN_HEADER: &str = "\
# Forbidden paths — never-allow globs checked first by the approval
# gate. No allowlist entry, /yolo toggle, or `Off` setting can override
# these paths (spec §6.1).
";

/// Returns `Ok(true)` if the file was freshly written, `Ok(false)` if the
/// existing file was preserved (force=false + path exists).
fn write_autonomy(
    dir: &Path,
    bundle: &PolicyBundle,
    preset: PolicyPreset,
    force: bool,
) -> Result<bool> {
    let path = dir.join("autonomy.toml");
    if path.exists() && !force {
        return Ok(false);
    }

    let mut doc = toml::value::Table::new();
    doc.insert(
        "autonomy".to_string(),
        toml::Value::Table(bundle.autonomy.clone()),
    );
    doc.insert(
        "approvals".to_string(),
        toml::Value::Table(bundle.approvals.clone()),
    );
    let body = toml::to_string_pretty(&doc)
        .with_context(|| format!("serialise autonomy section for preset {}", preset.id()))?;
    let out = format!("{AUTONOMY_HEADER}\n{body}");
    fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// Returns `Ok(true)` if the file was freshly written, `Ok(false)` if the
/// existing file was preserved.
fn write_patterns(
    path: &Path,
    section: &str,
    patterns: &[String],
    header: &str,
    force: bool,
) -> Result<bool> {
    if path.exists() && !force {
        return Ok(false);
    }
    // Hand-roll the TOML so the test fixtures (which assert on raw
    // string presence) get a stable, easily-grep-able shape.
    let mut body = String::new();
    body.push_str(header);
    body.push('\n');
    writeln!(body, "[{section}]").expect("write to String never fails");
    if patterns.is_empty() {
        body.push_str("patterns = []\n");
    } else {
        body.push_str("patterns = [\n");
        for p in patterns {
            // escape any embedded `"` defensively even though presets don't use them
            let escaped = p.replace('\\', "\\\\").replace('"', "\\\"");
            writeln!(body, "  \"{escaped}\",").expect("write to String never fails");
        }
        body.push_str("]\n");
    }
    fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

// ── Tests (unit) ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bundles_parse() {
        for p in [
            PolicyPreset::Manual,
            PolicyPreset::Smart,
            PolicyPreset::Strict,
            PolicyPreset::Off,
        ] {
            let parsed: PolicyBundle = toml::from_str(p.bundle())
                .unwrap_or_else(|e| panic!("preset {} bundle failed to parse: {e}", p.id()));
            // sanity: every preset declares its [autonomy] section.
            assert!(
                !parsed.autonomy.is_empty(),
                "preset {} should declare [autonomy]",
                p.id()
            );
        }
    }

    #[test]
    fn id_round_trip() {
        for p in [
            PolicyPreset::Manual,
            PolicyPreset::Smart,
            PolicyPreset::Strict,
            PolicyPreset::Off,
        ] {
            assert_eq!(PolicyPreset::from_str_ci(p.id()).unwrap(), p);
        }
    }

    #[test]
    fn from_str_ci_is_case_insensitive() {
        assert_eq!(
            PolicyPreset::from_str_ci("Smart").unwrap(),
            PolicyPreset::Smart
        );
        assert_eq!(
            PolicyPreset::from_str_ci("  OFF ").unwrap(),
            PolicyPreset::Off
        );
    }

    #[test]
    fn legacy_l1_l4_ids_still_parse() {
        // Backward compat: configs written by pre-v0.6.40 releases used
        // `preset = "L1"` … `"L4"`. Reading them must still work so the
        // user does not have to hand-edit their policy/autonomy.toml.
        assert_eq!(
            PolicyPreset::from_str_ci("L1").unwrap(),
            PolicyPreset::Manual
        );
        assert_eq!(
            PolicyPreset::from_str_ci("l2").unwrap(),
            PolicyPreset::Smart
        );
        assert_eq!(
            PolicyPreset::from_str_ci("L3").unwrap(),
            PolicyPreset::Strict
        );
        assert_eq!(
            PolicyPreset::from_str_ci("  L4 ").unwrap(),
            PolicyPreset::Off
        );
    }

    #[test]
    fn full_alias_maps_to_off() {
        // `rantaiclaw autonomy full` is the natural way for users who
        // think in AutonomyLevel terms to say "no prompts". Map it to Off.
        assert_eq!(
            PolicyPreset::from_str_ci("full").unwrap(),
            PolicyPreset::Off
        );
        assert_eq!(
            PolicyPreset::from_str_ci("FULL").unwrap(),
            PolicyPreset::Off
        );
    }

    #[test]
    fn next_cycles_in_canonical_order() {
        assert_eq!(PolicyPreset::Manual.next(), PolicyPreset::Smart);
        assert_eq!(PolicyPreset::Smart.next(), PolicyPreset::Strict);
        assert_eq!(PolicyPreset::Strict.next(), PolicyPreset::Off);
        assert_eq!(PolicyPreset::Off.next(), PolicyPreset::Manual);
    }

    #[test]
    fn off_preset_maps_to_full_autonomy() {
        use crate::security::AutonomyLevel;
        // The whole point of Off: short-circuit the gate. If this
        // mapping drifts from Full, `is_command_allowed` will still
        // run its checks and Off becomes cosmetic again (v0.6.49 bug).
        assert_eq!(PolicyPreset::Off.autonomy_level(), AutonomyLevel::Full);
    }

    #[test]
    fn non_off_presets_map_to_supervised() {
        use crate::security::AutonomyLevel;
        // Manual / Smart / Strict all need the gate to actually run —
        // they differ only in their command_allowlist contents.
        assert_eq!(
            PolicyPreset::Manual.autonomy_level(),
            AutonomyLevel::Supervised
        );
        assert_eq!(
            PolicyPreset::Smart.autonomy_level(),
            AutonomyLevel::Supervised
        );
        assert_eq!(
            PolicyPreset::Strict.autonomy_level(),
            AutonomyLevel::Supervised
        );
    }

    #[test]
    fn apply_preset_to_config_updates_level() {
        use crate::config::Config;
        use crate::security::AutonomyLevel;
        let mut config = Config::default();
        // Default is Supervised — confirm the helper changes it.
        assert_eq!(config.autonomy.level, AutonomyLevel::Supervised);
        apply_preset_to_config(&mut config, PolicyPreset::Off);
        assert_eq!(config.autonomy.level, AutonomyLevel::Full);
        apply_preset_to_config(&mut config, PolicyPreset::Smart);
        assert_eq!(config.autonomy.level, AutonomyLevel::Supervised);
    }

    #[test]
    fn apply_preset_smart_populates_allowed_commands_from_bundle() {
        use crate::config::Config;
        let mut config = Config::default();
        apply_preset_to_config(&mut config, PolicyPreset::Smart);
        // The Smart bundle declares `cd`, `which`, `ls`, `git status`,
        // `curl --head *`, etc. — basenames should land in the runtime
        // allowlist. Without this bridge, the `which`/`cd`/`echo`
        // patterns I added to the bundle were never read by the gate.
        let allow = &config.autonomy.allowed_commands;
        for expected in ["cd", "which", "echo", "ls", "git", "curl", "pwd"] {
            assert!(
                allow.iter().any(|c| c == expected),
                "Smart preset should populate `{expected}` in allowed_commands, got: {allow:?}"
            );
        }
    }

    #[test]
    fn apply_preset_manual_has_minimal_allowlist() {
        use crate::config::Config;
        let mut config = Config::default();
        apply_preset_to_config(&mut config, PolicyPreset::Manual);
        // Manual preset's bundle declares an empty allowlist; the
        // bridge should leave the runtime allowed_commands empty
        // (rather than carrying over the pre-bridge hardcoded
        // defaults). Every shell call then prompts — paranoid by design.
        assert!(
            config.autonomy.allowed_commands.is_empty(),
            "Manual preset must produce an empty allowlist, got: {:?}",
            config.autonomy.allowed_commands
        );
    }

    #[test]
    fn read_active_preset_returns_none_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_active_preset(tmp.path()).is_none());
    }

    #[test]
    fn read_active_preset_round_trips_each_preset() {
        for p in PolicyPreset::ALL {
            let tmp = tempfile::tempdir().unwrap();
            let bundle: PolicyBundle = toml::from_str(p.bundle()).unwrap();
            write_autonomy(tmp.path(), &bundle, p, true).unwrap();
            assert_eq!(
                read_active_preset(tmp.path()),
                Some(p),
                "preset {} should read back from its own bundle",
                p.id()
            );
        }
    }

    #[test]
    fn from_str_ci_rejects_unknown() {
        let err = PolicyPreset::from_str_ci("paranoid").unwrap_err();
        assert!(err.to_string().contains("unknown policy preset"));
        let err2 = PolicyPreset::from_str_ci("L42").unwrap_err();
        assert!(err2.to_string().contains("unknown policy preset"));
    }

    #[test]
    fn verify_written_policy_passes_for_freshly_written_bundle() {
        // For each preset, write the files into a tempdir then re-parse
        // them. Exercises the writer + bundle templates + verifier
        // together.
        for p in [
            PolicyPreset::Manual,
            PolicyPreset::Smart,
            PolicyPreset::Strict,
            PolicyPreset::Off,
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let bundle: PolicyBundle = toml::from_str(p.bundle()).expect("bundle parses");
            write_autonomy(tmp.path(), &bundle, p, true).expect("write autonomy");
            write_patterns(
                &tmp.path().join("command_allowlist.toml"),
                "command_allowlist",
                &bundle.command_allowlist.patterns,
                ALLOWLIST_HEADER,
                true,
            )
            .expect("write allowlist");
            write_patterns(
                &tmp.path().join("forbidden_paths.toml"),
                "forbidden_paths",
                &bundle.forbidden_paths.patterns,
                FORBIDDEN_HEADER,
                true,
            )
            .expect("write forbidden_paths");

            verify_written_policy(tmp.path(), true, true, true)
                .unwrap_or_else(|e| panic!("preset {} round-trip self-check failed: {e}", p.id()));
        }
    }

    #[test]
    fn verify_written_policy_rejects_missing_autonomy_block() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Write a malformed autonomy.toml lacking the [autonomy] block.
        std::fs::write(
            tmp.path().join("autonomy.toml"),
            "# no autonomy block\n[approvals]\nmode = \"manual\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("command_allowlist.toml"),
            "[command_allowlist]\npatterns = []\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("forbidden_paths.toml"),
            "[forbidden_paths]\npatterns = []\n",
        )
        .unwrap();
        let err = verify_written_policy(tmp.path(), true, true, true).unwrap_err();
        assert!(
            err.to_string().contains("autonomy"),
            "expected autonomy-related error, got {err}"
        );
    }

    #[test]
    fn verify_written_policy_rejects_non_string_patterns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("autonomy.toml"),
            "[autonomy]\npreset = \"smart\"\n[approvals]\nmode = \"manual\"\n",
        )
        .unwrap();
        // A pattern that's a number, not a string — the gate's glob
        // matcher would crash at use site without this guard.
        std::fs::write(
            tmp.path().join("command_allowlist.toml"),
            "[command_allowlist]\npatterns = [42]\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("forbidden_paths.toml"),
            "[forbidden_paths]\npatterns = []\n",
        )
        .unwrap();
        let err = verify_written_policy(tmp.path(), true, true, true).unwrap_err();
        assert!(
            err.to_string().contains("not a string"),
            "expected pattern type error, got {err}"
        );
    }
}
