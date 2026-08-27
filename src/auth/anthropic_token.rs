use serde::{Deserialize, Serialize};

/// How Anthropic credentials should be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnthropicAuthKind {
    /// Standard Anthropic API key via `x-api-key`.
    ApiKey,
    /// Subscription / setup token via `Authorization: Bearer ...`.
    Authorization,
}

impl AnthropicAuthKind {
    pub fn as_metadata_value(self) -> &'static str {
        match self {
            Self::ApiKey => "api-key",
            Self::Authorization => "authorization",
        }
    }

    pub fn from_metadata_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "api-key" | "x-api-key" | "apikey" => Some(Self::ApiKey),
            "authorization" | "bearer" | "auth-token" | "oauth" => Some(Self::Authorization),
            _ => None,
        }
    }
}

/// Detect auth kind with explicit override support.
pub fn detect_auth_kind(token: &str, explicit: Option<&str>) -> AnthropicAuthKind {
    if let Some(kind) = explicit.and_then(AnthropicAuthKind::from_metadata_value) {
        return kind;
    }

    let trimmed = token.trim();

    // JWT-like shape strongly suggests bearer token mode.
    if trimmed.matches('.').count() >= 2 {
        return AnthropicAuthKind::Authorization;
    }

    // Anthropic platform keys commonly start with this prefix.
    if trimmed.starts_with("sk-ant-api") {
        return AnthropicAuthKind::ApiKey;
    }

    // Default to API key for backward compatibility unless explicitly configured.
    AnthropicAuthKind::ApiKey
}

/// HTTP headers for a validation/probe request to the Anthropic API. Anthropic
/// rejects `Authorization: Bearer <api-key>` (that is only for subscription /
/// OAuth setup tokens) — a real `sk-ant-api…` key must go in `x-api-key`, and
/// every request needs `anthropic-version`. Sending Bearer made setup + doctor
/// report a *valid* key as rejected and push the operator to replace a working
/// credential. Returns `(header_name, header_value)` pairs.
pub fn anthropic_probe_headers(api_key: &str) -> Vec<(String, String)> {
    let mut headers = vec![("anthropic-version".to_string(), "2023-06-01".to_string())];
    match detect_auth_kind(api_key, None) {
        AnthropicAuthKind::Authorization => {
            headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
        }
        AnthropicAuthKind::ApiKey => {
            headers.push(("x-api-key".to_string(), api_key.to_string()));
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kind_from_metadata() {
        assert_eq!(
            AnthropicAuthKind::from_metadata_value("authorization"),
            Some(AnthropicAuthKind::Authorization)
        );
        assert_eq!(
            AnthropicAuthKind::from_metadata_value("x-api-key"),
            Some(AnthropicAuthKind::ApiKey)
        );
        assert_eq!(AnthropicAuthKind::from_metadata_value("nope"), None);
    }

    #[test]
    fn detect_prefers_override() {
        let kind = detect_auth_kind("sk-ant-api-123", Some("authorization"));
        assert_eq!(kind, AnthropicAuthKind::Authorization);
    }

    #[test]
    fn detect_jwt_like_as_authorization() {
        let kind = detect_auth_kind("aaa.bbb.ccc", None);
        assert_eq!(kind, AnthropicAuthKind::Authorization);
    }

    #[test]
    fn detect_default_for_api_prefix() {
        let kind = detect_auth_kind("sk-ant-api-123", None);
        assert_eq!(kind, AnthropicAuthKind::ApiKey);
    }

    #[test]
    fn anthropic_probe_uses_x_api_key_for_api_key() {
        let h = anthropic_probe_headers("sk-ant-api-xyz");
        assert!(
            h.iter()
                .any(|(k, v)| k == "x-api-key" && v == "sk-ant-api-xyz"),
            "{h:?}"
        );
        assert!(h.iter().any(|(k, _)| k == "anthropic-version"), "{h:?}");
        assert!(
            !h.iter().any(|(k, _)| k == "Authorization"),
            "must not send Bearer for an api key: {h:?}"
        );
    }

    #[test]
    fn anthropic_probe_uses_bearer_for_setup_token() {
        let h = anthropic_probe_headers("aaa.bbb.ccc");
        assert!(
            h.iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer aaa.bbb.ccc"),
            "{h:?}"
        );
        assert!(!h.iter().any(|(k, _)| k == "x-api-key"), "{h:?}");
    }
}
