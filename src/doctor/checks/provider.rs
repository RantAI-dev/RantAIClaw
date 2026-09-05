//! Provider live-ping check.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext, Severity};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct ProviderPingCheck {
    endpoint_override: Option<String>,
}

impl ProviderPingCheck {
    pub fn with_endpoint(url: impl Into<String>) -> Self {
        Self {
            endpoint_override: Some(url.into()),
        }
    }
}

#[async_trait]
impl DoctorCheck for ProviderPingCheck {
    fn name(&self) -> &'static str {
        "provider.ping"
    }
    fn category(&self) -> &'static str {
        "live"
    }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        if ctx.offline {
            return CheckResult::info(self.name(), "skipped (offline)")
                .with_category(self.category());
        }

        let provider = match ctx.config.default_provider.as_deref() {
            Some(p) => p,
            None => {
                return CheckResult::fail(self.name(), "no default_provider configured")
                    .with_category(self.category())
                    .with_hint("run: rantaiclaw setup provider")
            }
        };

        let endpoint = match self
            .endpoint_override
            .clone()
            .or_else(|| resolve_endpoint(provider, ctx.config.api_url.as_deref()))
        {
            Some(url) => url,
            // No endpoint known. Previously this fell through to using the
            // provider *name* as a URL — `minimax` became `minimax/models` —
            // and reqwest's refusal to build a request from it surfaced as
            // "network error: builder error", pointing every reader at their
            // connection when nothing had been sent. Say what is actually
            // true instead, and warn rather than fail: not knowing where to
            // probe is a gap in this check, not evidence the provider is
            // broken.
            None => {
                return CheckResult::warn(
                    self.name(),
                    format!("no probe endpoint known for {provider} — not probing"),
                )
                .with_category(self.category())
                .with_hint("set `api_url` in config.toml to probe this provider")
            }
        };

        // Resolve the way the send paths do. Reading only the top-level
        // `api_key` missed anything stored under `provider_api_keys` — what
        // the web console writes — and reported a spurious 401 for it.
        let api_key = ctx.config.resolve_key_for_provider(provider);

        // Never probe unauthenticated. Several providers serve `/models`
        // publicly (openrouter among them), so a keyless install got a 200 and
        // this check reported "provider responded 200 OK" for a config that
        // cannot send a single message. Local providers legitimately need no
        // key; everyone else without one is a hard fail, not a probe.
        // Same question as `config.provider_key`, so the two agree: an install
        // authenticated by an OAuth profile, a cached token or AWS env vars is
        // usable even with no key to send in a probe header.
        let usable = crate::providers::has_usable_credential(
            provider,
            api_key.as_deref(),
            Some(&crate::auth::state_dir_from_config(&ctx.config)),
        );

        // Usable, but not by a bearer this check could put in a header — an
        // auth profile, a cached OAuth token, AWS request signing. Probing
        // unauthenticated would draw a 401 and report a working install as
        // broken, which is the same false verdict in the other direction.
        if usable && api_key.is_none() && !crate::providers::provider_is_local(provider) {
            return CheckResult::ok(
                self.name(),
                format!(
                    "{provider} is authenticated by its own auth mode (not an API key) — \
                     not probing; this check can only send a bearer"
                ),
            )
            .with_category(self.category());
        }

        if !usable {
            // Warn, not Fail: a missing key is a setup gap (same as
            // `config.provider_key`), not a probe failure. Still refuses to
            // probe — a public endpoint answering 200 proves nothing — and
            // still surfaces it with a hint.
            return CheckResult::warn(
                self.name(),
                format!("no credential for {provider} — not probing; a public endpoint would answer 200 and prove nothing"),
            )
            .with_category(self.category())
            .with_hint("run: rantaiclaw setup provider");
        }

        let client = match reqwest::Client::builder().timeout(TIMEOUT).build() {
            Ok(c) => c,
            Err(e) => {
                return CheckResult::fail(self.name(), format!("HTTP client init failed: {e}"))
                    .with_category(self.category())
            }
        };

        let mut req = client.get(&endpoint);
        if let Some(key) = api_key {
            for (name, value) in probe_auth_headers(provider, &key) {
                req = req.header(name, value);
            }
        }

        classify_response(self.name(), self.category(), &endpoint, req.send().await)
    }
}

/// Auth headers for a provider validation/probe request. Anthropic needs
/// `x-api-key` + `anthropic-version` (Bearer is only for its OAuth/setup
/// tokens); every other provider uses `Authorization: Bearer <key>`. Shared by
/// the doctor provider check and the setup provider provisioner so they agree.
pub fn probe_auth_headers(provider: &str, api_key: &str) -> Vec<(String, String)> {
    if provider == "anthropic" {
        crate::auth::anthropic_token::anthropic_probe_headers(api_key)
    } else {
        vec![("Authorization".to_string(), format!("Bearer {api_key}"))]
    }
}

