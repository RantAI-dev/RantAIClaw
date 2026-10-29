//! Persona module — preset registry, template renderer, interview flow,
//! and the `persona.toml` writer.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Persona" in §"Components", and §"Section 3 — persona (NEW)" in §5.
//!
//! Five hand-written persona markdown templates ship in-binary via
//! `include_str!`. Each one carries five substitution placeholders
//! (`{{name}}`, `{{timezone}}`, `{{role}}`, `{{tone}}`, `{{avoid}}`) plus
//! two block guards, `{{#if avoid}}...{{/if}}` and
//! `{{#if timezone}}...{{/if}}`. The renderer is pure substring
//! replacement — no templating engine — so the binary stays lean and
//! snapshot tests are trivially deterministic.
//!
//! `PersonaSection` is wired into the orchestrator prompt builder, so a
//! configured persona shapes the agent's voice on every surface: TUI,
//! `agent -m`, gateway chat, and channels (spliced per message).

pub mod cli;
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

    /// Decode a slug back into a preset. The single source the API handler, the
    /// CLI, and the TUI share so a new preset added to the enum is reachable
    /// everywhere at once (previously each hardcoded its own list).
    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.slug() == s)
    }

    /// Human-friendly label for a preset, for pickers and API listings.
    pub fn label(self) -> &'static str {
        match self {
            PresetId::Default => "Default",
            PresetId::ConcisePro => "Concise Pro",
            PresetId::FriendlyCompanion => "Friendly Companion",
            PresetId::ResearchAnalyst => "Research Analyst",
            PresetId::ExecutiveAssistant => "Executive Assistant",
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

/// On-disk persona record at `persona/persona.toml` — the single source of
/// truth for the persona. Rendered to the `## Persona` prompt section on the
/// fly by `agent::prompt::render_persona_section`; there is no separate
/// rendered file on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersonaToml {
    pub preset: PresetId,
    pub name: String,
    pub timezone: String,
    pub role: String,
    pub tone: String,
    pub avoid: Option<String>,
    /// KB group ids whose documents are always retrieved for this persona,
    /// regardless of per-query group selection. `#[serde(default)]` keeps
    /// older `persona.toml` files (written before this field existed)
    /// parseable.
    #[serde(default)]
    pub always_on_kbs: Vec<String>,
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
            always_on_kbs: Vec::new(),
        }
    }

    /// Render the persona body (the `## Persona` prompt-section content)
    /// without touching the filesystem.
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

    /// Render the persona body for a guest turn: the owner's name is
    /// replaced by "the user" and the timezone is omitted, so neither reaches
    /// a non-owner sender. The templates use the name for the person in the
    /// chat ("mirror {{name}}'s energy"), and for a guest that person is the
    /// user, not the owner. Role, tone and avoid are unchanged. The literal
    /// replacement name is passed explicitly rather than an empty string,
    /// since the renderer maps an empty name to "you".
    pub fn render_for_guest(&self) -> String {
        renderer::render(
            template_for(self.preset),
            "the user",
            "",
            &self.role,
            &self.tone,
            self.avoid.as_deref(),
        )
    }
}

/// A timezone name that this build cannot use.
///
/// The empty value means "unset" and is never this error. The type is
/// separate from an I/O failure so a door that knows the difference (the HTTP
/// API answering `400`, the CLI answering "not a timezone") can tell them
/// apart instead of reporting every write problem as a server error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownTimezone {
    /// The rejected value, exactly as it was written.
    pub value: String,
}

impl std::fmt::Display for UnknownTimezone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown timezone {:?}: use an IANA name such as `Asia/Jakarta`",
            self.value
        )
    }
}

impl std::error::Error for UnknownTimezone {}

/// Whether `tz` is a timezone this build can use.
///
/// The empty value means "unset" and stays allowed. Any other value must be
/// an IANA name the timezone database holds (`Europe/Berlin`, `UTC`, …): the
/// value is what a timestamp is rendered in, so a name that cannot be parsed
/// is dead weight in the file. Surrounding spaces are ignored, so the value
/// that is stored is the trimmed one.
pub fn validate_timezone(tz: &str) -> Result<(), UnknownTimezone> {
    let trimmed = tz.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if trimmed.parse::<chrono_tz::Tz>().is_err() {
        return Err(UnknownTimezone {
            value: tz.to_string(),
        });
    }
    Ok(())
}

