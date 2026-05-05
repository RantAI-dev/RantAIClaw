//! Persona module — preset registry, template renderer, interview flow,
//! and `persona.toml` + `SYSTEM.md` writers.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Persona" in §"Components", and §"Section 3 — persona (NEW)" in §5.
//!
//! Five hand-written persona markdown templates ship in-binary via
//! `include_str!`. Each one carries five substitution placeholders
//! (`{{name}}`, `{{timezone}}`, `{{role}}`, `{{tone}}`, `{{avoid}}`) plus
//! one `{{#if avoid}}...{{/if}}` block guard. The renderer is pure
//! substring replacement — no templating engine — so the binary stays
//! lean and snapshot tests are trivially deterministic.
//!
//! Wave 3 will wire `PersonaSection` into the orchestrator; Wave 2C only
//! exposes the data, the renderer, the interview, and the section module
//! against the stub trait.

pub mod interview;
pub mod renderer;

use std::fs;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::profile::Profile;

/// Five general-assistant persona presets. **No coding-flavored persona** —
/// general assistant only per maintainer's earlier explicit feedback.
///
/// `clap::ValueEnum` so the future `--persona-preset <id>` CLI flag (added
/// when Wave 3 wires up `Commands::Setup { topic = persona }`) maps strings
/// to variants. `serde(rename_all = "snake_case")` matches the on-disk
/// representation in `persona/persona.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum PresetId {
    Default,
    ConcisePro,
    FriendlyCompanion,
    ResearchAnalyst,
    ExecutiveAssistant,
}

impl PresetId {
    /// Stable ordering used by the interview picker and by tests. Update
    /// alongside the `clap::ValueEnum` variant ordering above.
    pub const ALL: &'static [PresetId] = &[
        PresetId::Default,
        PresetId::ConcisePro,
        PresetId::FriendlyCompanion,
        PresetId::ResearchAnalyst,
        PresetId::ExecutiveAssistant,
    ];

    /// Stable string slug used in `persona.toml` and `--persona-preset`.
    pub fn slug(self) -> &'static str {
        match self {
            PresetId::Default => "default",
            PresetId::ConcisePro => "concise_pro",
            PresetId::FriendlyCompanion => "friendly_companion",
            PresetId::ResearchAnalyst => "research_analyst",
            PresetId::ExecutiveAssistant => "executive_assistant",
        }
    }

    /// One-line description shown in the interview preset picker.
    pub fn description(self) -> &'static str {
        match self {
            PresetId::Default => "Balanced general-purpose helper. Good if you're not sure.",
            PresetId::ConcisePro => {
                "Short, formal, lead-with-the-answer. For busy professional use."
            }
            PresetId::FriendlyCompanion => "Warm and conversational. Good for daily-life support.",
            PresetId::ResearchAnalyst => "Evidence-driven, cites sources, flags uncertainty.",
            PresetId::ExecutiveAssistant => {
                "Anticipatory, time-conscious, drafts ready-to-send replies."
            }
        }
    }
}

/// Resolve a preset id to its embedded markdown template body.
///
/// All five templates are compiled into the binary via `include_str!` so a
/// stripped/relocated install can never end up with a missing template
/// file at run time.
pub fn template_for(id: PresetId) -> &'static str {
    match id {
        PresetId::Default => include_str!("presets/default.md"),
        PresetId::ConcisePro => include_str!("presets/concise_pro.md"),
        PresetId::FriendlyCompanion => include_str!("presets/friendly_companion.md"),
        PresetId::ResearchAnalyst => include_str!("presets/research_analyst.md"),
        PresetId::ExecutiveAssistant => include_str!("presets/executive_assistant.md"),
    }
}

/// One-liner for the given preset. Mirrors `PresetId::description` but kept
/// at module level for symmetry with the spec's exposed API.
pub fn description(id: PresetId) -> &'static str {
    id.description()
}

/// On-disk persona record at `persona/persona.toml`. The `SYSTEM.md` file
/// alongside is the rendered output and is regenerated whenever this struct
/// changes (e.g. after the project-context section updates `name` or
/// `timezone`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersonaToml {
    pub preset: PresetId,
    pub name: String,
    pub timezone: String,
    pub role: String,
    pub tone: String,
    pub avoid: Option<String>,
}

impl PersonaToml {
    /// Sensible non-interactive defaults used by `setup persona` in headless
    /// mode and by the `is_already_configured == false` branch when no
    /// project context is available.
    pub fn default_for(name: &str, timezone: &str) -> Self {
        Self {
            preset: PresetId::Default,
            name: name.to_string(),
            timezone: timezone.to_string(),
            role: "general productivity and helpful assistance".to_string(),
            tone: "neutral".to_string(),
            avoid: None,
        }
    }

    /// Render the rendered SYSTEM.md body without touching the filesystem.
    /// Used by tests and by `render_system_md`.
    pub fn render(&self) -> String {
        renderer::render(
            template_for(self.preset),
            &self.name,
            &self.timezone,
            &self.role,
            &self.tone,
            self.avoid.as_deref(),
        )
    }
}

/// Persist `persona.toml` for the given profile. Creates the persona
/// directory if missing.
pub fn write_persona_toml(profile: &Profile, persona: &PersonaToml) -> Result<()> {
    let dir = profile.persona_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create persona dir {}", dir.display()))?;
    let path = dir.join("persona.toml");
    let body = toml::to_string_pretty(persona).context("serialize persona.toml")?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Read `persona.toml` if it exists. Returns `Ok(None)` when the file is
/// missing — callers treat that as "not yet configured".
pub fn read_persona_toml(profile: &Profile) -> Result<Option<PersonaToml>> {
    let path = profile.persona_dir().join("persona.toml");
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let persona: PersonaToml =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(persona))
}

/// Render `persona/SYSTEM.md` from the persona record. Overwrites any
/// existing file. Project-context section calls this directly after
/// updating `name` or `timezone` so SYSTEM.md never drifts.
pub fn render_system_md(profile: &Profile, persona: &PersonaToml) -> Result<()> {
    let dir = profile.persona_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create persona dir {}", dir.display()))?;
    let path = dir.join("SYSTEM.md");
    let body = persona.render();
    let tmp = path.with_extension("md.tmp");
    fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_unique_and_stable() {
        let mut slugs: Vec<&'static str> = PresetId::ALL.iter().map(|p| p.slug()).collect();
        slugs.sort_unstable();
        let n = slugs.len();
        slugs.dedup();
        assert_eq!(n, slugs.len(), "preset slugs must be unique");
    }

    #[test]
    fn every_preset_has_nonempty_template_and_description() {
        for &p in PresetId::ALL {
            assert!(!template_for(p).is_empty(), "{:?} template empty", p);
            assert!(!p.description().is_empty(), "{:?} description empty", p);
        }
    }

    #[test]
    fn default_for_round_trips_through_toml() {
        let persona = PersonaToml::default_for("Shiro", "Asia/Jakarta");
        let body = toml::to_string_pretty(&persona).unwrap();
        let back: PersonaToml = toml::from_str(&body).unwrap();
        assert_eq!(persona, back);
    }
}
