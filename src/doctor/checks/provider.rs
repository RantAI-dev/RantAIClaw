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

        let endpoint = self
            .endpoint_override
            .clone()
            .unwrap_or_else(|| resolve_endpoint(provider, ctx.config.api_url.as_deref()));

        // Resolve the way the send paths do. Reading only the top-level
        // `api_key` missed anything stored under `provider_api_keys` — what
        // the web console writes — and reported a spurious 401 for it.
        let api_key = ctx.config.resolve_key_for_provider(provider);

        // Never probe unauthenticated. Several providers serve `/models`
        // publicly (openrouter among them), so a keyless install got a 200 and
        // this check reported "provider responded 200 OK" for a config that
        // cannot send a single message. Local providers legitimately need no
        // key; everyone else without one is a hard fail, not a probe.
        if api_key.is_none() && !crate::providers::provider_is_local(provider) {
            // Warn, not Fail: a missing key is a setup gap (same as
            // `config.provider_key`), not a probe failure. Still refuses to
            // probe — a public endpoint answering 200 proves nothing — and
            // still surfaces it with a hint.
            return CheckResult::warn(
                self.name(),
                format!("no API key for {provider} — not probing; a public endpoint would answer 200 and prove nothing"),
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
            req = req.bearer_auth(key);
        }

        classify_response(self.name(), self.category(), &endpoint, req.send().await)
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

pub fn resolve_endpoint(provider: &str, api_url: Option<&str>) -> String {
    if let Some(base) = api_url.map(str::trim).filter(|s| !s.is_empty()) {
        return join_models(base);
    }
    let base = match provider {
        "openrouter" => "https://openrouter.ai/api/v1",
        "anthropic" => "https://api.anthropic.com/v1",
        "openai" => "https://api.openai.com/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "ollama" => "http://localhost:11434/v1",
        "deepseek" => "https://api.deepseek.com/v1",
        "zhipu" | "glm" => "https://open.bigmodel.cn/api/paas/v4",
        _ => {
            if let Some(rest) = provider.strip_prefix("custom:") {
                return join_models(rest);
            }
            return join_models(provider);
        }
    };
    join_models(base)
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
        let mut cfg = Config::default();
        cfg.api_key = None;
        let check = ProviderPingCheck::with_endpoint("http://127.0.0.1:1/models");
        let result = check.run(&ctx(cfg)).await;
        assert_eq!(result.severity, Severity::Warn, "msg: {}", result.message);
        assert!(result.message.contains("no API key"), "{}", result.message);
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
        assert_eq!(url, "https://example.com/v1/models");
    }

    #[test]
    fn resolve_endpoint_strips_trailing_slash() {
        let url = resolve_endpoint("openrouter", Some("https://example.com/v1/"));
        assert_eq!(url, "https://example.com/v1/models");
    }

    #[test]
    fn resolve_endpoint_falls_back_to_known_default() {
        let url = resolve_endpoint("openrouter", None);
        assert!(url.starts_with("https://openrouter.ai/api/v1"));
        assert!(url.ends_with("/models"));
    }

    #[test]
    fn resolve_endpoint_handles_custom_prefix() {
        let url = resolve_endpoint("custom:https://my-api.local", None);
        assert_eq!(url, "https://my-api.local/models");
    }
}
