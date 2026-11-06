//! The refusal for a config that names a subscription login that was removed.
//!
//! RantaiClaw uses API keys only. A config written before that change can
//! still name a login that no longer exists: `copilot`, `openai-codex` (and
//! its `codex` spelling), an OAuth-only Qwen alias, or a `minimax-oauth*`
//! alias. Such a config must not stop the daemon: a build-time error keeps
//! the gateway from binding, and the supervisor then retries the start in a
//! loop.
//!
//! So the factory keeps building a provider for those names. Every request
//! through it is refused here, with the API-key replacement named. The
//! refusal is a [`LocalRefusal`], so the retry wrapper recognises it by type
//! and does not spend the operator's time on a backoff that ends in the same
//! sentence.

use crate::providers::local_refusal::LocalRefusal;
use crate::providers::traits::Provider;

/// The refusal for a config that names the removed GitHub Copilot login.
///
/// Copilot has no API-key path at all, so there is nothing to move to.
pub(crate) const COPILOT_REFUSAL: &str = "The GitHub Copilot login was removed. Copilot has no API-key path in RantaiClaw — choose another provider.";

/// The refusal for a config that names the removed ChatGPT (OpenAI Codex) login.
pub(crate) const CODEX_REFUSAL: &str = "The ChatGPT (OpenAI Codex) login was removed. Use the \"openai\" provider with an OPENAI_API_KEY instead.";

/// The refusal for a config that names the removed Qwen Code OAuth login.
pub(crate) const QWEN_REFUSAL: &str = "The Qwen Code OAuth login was removed. Use the \"qwen\" provider with a DashScope API key (DASHSCOPE_API_KEY) instead.";

/// The refusal for a config that names the removed MiniMax OAuth login.
pub(crate) const MINIMAX_REFUSAL: &str = "The MiniMax OAuth login was removed. Use the \"minimax\" provider with a MINIMAX_API_KEY instead.";

/// The refusal for a provider name whose subscription login was removed.
///
/// `None` for every name that is still a provider. The names are matched
/// case-insensitively and trimmed, because a config value is free text.
pub(crate) fn removed_login_refusal(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "copilot" | "github-copilot" => Some(COPILOT_REFUSAL),
        "openai-codex" | "openai_codex" | "codex" => Some(CODEX_REFUSAL),
        "qwen-code" | "qwen-oauth" | "qwen_oauth" => Some(QWEN_REFUSAL),
        "minimax-oauth" | "minimax-oauth-cn" | "minimax-oauth-global" => Some(MINIMAX_REFUSAL),
        _ => None,
    }
}

/// Whether `name` is a provider name whose subscription login was removed.
pub(crate) fn is_removed_login_name(name: &str) -> bool {
    removed_login_refusal(name).is_some()
}

/// The refusal for a credential value that asks for a removed login.
///
/// A config can name the login in the credential instead of the provider
/// name: `api_key = "minimax-oauth"` or `api_key = "qwen-oauth"`. Those
/// values are placeholders, not keys, and the provider they are attached to
/// refuses with the same sentence the name would have produced.
pub(crate) fn removed_credential_refusal(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "minimax-oauth" | "minimax-oauth-cn" | "minimax-oauth-global" => Some(MINIMAX_REFUSAL),
        "qwen-oauth" | "qwen_code" => Some(QWEN_REFUSAL),
        _ => None,
    }
}

/// The provider a removed-login name resolves to.
///
/// It answers no request. The refusal names the API-key replacement, so the
/// operator learns what to configure without reading a log.
pub(crate) struct RemovedLoginProvider {
    message: &'static str,
}

impl RemovedLoginProvider {
    pub(crate) fn new(message: &'static str) -> Self {
        Self { message }
    }
}