fn classify_response(
    name: &'static str,
    cat: &'static str,
    endpoint: &str,
    outcome: Result<reqwest::Response, reqwest::Error>,
) -> CheckResult {
    match outcome {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                CheckResult::ok(name, format!("provider responded {status} at {endpoint}"))
                    .with_category(cat)
            } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                CheckResult::fail(name, format!("auth failed: {status} at {endpoint}"))
                    .with_category(cat)
                    .with_hint("re-enter API key with: rantaiclaw setup provider")
            } else if status == StatusCode::TOO_MANY_REQUESTS {
                CheckResult {
                    name: name.to_string(),
                    severity: Severity::Warn,
                    message: format!("rate limited ({status})"),
                    hint: Some("retry later or upgrade your provider plan".to_string()),
                    duration_ms: 0,
                    category: cat,
                }
            } else {
                CheckResult::fail(name, format!("unexpected status {status} at {endpoint}"))
                    .with_category(cat)
                    .with_hint("check provider URL and credentials")
            }
        }
        Err(e) if e.is_timeout() => CheckResult::fail(
            name,
            format!("provider ping timed out after {}s", TIMEOUT.as_secs()),
        )
        .with_category(cat)
        .with_hint("check network connectivity or provider status page"),
        Err(e) => CheckResult::fail(name, format!("network error: {e}"))
            .with_category(cat)
            .with_hint("check network connectivity"),
    }
}

/// Where to `GET /models` to prove a provider's key works, or `None` when this
/// check does not know.
///
/// Returning `None` is the point. The previous version ended with
/// `join_models(provider)`, turning an unknown provider name straight into a
/// URL — `minimax` became `minimax/models`, which reqwest cannot build a
/// request from, so every one of the ~20 providers absent from the list below
/// reported `network error: builder error` without a packet leaving the
/// machine. A wrong answer that reads as a connectivity problem is worse than
/// no answer.
///
/// Region-varying families are asked of `providers` rather than listed here.
/// Their endpoint depends on which alias was configured (`minimax` vs
/// `minimax-cn`), so a flat name→URL table cannot express them, and a second
/// copy of the constants would drift from the one `create_provider` uses.
pub fn resolve_endpoint(provider: &str, api_url: Option<&str>) -> Option<String> {
    if let Some(base) = api_url.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(join_models(base));
    }
    if let Some(rest) = provider.strip_prefix("custom:") {
        return Some(join_models(rest));
    }
    if let Some(base) = crate::providers::region_base_url(provider) {
        return Some(join_models(base));
    }
    // `zhipu` used to be listed here alongside `glm`, pointing at
    // `open.bigmodel.cn`. It is gone rather than moved: `is_glm_global_alias`
    // covers both names, so `region_base_url` above now answers first and this
    // arm was unreachable. Keeping it would have documented an endpoint the
    // code cannot reach — and the wrong one, since `create_provider` builds
    // both names against `api.z.ai` (GLM_GLOBAL). Only `glm-cn`/`zhipu-cn`/
    // `bigmodel` resolve to `open.bigmodel.cn`, which is exactly what the
    // family resolver already encodes.
    // Every entry below is the base `create_provider` builds the client with,
    // so a probe here exercises the path a real message would take. The ones
    // added beyond the original six were each confirmed against the live API
    // without credentials: the host answers 404 for a nonsense path and
    // something else (401/400/422) for this one, so the response distinguishes
    // "endpoint exists" from "wrong path".
    //
    // Providers whose host answers identically for every path — cloudflare,
    // doubao, together, cohere — are deliberately absent. Listing them would
    // look like coverage while proving nothing, and this resolver also decides
    // where onboarding sends a freshly-entered API key.
    let base = match provider {
        "openrouter" => "https://openrouter.ai/api/v1",
        "anthropic" => "https://api.anthropic.com/v1",
        "openai" => "https://api.openai.com/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "ollama" => "http://localhost:11434/v1",
        "deepseek" => "https://api.deepseek.com/v1",
        "mistral" => "https://api.mistral.ai/v1",
        "xai" | "grok" => "https://api.x.ai/v1",
        "perplexity" => "https://api.perplexity.ai",
        "fireworks" | "fireworks-ai" => "https://api.fireworks.ai/inference/v1",
        "nvidia" | "nvidia-nim" => "https://integrate.api.nvidia.com/v1",
        "venice" => "https://api.venice.ai/api/v1",
        "vercel" | "vercel-ai" => "https://ai-gateway.vercel.sh/v1",
        "opencode" | "opencode-zen" => "https://opencode.ai/zen/v1",
        "kimi-code" => "https://api.kimi.com/coding/v1",
        _ => return None,
    };
    Some(join_models(base))
}

