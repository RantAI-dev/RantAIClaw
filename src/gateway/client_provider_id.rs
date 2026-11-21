//! Resolves a provider id a gateway client sent against the ids the operator
//! configured.
//!
//! `/status` and the other read surfaces show a `custom:` id with its userinfo
//! and query removed (`without_userinfo_and_query`). A client that copies that
//! form back names no configured provider, so the guards would refuse it. The
//! routes call [`client_provider_id`] first and continue with the configured id
//! it resolves to.

use std::collections::BTreeSet;

use crate::config::Config;
use crate::providers::without_userinfo_and_query;

/// The answer for one id a client sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderIdResolution {
    /// The sent id is configured as written, or matches no configured id. The
    /// guards decide the rest.
    Exact(String),
    /// The sent id is the redacted form of exactly one configured id, which is
    /// returned with its userinfo and query.
    Resolved(String),
    /// Two or more distinct configured ids have the sent id as their redacted
    /// form. Picking one would send a request under the wrong credential.
    Ambiguous,
}

/// The detail a client gets when its id names more than one configured provider.
pub(crate) const AMBIGUOUS_PROVIDER_ID_DETAIL: &str =
    "The provider id matches more than one configured provider. Send the full provider id from /config.";

/// Resolve a sent provider id against every configured id.
///
/// A sent id that is itself configured stays as it is, even when another
/// configured id has it as its redacted form. Otherwise the sent id resolves
/// only when exactly one configured id redacts to it.
pub(crate) fn resolve_client_provider_id(cfg: &Config, sent: &str) -> ProviderIdResolution {
    let configured = configured_provider_ids(cfg);
    if configured.contains(sent) {
        return ProviderIdResolution::Exact(sent.to_string());
    }
    let mut redacts_to_sent = configured
        .iter()
        .filter(|id| without_userinfo_and_query(id) == sent);
    match (redacts_to_sent.next(), redacts_to_sent.next()) {
        (None, _) => ProviderIdResolution::Exact(sent.to_string()),
        (Some(id), None) => ProviderIdResolution::Resolved(id.clone()),
        (Some(_), Some(_)) => ProviderIdResolution::Ambiguous,
    }
}

/// Every provider id the config holds, from the fields that
/// `Config::is_configured_provider_id` reads, trimmed the same way. A set, so
/// an id that appears in two fields counts once.
fn configured_provider_ids(cfg: &Config) -> BTreeSet<String> {
    cfg.default_provider
        .iter()
        .map(String::as_str)
        .chain(cfg.model_routes.iter().map(|route| route.provider.as_str()))
        .chain(
            cfg.reliability
                .fallback_providers
                .iter()
                .map(String::as_str),
        )
        .chain(cfg.agents.values().map(|agent| agent.provider.as_str()))
        .chain(
            cfg.gateway_agents
                .values()
                .filter_map(|agent| agent.provider.as_deref()),
        )
        .chain(cfg.provider_api_keys.keys().map(String::as_str))
        .map(|id| id.trim().to_string())
        .collect()
}

