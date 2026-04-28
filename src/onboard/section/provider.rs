//! Provider setup section — wraps the existing prompt flow in
//! `crate::onboard::wizard::setup_provider`.
//!
//! Wave 3 brings the provider step under the `SetupSection` umbrella so it
//! can be re-run individually via `rantaiclaw setup provider` and skipped
//! when already configured. The interview body itself (provider tier,
//! API-key collection, live model fetch + caching) stays in `wizard.rs` —
//! that helper is large, well-tested, and not worth duplicating during the
//! orchestrator rewrite.
//!
//! Headless behavior: emit `headless_hint()` and bail. Quick-setup
//! callers (`rantaiclaw onboard --api-key ... --provider ...`) keep going
//! through `run_quick_setup` rather than this section.

use anyhow::Result;

use super::{SetupContext, SetupSection};
use crate::config::Config;
use crate::onboard::wizard;
use crate::profile::Profile;

pub struct ProviderSection;

impl SetupSection for ProviderSection {
    fn name(&self) -> &'static str {
        "provider"
    }

    fn description(&self) -> &'static str {
        "AI provider, API key, and default model"
    }

    fn is_already_configured(&self, _profile: &Profile, config: &Config) -> bool {
        // Spec §"is_already_configured for provider": presence of a
        // configured `default_provider` with at least one credential
        // (`api_key`) — bare provider with no key is *not* configured.
        config.default_provider.is_some()
            && config
                .api_key
                .as_deref()
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false)
    }

    fn run(&self, ctx: &mut SetupContext) -> Result<()> {
        if !ctx.interactive {
            eprintln!("{}", self.headless_hint());
            return Ok(());
        }

        let workspace_dir = ctx.profile.workspace_dir();
        std::fs::create_dir_all(&workspace_dir).ok();
        let (provider, api_key, model, provider_api_url) = wizard::setup_provider(&workspace_dir)?;
        ctx.config.default_provider = Some(provider);
        ctx.config.api_key = if api_key.is_empty() {
            None
        } else {
            Some(api_key)
        };
        ctx.config.default_model = Some(model);
        if let Some(url) = provider_api_url {
            ctx.config.api_url = Some(url);
        }
        Ok(())
    }

    fn headless_hint(&self) -> &'static str {
        "rantaiclaw onboard --api-key <KEY> --provider <NAME> [--model <ID>]"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let s = ProviderSection;
        assert_eq!(s.name(), "provider");
        assert!(!s.description().is_empty());
        assert!(s.headless_hint().contains("--api-key"));
    }

    #[test]
    fn already_configured_requires_provider_and_key() {
        let s = ProviderSection;
        let dummy_profile = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp/_rt_test"),
        };

        let mut cfg = Config::default();
        assert!(!s.is_already_configured(&dummy_profile, &cfg));

        cfg.default_provider = Some("openrouter".into());
        assert!(
            !s.is_already_configured(&dummy_profile, &cfg),
            "provider w/o api key should not count as configured",
        );

        cfg.api_key = Some(String::new());
        assert!(!s.is_already_configured(&dummy_profile, &cfg));

        cfg.api_key = Some("sk-test".into());
        assert!(s.is_already_configured(&dummy_profile, &cfg));
    }
}