fn join_models(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    format!("{trimmed}/models")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::doctor::Severity;
    use crate::profile::Profile;

    fn ctx(cfg: Config) -> DoctorContext {
        DoctorContext {
            profile: Profile {
                name: "test".into(),
                root: std::path::PathBuf::from("/nonexistent"),
            },
            config: cfg,
            offline: false,
        }
    }

    /// Was: with no key the check sent an UNAUTHENTICATED GET. Several
    /// providers serve `/models` publicly — openrouter, the default, among
    /// them — so it got a 200 and reported "provider responded 200 OK" for a
    /// config that cannot send a message. The endpoint here is unreachable on
    /// purpose: if the guard regresses, the check tries to probe and this
    /// fails on the message, not on the network.
    #[tokio::test]
    async fn ping_refuses_to_probe_without_a_key_rather_than_reporting_ok() {
        // The check now asks `has_usable_credential`, which reads the
        // environment — so this must own the environment, or a developer with
        // `OPENROUTER_API_KEY` exported gets the opposite verdict.
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let mut cfg = Config::default();
        cfg.api_key = None;
        let check = ProviderPingCheck::with_endpoint("http://127.0.0.1:1/models");
        let result = check.run(&ctx(cfg)).await;
        assert_eq!(result.severity, Severity::Warn, "msg: {}", result.message);
        assert!(
            result.message.contains("no credential"),
            "{}",
            result.message
        );
    }

    /// A provider authenticated by something that is not a bearer — an auth
    /// profile, a cached OAuth token, AWS request signing — must not be probed
    /// unauthenticated. The endpoint would answer 401 and this check would
    /// report a working install as broken: the same false verdict as the bug
    /// it replaced, pointing the other way.
    #[tokio::test]
    async fn ping_does_not_probe_unauthenticated_when_the_credential_is_not_a_key() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let home = tempfile::tempdir().expect("config home");
        let _xdg = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", home.path());

        let dir = home.path().join("rantaiclaw").join("copilot");
        std::fs::create_dir_all(&dir).expect("copilot dir");
        std::fs::write(dir.join("access-token"), "gho_neutral_placeholder_value")
            .expect("write token");

        let mut cfg = Config::default();
        cfg.default_provider = Some("copilot".into());
        cfg.api_key = None;
        let check = ProviderPingCheck::with_endpoint("http://127.0.0.1:1/models");
        let result = check.run(&ctx(cfg)).await;

        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
        assert!(
            result.message.contains("not probing"),
            "it must say it did not probe: {}",
            result.message
        );
    }

    /// Local providers have no key by design; refusing to probe them would
    /// turn a working Ollama install into a red cross.
    #[tokio::test]
    async fn ping_still_probes_local_providers_without_a_key() {
        let mut cfg = Config::default();
        cfg.default_provider = Some("ollama".into());
        cfg.api_key = None;
        let check = ProviderPingCheck::with_endpoint("http://127.0.0.1:1/models");
        let result = check.run(&ctx(cfg)).await;
        // Connection refused, not the no-key refusal — it reached the network.
        assert!(
            !result.message.contains("no API key"),
            "should have probed: {}",
            result.message
        );
    }

    #[test]
    fn resolve_endpoint_uses_api_url_override() {
        let url = resolve_endpoint("openrouter", Some("https://example.com/v1"));
        assert_eq!(url.as_deref(), Some("https://example.com/v1/models"));
    }

    #[test]
    fn resolve_endpoint_strips_trailing_slash() {
        let url = resolve_endpoint("openrouter", Some("https://example.com/v1/"));
        assert_eq!(url.as_deref(), Some("https://example.com/v1/models"));
    }

    #[test]
    fn resolve_endpoint_falls_back_to_known_default() {
        let url = resolve_endpoint("openrouter", None).expect("openrouter is known");
        assert!(url.starts_with("https://openrouter.ai/api/v1"));
        assert!(url.ends_with("/models"));
    }

    /// The bug this guards: the old fallback ended with `join_models(provider)`,
    /// so an unknown provider name became a "URL". `minimax` → `minimax/models`,
    /// which reqwest cannot build a request from — reported to the user as
    /// `network error: builder error`, with nothing having touched the network.
    #[test]
    fn resolve_endpoint_never_turns_a_bare_provider_name_into_a_url() {
        for provider in [
            "bedrock",
            "copilot",
            "openai-codex",
            "qianfan",
            "not-a-provider",
        ] {
            let url = resolve_endpoint(provider, None);
            assert!(
                url.is_none(),
                "{provider} resolved to {url:?}; unknown providers must resolve to None so the \
                 check can say so instead of probing a non-URL"
            );
        }
    }

    /// Region-varying providers come from `providers`, not from a second table
    /// here — the endpoint depends on which alias was configured, which a flat
    /// name→URL list cannot express.
    #[test]
    fn resolve_endpoint_asks_providers_for_region_varying_families() {
        let intl = resolve_endpoint("minimax", None).expect("minimax is known");
        assert_eq!(intl, "https://api.minimax.io/v1/models");

        let cn = resolve_endpoint("minimax-cn", None).expect("minimax-cn is known");
        assert_eq!(cn, "https://api.minimaxi.com/v1/models");
        assert_ne!(
            intl, cn,
            "the two regions must not collapse to one endpoint"
        );

        for provider in ["glm", "moonshot", "qwen", "zai"] {
            let url = resolve_endpoint(provider, None)
                .unwrap_or_else(|| panic!("{provider} should resolve"));
            assert!(url.starts_with("https://"), "{provider} -> {url}");
            assert!(url.ends_with("/models"), "{provider} -> {url}");
        }
    }

    /// `glm` and `zhipu` are the same provider under two names, and
    /// `create_provider` builds both against GLM_GLOBAL. doctor used to probe
    /// `open.bigmodel.cn` for them — a different host from the one the agent
    /// actually talks to, so a green tick there proved nothing about the
    /// configured client. Only the explicit `-cn` aliases belong on bigmodel.
    #[test]
    fn glm_aliases_probe_the_host_create_provider_uses() {
        for name in ["glm", "zhipu"] {
            assert_eq!(
                resolve_endpoint(name, None).as_deref(),
                Some("https://api.z.ai/api/paas/v4/models"),
                "{name}"
            );
        }
        for name in ["glm-cn", "zhipu-cn", "bigmodel"] {
            assert_eq!(
                resolve_endpoint(name, None).as_deref(),
                Some("https://open.bigmodel.cn/api/paas/v4/models"),
                "{name}"
            );
        }
    }

    /// This resolver decides where onboarding sends a freshly-entered API key,
    /// so a hostname here is a credential destination, not just a probe target.
    ///
    /// It replaced a second table in `onboard::provision::provider` that had
    /// drifted into naming domains that **do not exist** — `api.zPUmlw.com`
    /// for Z.AI and `api.moonshot.io` for Moonshot International. Neither
    /// resolves today, so the probe simply failed; but either could be
    /// registered by anyone, at which point every setup run would hand them a
    /// working key.
    ///
    /// Hence: every host below must be one `create_provider` also builds with.
    #[test]
    fn every_endpoint_is_a_host_the_client_actually_uses() {
        for provider in [
            "openrouter",
            "anthropic",
            "openai",
            "groq",
            "deepseek",
            "mistral",
            "xai",
            "perplexity",
            "fireworks",
            "nvidia",
            "venice",
            "vercel",
            "opencode",
            "kimi-code",
            "minimax",
            "glm",
            "moonshot",
            "qwen",
            "zai",
        ] {
            let url = resolve_endpoint(provider, None)
                .unwrap_or_else(|| panic!("{provider} should resolve"));
            assert!(url.starts_with("https://"), "{provider} -> {url}");
            // The two dead domains, pinned by name so neither can come back.
            assert!(
                !url.contains("zPUmlw") && !url.contains("moonshot.io"),
                "{provider} -> {url} names a domain that does not exist"
            );
        }
    }

    #[test]
    fn resolve_endpoint_handles_custom_prefix() {
        let url = resolve_endpoint("custom:https://my-api.local", None);
        assert_eq!(url.as_deref(), Some("https://my-api.local/models"));
    }

    #[test]
    fn probe_auth_headers_uses_x_api_key_for_anthropic() {
        let h = probe_auth_headers("anthropic", "sk-ant-api-1");
        assert!(
            h.iter().any(|(k, _)| k == "x-api-key"),
            "anthropic must use x-api-key, not Bearer: {h:?}"
        );
        assert!(h.iter().any(|(k, _)| k == "anthropic-version"), "{h:?}");
    }

    #[test]
    fn probe_auth_headers_uses_bearer_for_other_providers() {
        let h = probe_auth_headers("openai", "sk-1");
        assert_eq!(
            h,
            vec![("Authorization".to_string(), "Bearer sk-1".to_string())]
        );
    }
}
