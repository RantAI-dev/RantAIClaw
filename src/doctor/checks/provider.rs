//! Provider live-ping check.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext, Severity};

const TIMEOUT: Duration = Duration::from_secs(10);

pub struct ProviderPingCheck {
    endpoint_override: Option<String>,
}

impl Default for ProviderPingCheck {
    fn default() -> Self { Self { endpoint_override: None } }
}

impl ProviderPingCheck {
    pub fn with_endpoint(url: impl Into<String>) -> Self {
        Self { endpoint_override: Some(url.into()) }
    }
}

#[async_trait]
impl DoctorCheck for ProviderPingCheck {
    fn name(&self) -> &'static str { "provider.ping" }
    fn category(&self) -> &'static str { "live" }
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

        let api_key = ctx.config.api_key.clone();

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
