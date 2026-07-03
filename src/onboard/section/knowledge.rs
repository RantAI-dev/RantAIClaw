//! Knowledge Base setup section — captures the embedding API key (and an
//! optional OCR/vision key) into `config.knowledge` so the agent can search
//! documents the user ingests.
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"5. Section catalog" (Knowledge Base) — feature-gated on `kb`.
//!
//! On `run`:
//!   * if interactive — confirm enablement, then resolve an embedding key
//!     (reuse the main provider key when it's an OpenRouter key, enter a new
//!     one, or skip), and optionally an OCR/vision key;
//!   * if headless — no-op (the wizard's final `config.save()` still runs).
//!
//! `is_already_configured` is a presence check on
//! `config.knowledge.embedding_api_key` (or the `KB_EMBEDDING_API_KEY` env
//! var). This section never calls `config.save()` — the orchestrator persists
//! (and encrypts) once at the end of the run.

use anyhow::Result;

use super::{SetupContext, SetupSection};
use crate::config::Config;
use crate::profile::Profile;

pub struct KnowledgeSection;

impl SetupSection for KnowledgeSection {
    fn name(&self) -> &'static str {
        "knowledge"
    }

    fn description(&self) -> &'static str {
        "Knowledge Base — document search (embedding) + optional OCR/vision"
    }

    /// Configured iff an embedding key is already stored in `config.knowledge`
    /// or exported via `KB_EMBEDDING_API_KEY`.
    fn is_already_configured(&self, _profile: &Profile, config: &Config) -> bool {
        let in_config = config
            .knowledge
            .embedding_api_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty());
        let in_env = std::env::var("KB_EMBEDDING_API_KEY")
            .ok()
            .is_some_and(|k| !k.trim().is_empty());
        in_config || in_env
    }

    fn run(&self, ctx: &mut SetupContext) -> Result<()> {
        use dialoguer::theme::ColorfulTheme;
        use dialoguer::{Confirm, Select};

        if !ctx.interactive {
            return Ok(());
        }

        let theme = ColorfulTheme::default();

        let enable = Confirm::with_theme(&theme)
            .with_prompt("Enable Knowledge Base? Lets the agent search documents you ingest")
            .default(false)
            .interact()?;
        if !enable {
            return Ok(());
        }

        // ── Embedding key ────────────────────────────────────────────
        let main_key_reusable = ctx.config.default_provider.as_deref() == Some("openrouter")
            && ctx
                .config
                .api_key
                .as_deref()
                .is_some_and(|k| !k.trim().is_empty());

        let embedding_key: Option<String> = if main_key_reusable {
            let choice = Select::with_theme(&theme)
                .with_prompt("Embedding key")
                .items([
                    "Use the main provider key",
                    "Enter a key",
                    "Skip (leave disabled)",
                ])
                .default(0)
                .interact()?;
            match choice {
                0 => ctx.config.api_key.clone(),
                1 => prompt_key("Embedding API key")?,
                _ => None,
            }
        } else {
            let choice = Select::with_theme(&theme)
                .with_prompt("Embedding key")
                .items(["Enter a key", "Skip (leave disabled)"])
                .default(0)
                .interact()?;
            match choice {
                0 => prompt_key("Embedding API key")?,
                _ => None,
            }
        };

        let Some(embedding_key) = embedding_key else {
            // No embedding key resolved — Knowledge Base stays disabled.
            return Ok(());
        };
        ctx.config.knowledge.embedding_api_key = Some(embedding_key.clone());

        // ── Vision / OCR key (optional) ──────────────────────────────
        let vision_choice = Select::with_theme(&theme)
            .with_prompt("OCR / vision key (for scanned documents)")
            .items(["Use the embedding key", "Enter a different key", "Skip OCR"])
            .default(0)
            .interact()?;
        ctx.config.knowledge.vision_api_key = match vision_choice {
            0 => Some(embedding_key),
            1 => prompt_key("OCR / vision API key")?,
            _ => None,
        };

        Ok(())
    }

    fn headless_hint(&self) -> &'static str {
        "set config.knowledge.embedding_api_key (or export KB_EMBEDDING_API_KEY)"
    }
}

/// Prompt for a key, returning `None` when the input is empty/whitespace.
fn prompt_key(prompt: &str) -> Result<Option<String>> {
    use dialoguer::theme::ColorfulTheme;
    use dialoguer::Input;

    let theme = ColorfulTheme::default();

    let raw: String = Input::with_theme(&theme)
        .with_prompt(prompt)
        .allow_empty(true)
        .interact_text()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
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
        let section = KnowledgeSection;
        assert_eq!(section.name(), "knowledge");
        assert!(!section.description().is_empty());
        assert!(!section.headless_hint().is_empty());
    }

    #[test]
    fn is_already_configured_true_when_config_has_key() {
        std::env::remove_var("KB_EMBEDDING_API_KEY");
        let mut config = crate::config::Config::default();
        config.knowledge.embedding_api_key = Some("k".into());
        assert!(KnowledgeSection.is_already_configured(&dummy_profile(), &config));
    }

    #[test]
    fn is_already_configured_false_when_empty() {
        std::env::remove_var("KB_EMBEDDING_API_KEY");
        let config = crate::config::Config::default();
        assert!(!KnowledgeSection.is_already_configured(&dummy_profile(), &config));
    }
}
