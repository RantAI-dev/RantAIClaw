//! CLI handlers for persona inspection (mirrors TUI `/personality`).

use anyhow::{bail, Result};

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

/// Set the persona of the active profile.
///
/// The preset is the positional argument; the name and the timezone are
/// optional flags. All three go through [`super::apply_update`], so a
/// timezone this build cannot use is refused here exactly as the HTTP API
/// refuses it — the file is left untouched.
pub fn set(preset: Option<PresetId>, name: Option<String>, timezone: Option<String>) -> Result<()> {
    if preset.is_none() && name.is_none() && timezone.is_none() {
        bail!("nothing to set: name a preset, or pass --name or --timezone");
    }
    let profile = ProfileManager::active()?;
    let update = super::PersonaUpdate {
        preset,
        name: name.clone(),
        timezone: timezone.clone(),
        ..Default::default()
    };
    let persona = super::apply_update(&profile, update)?;
    if let Some(preset) = preset {
        println!(
            "Persona preset set to {} for profile '{}'.",
            preset.slug(),
            profile.name
        );
    }
    if name.is_some() {
        println!(
            "Persona name set to {:?} for profile '{}'.",
            persona.name, profile.name
        );
    }
    if timezone.is_some() {
        println!(
            "Persona timezone set to {:?} for profile '{}'.",
            persona.timezone, profile.name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CLI refuses a timezone this build cannot use, exactly as the HTTP
    /// API does, and leaves the persona file untouched.
    #[test]
    fn set_refuses_an_unknown_timezone_and_writes_nothing() {
        let _guard = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::test_env::HomeGuard::set(tmp.path());

        let error = set(
            Some(PresetId::ConcisePro),
            None,
            Some("Not/AZone".to_string()),
        )
        .expect_err("an unknown timezone must be refused");
        assert!(
            error.to_string().contains("Not/AZone"),
            "the error must name the bad value, got: {error}"
        );

        let profile = ProfileManager::active().expect("active profile");
        assert!(
            !profile.persona_dir().join("persona.toml").exists(),
            "the refused update must not write a persona file"
        );
    }

    /// An IANA name is accepted and stored.
    #[test]
    fn set_stores_an_iana_timezone() {
        let _guard = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::test_env::HomeGuard::set(tmp.path());

        set(
            Some(PresetId::ConcisePro),
            None,
            Some("Asia/Jakarta".to_string()),
        )
        .expect("an IANA name is accepted");

        let profile = ProfileManager::active().expect("active profile");
        let persona = read_persona_toml(&profile)
            .unwrap()
            .expect("a persona file");
        assert_eq!(persona.timezone, "Asia/Jakarta");
        assert_eq!(persona.preset, PresetId::ConcisePro);
    }

    /// The empty value means "unset" and stays allowed.
    #[test]
    fn set_accepts_an_empty_timezone() {
        let _guard = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::test_env::HomeGuard::set(tmp.path());

        set(Some(PresetId::ConcisePro), None, Some(String::new()))
            .expect("an empty timezone means unset");

        let profile = ProfileManager::active().expect("active profile");
        let persona = read_persona_toml(&profile)
            .unwrap()
            .expect("a persona file");
        assert_eq!(persona.timezone, "");
    }
}
