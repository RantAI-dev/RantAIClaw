//! CLI handlers for persona inspection (mirrors TUI `/personality`).

use anyhow::Result;

use crate::profile::ProfileManager;

use super::{read_persona_toml, PresetId};

pub fn show() -> Result<()> {
    let profile = ProfileManager::active()?;
    match read_persona_toml(&profile)? {
        Some(p) => {
            println!("Active persona ({}):", profile.name);
            println!("  preset:   {}", p.preset.slug());
            println!("  name:     {}", p.name);
            println!("  timezone: {}", p.timezone);
            println!("  role:     {}", p.role);
            println!("  tone:     {}", p.tone);
            if let Some(avoid) = &p.avoid {
                println!("  avoid:    {avoid}");
            }
        }
        None => {
            println!(
                "No persona configured for profile '{}'. Run `rantaiclaw setup persona`.",
                profile.name
            );
        }
    }
    Ok(())
}

pub fn list() -> Result<()> {
    println!("Persona presets:");
    println!();
    for id in PresetId::ALL {
        println!("  {:<22} {}", id.slug(), id.description());
    }
    Ok(())
}

pub fn set(preset: PresetId) -> Result<()> {
    let profile = ProfileManager::active()?;
    super::apply_update(
        &profile,
        super::PersonaUpdate {
            preset: Some(preset),
            ..Default::default()
        },
    )?;
    println!(
        "Persona preset set to {} for profile '{}'.",
        preset.slug(),
        profile.name
    );
    Ok(())
}
