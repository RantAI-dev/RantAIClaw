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
    fs::create_dir_all(&dir)
        .with_context(|| format!("create policy dir {}", dir.display()))?;

    let bundle: PolicyBundle = toml::from_str(preset.bundle()).with_context(|| {
        format!(
            "parse bundled preset {} (this is a build-time error)",
            preset.id()
        )
    })?;

    write_autonomy(&dir, &bundle, preset, force)?;
    write_patterns(
        &dir.join("command_allowlist.toml"),
        "command_allowlist",
        &bundle.command_allowlist.patterns,
        ALLOWLIST_HEADER,
        force,
    )?;
    write_patterns(
        &dir.join("forbidden_paths.toml"),
        "forbidden_paths",
        &bundle.forbidden_paths.patterns,
        FORBIDDEN_HEADER,
        force,
    )?;

    if matches!(preset, PolicyPreset::L4Off) {
        eprintln!(
            "{}",
            "⚠️  approval policy preset L4 selected — gating is OFF.\n   \
             Every tool call will execute without prompts. Use this only in \
             trusted CI environments. To revert: `rantaiclaw setup approvals --force`."
        );
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

fn write_autonomy(
    dir: &Path,
    bundle: &PolicyBundle,
    preset: PolicyPreset,
    force: bool,
) -> Result<()> {
    let path = dir.join("autonomy.toml");
    if path.exists() && !force {
        return Ok(());
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
    let body = toml::to_string_pretty(&doc).with_context(|| {
        format!("serialise autonomy section for preset {}", preset.id())
    })?;
    let out = format!("{AUTONOMY_HEADER}\n{body}");
    fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn write_patterns(
    path: &Path,
    section: &str,
    patterns: &[String],
    header: &str,
    force: bool,
) -> Result<()> {
    if path.exists() && !force {
        return Ok(());
    }
    // Hand-roll the TOML so the test fixtures (which assert on raw
    // string presence) get a stable, easily-grep-able shape.
    let mut body = String::new();
    body.push_str(header);
    body.push('\n');
    body.push_str(&format!("[{section}]\n"));
    if patterns.is_empty() {
        body.push_str("patterns = []\n");
    } else {
        body.push_str("patterns = [\n");
        for p in patterns {
            // escape any embedded `"` defensively even though presets don't use them
            let escaped = p.replace('\\', "\\\\").replace('"', "\\\"");
            body.push_str(&format!("  \"{escaped}\",\n"));
        }
        body.push_str("]\n");
    }
    fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
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
}
