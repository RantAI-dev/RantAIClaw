//! Persona setup section — implements the Wave-2 stub trait so Wave 3 can
//! adapt it to the real `SetupSection` trait without re-engineering the
//! body. Behaviour mirrors §"Section 3 — persona (NEW)" of the design spec.
//!
//! On `run`:
//!   1. Compute defaults (`name`, `timezone`) from existing config when
//!      available; fall back to `""` and `"UTC"`.
//!   2. If interactive, run the 4-question interview; otherwise build a
//!      `PersonaToml::default_for(...)`.
//!   3. Persist `persona.toml` and render `SYSTEM.md`.
//!
//! `is_already_configured` is a presence check on `persona/persona.toml`.

use anyhow::Result;

use crate::config::Config;
use crate::onboard::section::{SetupContext, SetupSection};
use crate::persona::{self, render_system_md, write_persona_toml, PersonaToml, PresetId};
use crate::profile::Profile;

pub struct PersonaSection;

impl SetupSection for PersonaSection {
    fn name(&self) -> &'static str {
        "persona"
    }

    fn description(&self) -> &'static str {
        "Agent personality (preset + interview)"
    }

    /// Configured iff the persona file already exists on disk. Wave 3's
    /// orchestrator surfaces this as the `[skip / reconfigure / show]`
    /// branch.
    fn is_already_configured(&self, profile: &Profile, _config: &Config) -> bool {
        profile.persona_dir().join("persona.toml").exists()
    }

    fn run(&self, ctx: &mut SetupContext) -> Result<()> {
        let (default_name, default_tz) = derive_defaults(ctx.config);

        let persona_record: PersonaToml = if ctx.interactive {
            persona::interview::run_interactive(&default_name, &default_tz)?
        } else {
            // Headless fallback: pure default preset, no avoid, neutral tone.
            // Wave 3's `Commands::Setup { topic = persona }` branch will add
            // `--persona-preset`, `--role`, `--tone` flags that override these.
            let mut p = PersonaToml::default_for(&default_name, &default_tz);
            p.preset = PresetId::Default;
            p
        };

        write_persona_toml(ctx.profile, &persona_record)?;
        render_system_md(ctx.profile, &persona_record)?;
        Ok(())
    }

    fn headless_hint(&self) -> &'static str {
        "rantaiclaw setup persona --preset <id> --role <text> --tone <formal|neutral|casual>"
    }
}

/// Pull `name` and `timezone` out of `Config` when those fields exist;
/// otherwise return `("", "UTC")`. The shape of `Config` is owned by the
/// config crate and may have moved; we only need string-ish accessors so
/// keep this loose and recover via defaults.
fn derive_defaults(_config: &Config) -> (String, String) {
    // Wave 3 will replace this with proper project-context lookups from
    // `Config::project_context.{name, timezone}`. For now we use the
    // sensible fallbacks the spec lists — the project-context section will
    // re-render `SYSTEM.md` once those fields exist anyway.
    (String::new(), "UTC".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::profile::ProfileManager;
    use tempfile::TempDir;

    #[test]
    fn is_already_configured_flips_after_write() {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("RANTAICLAW_PROFILE");
        let profile = ProfileManager::ensure("rt-iac-section").unwrap();
        let config = Config::default();

        let section = PersonaSection;
        assert!(!section.is_already_configured(&profile, &config));

        let persona = PersonaToml::default_for("Shiro", "Asia/Jakarta");
        persona::write_persona_toml(&profile, &persona).unwrap();

        assert!(section.is_already_configured(&profile, &config));
    }

    #[test]
    fn name_and_description_are_stable() {
        let s = PersonaSection;
        assert_eq!(s.name(), "persona");
        assert!(!s.description().is_empty());
        assert!(s.headless_hint().contains("--preset"));
    }
}