/// The timezone value to store: trimmed, so a stray space cannot make a name
/// unparseable for the reader that comes later.
fn stored_timezone(tz: &str) -> String {
    tz.trim().to_string()
}

/// A partial persona edit. `None` leaves the current value unchanged. `avoid`
/// uses `Some(None)` to clear the block and `Some(Some(text))` to set it, so a
/// partial update can distinguish "leave" from "clear".
#[derive(Debug, Default, Clone)]
pub struct PersonaUpdate {
    pub preset: Option<PresetId>,
    pub name: Option<String>,
    pub timezone: Option<String>,
    pub role: Option<String>,
    pub tone: Option<String>,
    pub avoid: Option<Option<String>>,
    pub always_on_kbs: Option<Vec<String>>,
}

/// The one read → default → merge → write sequence shared by the HTTP API and
/// the CLI, so persona edits behave identically everywhere. Returns the merged
/// persona that was persisted.
pub fn apply_update(profile: &Profile, up: PersonaUpdate) -> Result<PersonaToml> {
    let existing = read_persona_toml(profile)?;
    let mut next = existing
        .clone()
        .unwrap_or_else(|| PersonaToml::default_for("RantaiClaw", "UTC"));
    if let Some(p) = up.preset {
        next.preset = p;
    }
    if let Some(n) = up.name {
        next.name = n;
    }
    if let Some(tz) = up.timezone {
        // Refuse a timezone this build cannot use, before anything is
        // written: every caller of this function shares the rule, so no door
        // can store a name that is not in the timezone database.
        validate_timezone(&tz)?;
        next.timezone = stored_timezone(&tz);
    }
    if let Some(r) = up.role {
        next.role = r;
    }
    if let Some(t) = up.tone {
        next.tone = t;
    }
    if let Some(a) = up.avoid {
        // Empty/whitespace clears the block (the renderer treats blank as none).
        next.avoid = a.filter(|s| !s.trim().is_empty());
    }
    if let Some(k) = up.always_on_kbs {
        next.always_on_kbs = k;
    }
    write_persona_toml(profile, &next)?;
    Ok(next)
}

/// Persist `persona.toml` for the given profile. Creates the persona
/// directory if missing.
pub fn write_persona_toml(profile: &Profile, persona: &PersonaToml) -> Result<()> {
    // The one door that writes `persona.toml`: a timezone the timezone
    // database does not hold is refused here, so a caller that builds a
    // persona itself (the onboarding section, the provisioner) cannot store
    // one either.
    validate_timezone(&persona.timezone)?;
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
    let mut persona: PersonaToml =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    // A hand-edited file can hold a timezone this build cannot use. It reads
    // as "unset" — the file itself is left as the operator wrote it — and the
    // operator is told once, so neither `personality show` nor the prompt
    // renderer ever carries a name that cannot be parsed.
    if validate_timezone(&persona.timezone).is_err() {
        warn_unknown_timezone_once(&persona.timezone, &path);
        persona.timezone = String::new();
    }
    Ok(Some(persona))
}

