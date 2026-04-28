//! Persona interview — pipe-safe interactive prompts for the persona section.
//!
//! Asks four short questions:
//!   1. Pick a preset (5 options).
//!   2. Primary role for this agent (one sentence).
//!   3. Tone (`formal` / `neutral` / `casual`).
//!   4. Anything to avoid (optional free text).
//!
//! Headless callers should use `PersonaToml::default_for` instead of this
//! module — `run_interactive` requires a TTY (it shells out to dialoguer's
//! interactive widgets).

use anyhow::{Context, Result};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Select};

use super::{PersonaToml, PresetId};

/// Run the persona interview and return a populated `PersonaToml`.
///
/// `default_name` and `default_timezone` come from the project-context
/// section (or are sensible fallbacks: `""` and `"UTC"`).
pub fn run_interactive(default_name: &str, default_timezone: &str) -> Result<PersonaToml> {
    let preset = prompt_preset_picker()?;
    let role = prompt_role()?;
    let tone = prompt_tone()?;
    let avoid = prompt_avoid()?;

    Ok(PersonaToml {
        preset,
        name: default_name.to_string(),
        timezone: default_timezone.to_string(),
        role,
        tone,
        avoid,
    })
}

/// Render the 5-preset picker. Each option shows the preset slug followed
/// by its one-liner description.
fn prompt_preset_picker() -> Result<PresetId> {
    let theme = ColorfulTheme::default();
    let items: Vec<String> = PresetId::ALL
        .iter()
        .map(|p| format!("{} — {}", p.slug(), p.description()))
        .collect();
    let idx = Select::with_theme(&theme)
        .with_prompt("Pick a persona preset")
        .items(&items)
        .default(0)
        .interact()
        .context("preset picker prompt")?;
    Ok(PresetId::ALL[idx])
}

fn prompt_role() -> Result<String> {
    let theme = ColorfulTheme::default();
    let role: String = Input::with_theme(&theme)
        .with_prompt("Primary role for this agent (one sentence)")
        .default("general productivity and helpful assistance".to_string())
        .interact_text()
        .context("role prompt")?;
    Ok(role.trim().to_string())
}

fn prompt_tone() -> Result<String> {
    let theme = ColorfulTheme::default();
    let tones = ["formal", "neutral", "casual"];
    let idx = Select::with_theme(&theme)
        .with_prompt("Tone")
        .items(tones.as_slice())
        .default(1)
        .interact()
        .context("tone prompt")?;
    Ok(tones[idx].to_string())
}

fn prompt_avoid() -> Result<Option<String>> {
    let theme = ColorfulTheme::default();
    // `allow_empty(true)` lets the user just press Enter to skip.
    let raw: String = Input::with_theme(&theme)
        .with_prompt("Anything to avoid? (Enter to skip)")
        .allow_empty(true)
        .interact_text()
        .context("avoid prompt")?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}
