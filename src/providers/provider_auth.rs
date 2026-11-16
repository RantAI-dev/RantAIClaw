//! The marker for a provider that rejected the credential it was sent.
//!
//! The retry wrapper learns the upstream status only as text, so a caller that
//! wants to tell "the operator must fix the credential" from "the server
//! failed" would have to read that text. The wrapper keeps this type in the
//! error it returns instead, the same way it keeps a local refusal.

use std::fmt;

/// The code a surface reports for this failure.
pub const PROVIDER_AUTH_FAILED: &str = "provider_auth_failed";

/// A provider answered 401 or 403 to the credential it was sent.
///
/// It carries the provider id and the status, and nothing the provider wrote:
/// the upstream body can echo the credential back, and the id is stripped of
/// URL userinfo and query because a `custom:` id is a URL the operator typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderAuthError {
    provider: String,
    status: u16,
}

impl ProviderAuthError {
    pub(crate) fn new(provider: &str, status: u16) -> Self {
        Self {
            provider: without_userinfo_and_query(provider),
            status,
        }
    }

    /// What happened, without the code: the sentence an operator reads.
    pub(crate) fn sentence(&self) -> String {
        format!(
            "provider {} rejected the credential (HTTP {})",
            self.provider, self.status
        )
    }
}

/// The text names the code too: a turn that streams its failure carries one
/// string and no separate code field, so the code travels in the text.
impl fmt::Display for ProviderAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PROVIDER_AUTH_FAILED}: {}", self.sentence())
    }
}

impl std::error::Error for ProviderAuthError {}

/// The provider authentication failure `error`, or any error it wraps, carries.
pub(crate) fn provider_auth_failure(error: &anyhow::Error) -> Option<&ProviderAuthError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ProviderAuthError>())
}

/// `id` with the `user:password@` part of a URL authority and any query or
/// fragment removed. An id that is not a URL comes back as it was.
fn without_userinfo_and_query(id: &str) -> String {
    let id = id.split(['?', '#']).next().unwrap_or(id);
    let Some(scheme_end) = id.find("://") else {
        return id.to_string();
    };
    let authority_start = scheme_end + "://".len();
    let rest = &id[authority_start..];
    let authority_len = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_len].rfind('@') {
        Some(at) => format!(
            "{}{}{}",
            &id[..authority_start],
            &rest[at + 1..authority_len],
            &rest[authority_len..]
        ),
        None => id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_names_the_code_the_provider_and_the_status() {
        let error = ProviderAuthError::new("openrouter", 401);

        assert_eq!(
            error.to_string(),
            "provider_auth_failed: provider openrouter rejected the credential (HTTP 401)"
        );
        assert_eq!(
            error.sentence(),
            "provider openrouter rejected the credential (HTTP 401)"
        );
    }

    #[test]
    fn a_url_id_loses_its_userinfo_and_query() {
        for (id, shown) in [
            (
                "custom:https://user:hunter2@api.example.com/v1",
                "custom:https://api.example.com/v1",
            ),
            (
                "custom:https://api.example.com/v1?api_key=hunter2",
                "custom:https://api.example.com/v1",
            ),
            (
                "custom:http://127.0.0.1:8080",
                "custom:http://127.0.0.1:8080",
            ),
            ("openrouter", "openrouter"),
        ] {
            let text = ProviderAuthError::new(id, 403).sentence();
            assert!(text.contains(shown), "{id}: {text}");
            assert!(!text.contains("hunter2"), "{id}: {text}");
        }
    }

    #[test]
    fn the_failure_is_found_through_added_context() {
        let error =
            anyhow::Error::new(ProviderAuthError::new("openrouter", 401)).context("turn failed");

        assert_eq!(provider_auth_failure(&error).map(|e| e.status), Some(401));
        assert!(provider_auth_failure(&anyhow::anyhow!("something else")).is_none());
    }
}