/// Warn about a timezone this build cannot use, once per distinct value.
///
/// The prompt renderer reads the persona on every turn, so an unconditional
/// warning would repeat forever. The file is never rewritten from here.
fn warn_unknown_timezone_once(value: &str, path: &std::path::Path) {
    use std::sync::Mutex;
    static WARNED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut warned = WARNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if warned.iter().any(|seen| seen == value) {
        return;
    }
    warned.push(value.to_string());
    tracing::warn!(
        "Persona timezone {value:?} in {} is not an IANA name this build knows; treating it \
         as unset. The file is left unchanged — set a usable one with \
         `rantaiclaw personality set --timezone <IANA name>`.",
        path.display()
    );
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

    /// A persona reaches every prompt, a guest's included. Its workspace line
    /// sends files the person asks for to the workspace, and does not steer a
    /// note the person wants remembered into a file, and it names no tool.
    #[test]
    fn every_preset_sends_only_requested_files_to_the_workspace() {
        for &preset in PresetId::ALL {
            let line = template_for(preset)
                .lines()
                .find(|line| line.contains("profiles/<active>/workspace/"))
                .unwrap_or_else(|| panic!("{preset:?} has no workspace line"));
            assert!(
                line.contains("the person asks you to create"),
                "{preset:?} workspace line: {line}"
            );
            for steering in ["persistent files", "notes"] {
                assert!(
                    !line.contains(steering),
                    "{preset:?} workspace line steers a note into a file ({steering:?}): {line}"
                );
            }
            assert!(
                !line.contains("memory_"),
                "{preset:?} workspace line names a memory tool: {line}"
            );
        }
    }

    #[test]
    fn default_for_round_trips_through_toml() {
        let persona = PersonaToml::default_for("Shiro", "Asia/Jakarta");
        let body = toml::to_string_pretty(&persona).unwrap();
        let back: PersonaToml = toml::from_str(&body).unwrap();
        assert_eq!(persona, back);
    }

    /// The persona's timezone is the zone a timestamp is rendered in, so only
    /// a name the timezone database holds may be written. The empty value
    /// means "unset" and stays allowed.
    #[test]
    fn validate_timezone_accepts_iana_names_and_refuses_the_rest() {
        for good in ["", "UTC", "Asia/Jakarta", "Europe/Berlin", "US/Eastern"] {
            assert!(
                validate_timezone(good).is_ok(),
                "{good:?} is a timezone this build knows"
            );
        }
        for bad in ["Not/AZone", "utc", "GMT+7", "Asia/Jakarta/Extra"] {
            let error = validate_timezone(bad).expect_err("a name that is not a timezone");
            assert_eq!(error.value, bad, "the error must name the value");
            assert!(
                error.to_string().contains(bad),
                "the message must name the value, got: {error}"
            );
        }
    }

    /// A timezone this build cannot use is refused before anything is
    /// written, so the persona file keeps the value it had.
    #[test]
    fn apply_update_refuses_an_unknown_timezone_and_writes_nothing() {
        let _guard = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let profile = crate::profile::ProfileManager::active().expect("active profile");

        let error = apply_update(
            &profile,
            PersonaUpdate {
                timezone: Some("Not/AZone".to_string()),
                ..Default::default()
            },
        )
        .expect_err("an unknown timezone must be refused");
        assert!(
            error.downcast_ref::<UnknownTimezone>().is_some(),
            "the error must be the typed one, got: {error}"
        );
        assert!(
            !profile.persona_dir().join("persona.toml").exists(),
            "the refused update must not write a persona file"
        );
    }

    /// The one door that writes `persona.toml` refuses a timezone this build
    /// cannot use, so a caller that builds a persona itself cannot store one
    /// either.
    #[test]
    fn write_persona_toml_refuses_an_unknown_timezone() {
        let _guard = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let profile = crate::profile::ProfileManager::active().expect("active profile");

        let mut persona = PersonaToml::default_for("RantaiClaw", "UTC");
        persona.timezone = "Not/AZone".to_string();

        let error = write_persona_toml(&profile, &persona)
            .expect_err("an unknown timezone must be refused");
        assert!(
            error.downcast_ref::<UnknownTimezone>().is_some(),
            "the error must be the typed one, got: {error}"
        );
        assert!(
            !profile.persona_dir().join("persona.toml").exists(),
            "the refused write must not create a persona file"
        );
    }

    /// A hand-edited `persona.toml` can hold a timezone this build cannot
    /// use. It reads as "unset", the operator is told once, and the file is
    /// left exactly as they wrote it.
    #[test]
    fn a_hand_edited_unknown_timezone_reads_as_unset_and_leaves_the_file_alone() {
        let _guard = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let profile = crate::profile::ProfileManager::active().expect("active profile");

        let dir = profile.persona_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("persona.toml");
        let hand_edited = "preset = \"default\"\n\
             name = \"RantaiClaw\"\n\
             timezone = \"Not/AZone\"\n\
             role = \"a role\"\n\
             tone = \"neutral\"\n";
        std::fs::write(&path, hand_edited).unwrap();

        let persona = read_persona_toml(&profile)
            .unwrap()
            .expect("a persona file");
        assert_eq!(persona.timezone, "", "an unusable timezone reads as unset");
        assert_eq!(persona.name, "RantaiClaw", "the other fields are untouched");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            hand_edited,
            "the file must not be rewritten"
        );
    }
}