/// The id a route continues with: the sent id or its configured form. An
/// ambiguous id is refused with [`AMBIGUOUS_PROVIDER_ID_DETAIL`].
pub(crate) fn client_provider_id(cfg: &Config, sent: &str) -> Result<String, &'static str> {
    match resolve_client_provider_id(cfg, sent) {
        ProviderIdResolution::Exact(id) | ProviderIdResolution::Resolved(id) => Ok(id),
        ProviderIdResolution::Ambiguous => Err(AMBIGUOUS_PROVIDER_ID_DETAIL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{DelegateAgentConfig, GatewayAgentConfig, ModelRouteConfig};

    const SECRET_URL: &str = "custom:http://user:secret@127.0.0.1:9/v1?key=abc";
    const REDACTED: &str = "custom:http://127.0.0.1:9/v1";

    fn config_with_default(id: &str) -> Config {
        let mut cfg = Config::default();
        cfg.default_provider = Some(id.to_string());
        cfg
    }

    fn model_route(provider: &str) -> ModelRouteConfig {
        ModelRouteConfig {
            hint: "fast".into(),
            provider: provider.into(),
            model: "m".into(),
            api_key: None,
        }
    }

    #[test]
    fn a_redacted_custom_id_resolves_to_the_one_configured_id_it_redacts() {
        let cfg = config_with_default(SECRET_URL);
        assert_eq!(
            resolve_client_provider_id(&cfg, REDACTED),
            ProviderIdResolution::Resolved(SECRET_URL.to_string())
        );
    }

    #[test]
    fn a_configured_id_stays_exact_when_another_configured_id_redacts_to_it() {
        let mut cfg = config_with_default(REDACTED);
        cfg.model_routes.push(model_route(SECRET_URL));
        assert_eq!(
            resolve_client_provider_id(&cfg, REDACTED),
            ProviderIdResolution::Exact(REDACTED.to_string())
        );
        assert_eq!(
            resolve_client_provider_id(&cfg, SECRET_URL),
            ProviderIdResolution::Exact(SECRET_URL.to_string())
        );
    }

    #[test]
    fn two_configured_ids_that_redact_to_one_key_are_ambiguous() {
        let mut cfg = config_with_default("custom:http://first:a@127.0.0.1:9/v1?key=abc");
        cfg.model_routes
            .push(model_route("custom:http://second:b@127.0.0.1:9/v1?key=abc"));
        assert_eq!(
            resolve_client_provider_id(&cfg, REDACTED),
            ProviderIdResolution::Ambiguous
        );
        assert_eq!(
            client_provider_id(&cfg, REDACTED),
            Err(AMBIGUOUS_PROVIDER_ID_DETAIL)
        );
    }

    #[test]
    fn a_redacted_id_resolves_from_every_field_that_holds_a_configured_provider_id() {
        let mut routes = Config::default();
        routes.model_routes.push(model_route(SECRET_URL));

        let mut fallback = Config::default();
        fallback.reliability.fallback_providers = vec![SECRET_URL.to_string()];

        let mut agents = Config::default();
        agents.agents.insert(
            "worker".into(),
            DelegateAgentConfig {
                provider: SECRET_URL.into(),
                model: "m".into(),
                system_prompt: None,
                api_key: None,
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                max_iterations: 10,
            },
        );

        let mut gateway_agents = Config::default();
        gateway_agents.gateway_agents.insert(
            "front".into(),
            serde_json::from_value::<GatewayAgentConfig>(serde_json::json!({
                "workspace_dir": "/tmp/rantaiclaw-gateway-agent",
                "provider": SECRET_URL,
            }))
            .expect("a gateway agent that names the id"),
        );

        let mut stored_keys = Config::default();
        stored_keys
            .provider_api_keys
            .insert(SECRET_URL.to_string(), "key-for-test-0000".into());

        for (field, cfg) in [
            ("default_provider", config_with_default(SECRET_URL)),
            ("model_routes", routes),
            ("reliability.fallback_providers", fallback),
            ("agents", agents),
            ("gateway_agents", gateway_agents),
            ("provider_api_keys", stored_keys),
        ] {
            assert_eq!(
                resolve_client_provider_id(&cfg, REDACTED),
                ProviderIdResolution::Resolved(SECRET_URL.to_string()),
                "field {field}"
            );
        }
    }

    #[test]
    fn a_plain_provider_id_is_exact_whether_or_not_it_is_configured() {
        let cfg = config_with_default("openrouter");
        assert_eq!(
            resolve_client_provider_id(&cfg, "openrouter"),
            ProviderIdResolution::Exact("openrouter".to_string())
        );
        assert_eq!(
            resolve_client_provider_id(&Config::default(), "groq"),
            ProviderIdResolution::Exact("groq".to_string())
        );
    }

    #[test]
    fn an_unconfigured_custom_id_is_left_as_sent_for_the_guard_to_refuse() {
        assert_eq!(
            resolve_client_provider_id(&Config::default(), REDACTED),
            ProviderIdResolution::Exact(REDACTED.to_string())
        );
    }
}