#[async_trait::async_trait]
impl Provider for RemovedLoginProvider {
    /// The concrete provider behind this one, for a test that needs to reach
    /// the implementation a factory built.
    #[cfg(test)]
    fn test_as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Err(LocalRefusal::new(self.message).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::local_refusal::is_local_refusal;
    use crate::providers::{create_provider, has_usable_credential};

    /// Every removed provider name and the sentence it must refuse with.
    const REMOVED_NAMES: &[(&str, &str)] = &[
        ("copilot", COPILOT_REFUSAL),
        ("github-copilot", COPILOT_REFUSAL),
        ("openai-codex", CODEX_REFUSAL),
        ("openai_codex", CODEX_REFUSAL),
        ("codex", CODEX_REFUSAL),
        ("qwen-code", QWEN_REFUSAL),
        ("qwen-oauth", QWEN_REFUSAL),
        ("qwen_oauth", QWEN_REFUSAL),
        ("minimax-oauth", MINIMAX_REFUSAL),
        ("minimax-oauth-cn", MINIMAX_REFUSAL),
        ("minimax-oauth-global", MINIMAX_REFUSAL),
    ];

    /// The factory must still build a provider for a removed name, so the
    /// gateway binds and the daemon stays up; the refusal happens when a
    /// request is sent.
    #[tokio::test]
    async fn a_removed_login_name_is_refused_at_request_time() {
        let _env = crate::test_env::ENV_LOCK.lock().await;

        for (name, sentence) in REMOVED_NAMES {
            let provider = create_provider(name, None)
                .unwrap_or_else(|error| panic!("{name} must still build a provider: {error}"));

            // The provider behind the factory must be the refusing one. The
            // check comes before the request: a provider that is not this one
            // would reach for the network, and a test must never do that.
            assert!(
                provider
                    .test_as_any()
                    .and_then(|inner| inner.downcast_ref::<RemovedLoginProvider>())
                    .is_some(),
                "{name} must build the refusing provider"
            );

            let error = provider
                .simple_chat("hello", "test-model", 0.0)
                .await
                .expect_err("a removed login must refuse the request");
            assert_eq!(error.to_string(), *sentence, "{name}");
            assert!(is_local_refusal(&error), "{name}: {error}");
        }
    }

    /// The doctor and the wizard ask whether a provider has a credential it
    /// could send. A removed login has none — the replacement is a different
    /// provider.
    #[tokio::test]
    async fn a_removed_login_name_has_no_usable_credential() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        // The generic fallback keys must not make a removed login look
        // configured: the check comes before them.
        let _generic = crate::test_env::EnvGuard::set("RANTAICLAW_API_KEY", "generic-key");
        let _generic = crate::test_env::EnvGuard::set("API_KEY", "generic-key");

        for (name, _) in REMOVED_NAMES {
            assert!(
                !has_usable_credential(name, None),
                "{name} must not report a usable credential"
            );
        }
    }

    /// A config can name the login in the credential instead of the provider
    /// name. The placeholder is refused with the same sentence.
    #[tokio::test]
    async fn a_removed_credential_placeholder_is_refused() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        for (provider_name, placeholder, sentence) in [
            ("minimax", "minimax-oauth", MINIMAX_REFUSAL),
            ("minimax-cn", "minimax-oauth-cn", MINIMAX_REFUSAL),
            ("qwen", "qwen-oauth", QWEN_REFUSAL),
        ] {
            let provider = create_provider(provider_name, Some(placeholder))
                .unwrap_or_else(|error| panic!("{provider_name} must still build: {error}"));
            // Before the request: a provider that is not the refusing one
            // would send the placeholder to the vendor's endpoint.
            assert!(
                provider
                    .test_as_any()
                    .and_then(|inner| inner.downcast_ref::<RemovedLoginProvider>())
                    .is_some(),
                "{provider_name} must build the refusing provider"
            );
            let error = provider
                .simple_chat("hello", "test-model", 0.0)
                .await
                .expect_err("a removed credential placeholder must refuse the request");
            assert_eq!(error.to_string(), sentence, "{provider_name}");
            assert!(is_local_refusal(&error), "{provider_name}: {error}");

            assert!(
                !has_usable_credential(provider_name, Some(placeholder)),
                "{provider_name} must not report a usable credential"
            );
        }
    }

    /// A name that never was a provider is still a build-time error: the
    /// operator mistyped it, and the exit code says so.
    #[tokio::test]
    async fn an_unknown_provider_name_is_still_a_build_error() {
        let _env = crate::test_env::ENV_LOCK.lock().await;

        let error = create_provider("totally-unknown-provider-xyz", None)
            .err()
            .expect("a name that never existed must not build");
        assert!(
            error
                .downcast_ref::<crate::providers::ProviderConfigError>()
                .is_some(),
            "{error}"
        );
    }

    /// A removed name is not an alias of a kept provider: it must not inherit
    /// that provider's endpoint, its env var or its stored credential.
    #[tokio::test]
    async fn a_removed_name_is_not_an_alias_of_a_kept_provider() {
        let _env = crate::test_env::ENV_LOCK.lock().await;

        assert!(!crate::providers::is_minimax_alias("minimax-oauth"));
        assert!(!crate::providers::is_minimax_alias("minimax-oauth-cn"));
        assert!(!crate::providers::is_minimax_alias("minimax-oauth-global"));
        assert!(!crate::providers::is_qwen_alias("qwen-code"));
        assert!(!crate::providers::is_qwen_alias("qwen-oauth"));
        assert!(!crate::providers::is_qwen_alias("qwen_oauth"));

        // The per-provider state key keeps the removed name apart from the
        // kept one, so a stored key cannot answer for the removed name.
        assert_eq!(
            crate::providers::normalize_provider_name("minimax-oauth"),
            "minimax-oauth"
        );
        assert_eq!(
            crate::providers::normalize_provider_name("qwen-code"),
            "qwen-code"
        );
    }
}
