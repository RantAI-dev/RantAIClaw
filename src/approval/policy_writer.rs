//! Approval policy bootstrap — materialises the three on-disk policy
//! files (`autonomy.toml`, `command_allowlist.toml`, `forbidden_paths.toml`)
//! under `<profile>/policy/` from a chosen `PolicyPreset` (L1-L4).
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §6 "Approval runtime" + §"L1-L4 presets".
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
//! The L4 ("off") preset prints a stern stderr warning whenever it is
//! selected — the spec is explicit that L4 is for trusted CI only.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::profile::Profile;

// ── Preset bundles (compiled into the binary) ───────────────────

const L1_BUNDLE: &str = include_str!("presets/policy_l1.toml");
const L2_BUNDLE: &str = include_str!("presets/policy_l2.toml");
const L3_BUNDLE: &str = include_str!("presets/policy_l3.toml");
const L4_BUNDLE: &str = include_str!("presets/policy_l4.toml");

// ── PolicyPreset enum ───────────────────────────────────────────

/// One of the four canonical approval-policy presets.
///
/// The variants are deliberately spelt out (rather than `enum_iterator`-
/// style) so callers can pattern-match exhaustively and so the spec's
/// "L1Manual / L2Smart / L3Strict / L4Off" naming survives as code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyPreset {
    /// L1 — paranoid manual mode. Empty allowlist, every tool prompts.
    L1Manual,
    /// L2 — smart safe-by-default. Read-only commands pre-allowed.
    L2Smart,
    /// L3 — deny-by-default strict mode. For unattended agents.
    L3Strict,
    /// L4 — gating disabled. Trusted CI environments only.
    L4Off,
}

impl PolicyPreset {
    /// Stable identifier (`"L1"` … `"L4"`) used in TOML and CLI flags.
    pub fn id(self) -> &'static str {
        match self {
            Self::L1Manual => "L1",
            Self::L2Smart => "L2",
            Self::L3Strict => "L3",
            Self::L4Off => "L4",
        }
    }

    /// Parse a case-insensitive preset id (`"L1"`, `"l3"`, …).
    pub fn from_str_ci(s: &str) -> Result<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "L1" => Ok(Self::L1Manual),
            "L2" => Ok(Self::L2Smart),
            "L3" => Ok(Self::L3Strict),
            "L4" => Ok(Self::L4Off),
            other => Err(anyhow!(
                "unknown policy preset '{other}' (valid: L1, L2, L3, L4)"
            )),
        }
    }

    /// Raw bundle TOML for this preset (parsed by `write_policy_files`).
    fn bundle(self) -> &'static str {
        match self {
            Self::L1Manual => L1_BUNDLE,
            Self::L2Smart => L2_BUNDLE,
            Self::L3Strict => L3_BUNDLE,
            Self::L4Off => L4_BUNDLE,
        }
    }
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

/// Write the three policy files for `preset` into `profile.policy_dir()`.
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
pub fn write_policy_files(profile: &Profile, preset: PolicyPreset, force: bool) -> Result<()> {
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

    if matches!(preset, PolicyPreset::L4Off) {
        eprintln!(
            "⚠️  approval policy preset L4 selected — gating is OFF.\n   \
             Every tool call will execute without prompts. Use this only in \
             trusted CI environments. To revert: `rantaiclaw setup approvals --force`."
        );
    }

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

    Ok(())
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
# RantaiClaw L1-L4 preset; safe to hand-edit. Re-running
# `rantaiclaw setup approvals` without --force will leave this file
# alone.
";

const ALLOWLIST_HEADER: &str = "\
# Command allowlist — globs of `<tool> <args>` shape that the approval
# gate auto-approves. Accreted entries from `[a]lways` prompts append
# here over time. Comments are preserved on round-trip via toml_edit.
";

const FORBIDDEN_HEADER: &str = "\
# Forbidden paths — never-allow globs checked first by the approval
# gate. No allowlist entry, /yolo toggle, or L4 setting can override
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
            PolicyPreset::L1Manual,
            PolicyPreset::L2Smart,
            PolicyPreset::L3Strict,
            PolicyPreset::L4Off,
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
            PolicyPreset::L1Manual,
            PolicyPreset::L2Smart,
            PolicyPreset::L3Strict,
            PolicyPreset::L4Off,
        ] {
            assert_eq!(PolicyPreset::from_str_ci(p.id()).unwrap(), p);
        }
    }

    #[test]
    fn from_str_ci_is_case_insensitive() {
        assert_eq!(
            PolicyPreset::from_str_ci("l2").unwrap(),
            PolicyPreset::L2Smart
        );
        assert_eq!(
            PolicyPreset::from_str_ci("  L4 ").unwrap(),
            PolicyPreset::L4Off
        );
    }

    #[test]
    fn from_str_ci_rejects_unknown() {
        let err = PolicyPreset::from_str_ci("L42").unwrap_err();
        assert!(err.to_string().contains("unknown policy preset"));
    }

    #[test]
    fn verify_written_policy_passes_for_freshly_written_bundle() {
        // For each L1-L4 preset, write the files into a tempdir then
        // re-parse them. Exercises the writer + bundle templates +
        // verifier together.
        for p in [
            PolicyPreset::L1Manual,
            PolicyPreset::L2Smart,
            PolicyPreset::L3Strict,
            PolicyPreset::L4Off,
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
            "[autonomy]\nlevel = \"l2\"\n[approvals]\nmode = \"manual\"\n",
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
