use crate::config::Config;
use crate::onboard::wizard::{humanize_age, ModelRefreshOutcome, StaleCacheReason};
use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelProbeOutcome {
    Ok,
    Skipped,
    AuthOrAccess,
    Error,
}

fn classify_model_probe_error(err_message: &str) -> ModelProbeOutcome {
    let lower = err_message.to_lowercase();

    if lower.contains("does not support live model discovery") {
        return ModelProbeOutcome::Skipped;
    }

    if [
        "401",
        "403",
        "429",
        "unauthorized",
        "forbidden",
        "api key",
        "token",
        "insufficient balance",
        "insufficient quota",
        "plan does not include",
        "rate limit",
    ]
    .iter()
    .any(|hint| lower.contains(hint))
    {
        return ModelProbeOutcome::AuthOrAccess;
    }

    ModelProbeOutcome::Error
}

/// Every provider to probe: the override alone, else all registered providers.
pub(crate) fn doctor_model_targets(provider_override: Option<&str>) -> Vec<String> {
    if let Some(provider) = provider_override.map(str::trim).filter(|p| !p.is_empty()) {
        return vec![provider.to_string()];
    }

    crate::providers::list_providers()
        .into_iter()
        .map(|provider| provider.name.to_string())
        .collect()
}

/// How a batch of catalog probes came out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeSummary {
    pub ok: usize,
    pub skipped: usize,
    pub auth: usize,
    pub errors: usize,
    /// The provider could not be reached, but a previously-cached catalog was
    /// shown. Counted apart from `ok` because nothing was verified — this bucket
    /// exists precisely so a failed probe stops reading as a passed one.
    pub stale: usize,
}

/// Refresh each provider's model catalog in turn, printing a block per
/// provider, and **continue past failures**. One provider without an API key
/// must not abort the sweep, so errors are classified into buckets rather than
/// returned — the caller decides what a given bucket means for its exit status.
pub(crate) fn refresh_model_catalogs(
    config: &Config,
    targets: &[String],
    force: bool,
) -> ProbeSummary {
    let mut summary = ProbeSummary::default();

    for provider_name in targets {
        println!("  [{}]", provider_name);

        match crate::onboard::run_models_refresh(config, Some(provider_name), force) {
            Ok(ModelRefreshOutcome::FetchedLive { count }) => {
                summary.ok += 1;
                println!("    ✅ model catalog check passed ({count} models)");
            }
            Ok(ModelRefreshOutcome::ServedFreshCache { age_secs }) => {
                // Cache-first mode asked for exactly this, so it is not a
                // failure — but no request was made, so say so rather than
                // implying the provider answered.
                summary.ok += 1;
                println!(
                    "    ✅ cached catalog is fresh ({} old, provider not contacted)",
                    humanize_age(age_secs)
                );
            }
            Ok(ModelRefreshOutcome::ServedStaleCache { age_secs, reason }) => {
                // The headline bug: this printed `✅ model catalog check passed`
                // on a machine with no Ollama running.
                summary.stale += 1;
                let why = match reason {
                    StaleCacheReason::FetchFailed => "provider unreachable",
                    StaleCacheReason::ProviderReturnedEmpty => "provider returned no models",
                };
                println!(
                    "    ⚠️  stale: showing cache from {} ago — {why}, nothing verified",
                    humanize_age(age_secs)
                );
            }
            Err(error) => {
                let error_text = format_error_chain(&error);
                match classify_model_probe_error(&error_text) {
                    ModelProbeOutcome::Skipped => {
                        summary.skipped += 1;
                        println!("    ⚪ skipped: {}", truncate_for_display(&error_text, 160));
                    }
                    ModelProbeOutcome::AuthOrAccess => {
                        summary.auth += 1;
                        println!(
                            "    ⚠️  auth/access: {}",
                            truncate_for_display(&error_text, 160)
                        );
                    }
                    ModelProbeOutcome::Error => {
                        summary.errors += 1;
                        println!("    ❌ error: {}", truncate_for_display(&error_text, 160));
                    }
                    ModelProbeOutcome::Ok => {
                        summary.ok += 1;
                    }
                }
            }
        }

        println!();
    }

    summary
}

/// Backs `rantaiclaw models refresh --all`.
///
/// The all-provider sweep already existed, but only under `doctor models` —
/// the command nobody reaches for when they want to update a model list. This
/// is the same enumerator and the same loop under the name operators guess.
pub fn refresh_all_model_catalogs(config: &Config, force: bool) -> Result<()> {
    let targets = doctor_model_targets(None);
    if targets.is_empty() {
        anyhow::bail!("No providers available for model refresh");
    }

    println!("Refreshing model catalogs for {} providers.", targets.len());
    println!();

    let summary = refresh_model_catalogs(config, &targets, force);

    println!(
        "  Summary: {} ok, {} stale, {} skipped, {} auth/access, {} errors",
        summary.ok, summary.stale, summary.skipped, summary.auth, summary.errors
    );
    if summary.auth > 0 {
        println!("  Some providers need a valid API key before their catalog can be fetched.");
    }
    if summary.stale > 0 {
        println!("  Stale entries kept a previous catalog; those providers were not reached.");
    }

    Ok(())
}

