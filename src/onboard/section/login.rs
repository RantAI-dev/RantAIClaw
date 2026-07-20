//! Console login setup section — captures a single operator's username +
//! argon2 password hash into `config.gateway.login`, gating the web console and
//! the TUI. Enabled by the presence of `password_hash` (mirrors the KB section).
//!
//! On `run`:
//!   * if interactive — confirm enablement, then capture username + a
//!     double-entered password (argon2-hashed);
//!   * if headless — no-op (the wizard's final `config.save()` still runs).
//!
//! This section never calls `config.save()` — the orchestrator persists once at
//! the end of the run. Declining (or a prior credential + "no") clears the
//! stored credential, turning the gate off.

use anyhow::Result;

use super::{SetupContext, SetupSection};
use crate::config::Config;
use crate::profile::Profile;

pub struct LoginSection;

impl SetupSection for LoginSection {
    fn name(&self) -> &'static str {
        "login"
    }

    fn description(&self) -> &'static str {
        "Console login — username + password gate for the web console & TUI"
    }

    /// Configured iff a password hash is stored in `config.gateway.login`.
    fn is_already_configured(&self, _profile: &Profile, config: &Config) -> bool {
        config
            .gateway
            .login
            .password_hash
            .as_deref()
            .is_some_and(|h| !h.trim().is_empty())
    }

    fn run(&self, ctx: &mut SetupContext) -> Result<()> {
        use dialoguer::theme::ColorfulTheme;
        use dialoguer::{Confirm, Input, Password, Select};

        if !ctx.interactive {
            return Ok(());
        }
        let theme = ColorfulTheme::default();
        let already = self.is_already_configured(ctx.profile, ctx.config);

        let enable = Confirm::with_theme(&theme)
            .with_prompt("Enable web console & TUI login (username + password)?")
            .default(already)
            .interact()?;
        if !enable {
            // Turn the gate off by clearing any stored credential, and drop the
            // auto-lock window with it — it is meaningless with no credential.
            ctx.config.gateway.login.username = None;
            ctx.config.gateway.login.password_hash = None;
            ctx.config.gateway.login.idle_timeout_secs = 0;
            return Ok(());
        }

        let username: String = Input::with_theme(&theme)
            .with_prompt("Console username")
            .with_initial_text(
                ctx.config
                    .gateway
                    .login
                    .username
                    .clone()
                    .unwrap_or_default(),
            )
            .interact_text()?;

        let password = loop {
            let p1 = Password::with_theme(&theme)
                .with_prompt("Console password")
                .interact()?;
            let p2 = Password::with_theme(&theme)
                .with_prompt("Confirm password")
                .interact()?;
            if !p1.trim().is_empty() && p1 == p2 {
                break p1;
            }
            println!("  Passwords were empty or did not match — try again.");
        };

        // Idle auto-lock window. Same presets as the TUI provisioner; the
        // current setting is pre-selected so re-running setup does not silently
        // reset it.
        use crate::security::login::IDLE_PRESETS;
        let labels: Vec<&str> = IDLE_PRESETS.iter().map(|(l, _)| *l).collect();
        let current = ctx.config.gateway.login.idle_timeout_secs;
        let default_idx = IDLE_PRESETS
            .iter()
            .position(|(_, secs)| *secs == current)
            .unwrap_or(0);
        let idle_idx = Select::with_theme(&theme)
            .with_prompt("Lock automatically after a stretch of inactivity?")
            .items(&labels)
            .default(default_idx)
            .interact()?;

        ctx.config.gateway.login.username = Some(username.trim().to_string());
        ctx.config.gateway.login.password_hash =
            Some(crate::security::login::hash_password(&password)?);
        ctx.config.gateway.login.idle_timeout_secs =
            IDLE_PRESETS.get(idle_idx).map_or(0, |(_, secs)| *secs);
        println!(
            "  ⚠ The web console requires a claw-ui build with the login page; \
             the TUI will prompt for this password on next launch."
        );
        Ok(())
    }

    fn headless_hint(&self) -> &'static str {
        "run `rantaiclaw setup login` (sets config.gateway.login.username + password_hash)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_profile() -> Profile {
        Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn name_and_description_are_set() {
        let s = LoginSection;
        assert_eq!(s.name(), "login");
        assert!(!s.description().is_empty());
        assert!(!s.headless_hint().is_empty());
    }

    #[test]
    fn is_already_configured_tracks_password_hash() {
        let mut config = Config::default();
        assert!(!LoginSection.is_already_configured(&dummy_profile(), &config));
        config.gateway.login.password_hash = Some("$argon2id$v=19$m=1,t=1,p=1$a$b".into());
        assert!(LoginSection.is_already_configured(&dummy_profile(), &config));
    }
}
