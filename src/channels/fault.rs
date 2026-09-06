//! Which polling faults the supervisor must be told about.
//!
//! `Channel::listen`'s contract (see `traits.rs`) is that a transport or auth
//! fault returns `Err`, and `supervisor::spawn_supervised_listener` doubles its
//! backoff only when it sees one. A listener that swallows every error to
//! `continue` keeps the supervisor resetting to the initial delay, so a revoked
//! token becomes a poll-rate reconnect storm against the platform instead of an
//! escalating retry — and `health_check` still reports green, because it asks a
//! different question.
//!
//! The distinction the polling channels can actually make is the HTTP status:
//! 401/403 mean the credential is wrong and no amount of retrying fixes it,
//! while a connection reset or a 5xx is exactly what backoff exists for.

/// Whether an HTTP status from a channel's poll means "this credential will
/// never work" rather than "try again".
///
/// Deliberately narrow. Anything else — 5xx, 429, a timeout, a reset — is
/// transient and stays a `continue`, because turning a blip into a give-up is
/// the failure mode in the other direction.
pub(crate) fn is_fatal_auth_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    )
}

/// Slack answers `200 OK` with `{"ok": false, "error": "invalid_auth"}` rather
/// than a 4xx, so the status alone cannot classify it.
///
/// Only the errors that mean the token is finished are listed; a transient
/// Slack error (`ratelimited`, `service_unavailable`) must not match, or a
/// rate-limit would be treated as a dead credential.
pub(crate) fn slack_error_is_fatal(error: &str) -> bool {
    matches!(
        error,
        "invalid_auth"
            | "not_authed"
            | "account_inactive"
            | "token_revoked"
            | "token_expired"
            | "missing_scope"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test per half of each classifier: "recognises the fatal ones" alone
    // would pass a function that returns true for everything, which would turn
    // every rate-limit into a listener that gives up.

    #[test]
    fn unauthorized_and_forbidden_are_fatal() {
        assert!(is_fatal_auth_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(is_fatal_auth_status(reqwest::StatusCode::FORBIDDEN));
    }

    #[test]
    fn server_errors_and_rate_limits_are_transient() {
        for s in [
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            reqwest::StatusCode::BAD_GATEWAY,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::REQUEST_TIMEOUT,
            reqwest::StatusCode::OK,
        ] {
            assert!(!is_fatal_auth_status(s), "{s} must stay retryable");
        }
    }

    /// Slack hard-codes `https://slack.com/api/...` at every call site, so its
    /// listener cannot be pointed at a local server and there is no routed test
    /// for it the way there is for Mattermost. Adding a base-URL seam is its own
    /// change (the same call plan 310 made). Until then, assert the wiring by
    /// reading: the poll must check BOTH the HTTP status and the `ok: false`
    /// body, because Slack answers `200 OK` for a revoked token.
    #[test]
    fn the_slack_listener_checks_both_the_status_and_the_body() {
        let src = include_str!("slack.rs");
        // Assembled at runtime so this assertion does not count itself.
        let status_check = format!("is_fatal_auth_{}(resp.status())", "status");
        let body_check = format!("slack_error_is_{}(err)", "fatal");
        assert!(
            src.contains(status_check.as_str()),
            "the Slack poll must classify the HTTP status"
        );
        assert!(
            src.contains(body_check.as_str()),
            "the Slack poll must classify the ok:false body — a revoked token \
             comes back as 200"
        );
    }

    #[test]
    fn slack_credential_errors_are_fatal() {
        for e in [
            "invalid_auth",
            "not_authed",
            "account_inactive",
            "token_revoked",
            "token_expired",
            "missing_scope",
        ] {
            assert!(slack_error_is_fatal(e), "{e} must stop the listener");
        }
    }

    #[test]
    fn slack_transient_errors_are_not_fatal() {
        for e in [
            "ratelimited",
            "service_unavailable",
            "fatal_error",
            "channel_not_found",
            "",
        ] {
            assert!(!slack_error_is_fatal(e), "{e} must stay retryable");
        }
    }
}