pub fn run_models(config: &Config, provider_override: Option<&str>, use_cache: bool) -> Result<()> {
    let targets = doctor_model_targets(provider_override);

    if targets.is_empty() {
        anyhow::bail!("No providers available for model probing");
    }

    println!("🩺 RantaiClaw Doctor — Model Catalog Probe");
    println!("  Providers to probe: {}", targets.len());
    println!(
        "  Mode: {}",
        if use_cache {
            "cache-first"
        } else {
            "force live refresh"
        }
    );
    println!();

    let summary = refresh_model_catalogs(config, &targets, !use_cache);

    println!(
        "  Summary: {} ok, {} stale, {} skipped, {} auth/access, {} errors",
        summary.ok, summary.stale, summary.skipped, summary.auth, summary.errors
    );

    if summary.stale > 0 {
        println!(
            "  ⚠️  {} provider(s) served a cached catalog without being reached — not a pass.",
            summary.stale
        );
    }

    if summary.auth > 0 {
        println!(
            "  💡 Some providers need valid API keys/plan access before `/models` can be fetched."
        );
    }

    // A stale cache is deliberately NOT counted here: `--provider X` asks
    // whether X is reachable, and showing yesterday's list does not answer that.
    if provider_override.is_some() && summary.ok == 0 {
        anyhow::bail!("Model probe failed for target provider")
    }

    Ok(())
}

fn format_error_chain(error: &anyhow::Error) -> String {
    let mut parts = Vec::new();
    for cause in error.chain() {
        let message = cause.to_string();
        if !message.is_empty() {
            parts.push(message);
        }
    }

    if parts.is_empty() {
        return String::new();
    }

    parts.join(": ")
}

fn truncate_for_display(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn classify_model_probe_error_marks_unsupported_as_skipped() {
        let outcome = classify_model_probe_error(
            "Provider 'copilot' does not support live model discovery yet",
        );
        assert_eq!(outcome, ModelProbeOutcome::Skipped);
    }

    #[test]
    fn classify_model_probe_error_marks_auth_and_plan_issues() {
        let auth_outcome = classify_model_probe_error("OpenAI API error (401): unauthorized");
        assert_eq!(auth_outcome, ModelProbeOutcome::AuthOrAccess);

        let plan_outcome = classify_model_probe_error(
            "Z.AI API error (429): plan does not include requested model",
        );
        assert_eq!(plan_outcome, ModelProbeOutcome::AuthOrAccess);
    }

    #[test]
    fn truncate_for_display_preserves_utf8_boundaries() {
        let preview = truncate_for_display("🙂example-alpha-build", 3);
        assert_eq!(preview, "🙂ex…");
    }

    // ── batch catalog refresh ────────────────────────────────────

    #[test]
    fn doctor_model_targets_defaults_to_every_registered_provider() {
        let all = doctor_model_targets(None);
        assert_eq!(all.len(), crate::providers::list_providers().len());
        assert!(all.iter().any(|p| p == "openrouter"));

        // An override narrows to exactly one, which is what `--provider` means.
        assert_eq!(doctor_model_targets(Some("openrouter")), vec!["openrouter"]);
        assert_eq!(doctor_model_targets(Some("   ")), all);
    }

    #[test]
    fn a_provider_served_from_stale_cache_is_not_counted_as_passing() {
        // The reported bug, at the level the operator sees it: `doctor models`
        // printed `✅ model catalog check passed` for a provider it could not
        // reach, and the run summary counted it among the "ok".
        let tmp = TempDir::new().unwrap();
        crate::onboard::wizard::cache_live_models_for_provider(
            tmp.path(),
            "llamacpp",
            &["cached-model".to_string()],
        )
        .unwrap();

        let mut config = Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        config.default_provider = Some("llamacpp".into());
        // Unparseable endpoint: the fetch fails before a socket is opened, so
        // this stays offline.
        config.api_url = Some("not-a-url".into());

        let summary = refresh_model_catalogs(&config, &["llamacpp".to_string()], true);

        assert_eq!(summary.stale, 1, "an unreached provider belongs in `stale`");
        assert_eq!(
            summary.ok, 0,
            "showing yesterday's catalog is not a passed check"
        );
        assert_eq!(summary.errors, 0, "the command itself did not fail");
        assert_eq!(summary.auth, 0);
        assert_eq!(summary.skipped, 0);
    }

    #[test]
    fn refresh_model_catalogs_continues_past_a_failing_provider() {
        // Providers with no live-fetch support bail before any network call, so
        // this exercises the loop offline. Every target fails; the point is that
        // the batch still visits all of them and reports per-provider outcomes
        // instead of aborting on the first error.
        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = tmp.path().to_path_buf();

        let targets: Vec<String> = ["bedrock", "perplexity", "minimax"]
            .iter()
            .map(|p| (*p).to_string())
            .collect();

        let summary = refresh_model_catalogs(&config, &targets, true);

        assert_eq!(
            summary.skipped,
            targets.len(),
            "every unsupported provider should be skipped, not abort the sweep"
        );
        assert_eq!(summary.ok, 0);
        assert_eq!(summary.errors, 0);
        assert_eq!(summary.auth, 0);
    }
}
