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

/// Whether a WebSocket close code from Discord's gateway means the connection
/// will never succeed with this configuration.
///
/// Discord does not answer a revoked token with an HTTP status — the handshake
/// succeeds and the gateway closes the socket after `IDENTIFY` with `4004`. A
/// listener that treats every close as "reconnect" therefore reconnects into
/// the same rejection forever at the supervisor's initial backoff.
///
/// The listed codes are the ones whose remedy is an operator change (a new
/// token, a different intent set, a shard count), not time:
/// 4004 authentication failed, 4010 invalid shard, 4011 sharding required,
/// 4012 invalid API version, 4013 invalid intents, 4014 disallowed intents.
/// Everything else — 4000 unknown error, 4007 invalid seq, 4009 session timed
/// out, a normal 1000/1001 — is a reconnect, and must stay one.
pub(crate) fn discord_close_is_fatal(code: u16) -> bool {
    matches!(code, 4004 | 4010 | 4011 | 4012 | 4013 | 4014)
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

    /// Discord hard-codes `https://discord.com/api/v10/...` at every call site
    /// and its gateway is a live WebSocket, so its listener cannot be pointed at
    /// a local server — the same limitation Slack has above, for the same
    /// reason, and a base-URL seam is its own change. Until then, assert the
    /// wiring by reading. Each needle is one of the four ways this listener used
    /// to report a fault as a clean exit.
    #[test]
    fn the_discord_listener_reports_its_faults_to_the_supervisor() {
        let src = include_str!("discord.rs");
        // Assembled at runtime so these assertions cannot count themselves.
        for (needle, why) in [
            (
                format!("fault::is_fatal_auth_{}(status)", "status"),
                "a revoked bot token answers 401 on the gateway lookup",
            ),
            (
                format!("fault::discord_close_is_{}(code)", "fatal"),
                "4004 arrives as a close frame, not an HTTP status",
            ),
            (
                format!("Some(Err(e)) => return {}(", "Err"),
                "a read error is a transport fault, not something to poll past",
            ),
            (
                format!("write.send(Message::{}(None))", "Close"),
                "cancellation owes Discord a close frame",
            ),
        ] {
            assert!(src.contains(needle.as_str()), "{why} (missing: {needle})");
        }
    }

    /// Lark's WS endpoint is issued per-connection by Feishu and its callback
    /// host is hard-coded, so the WS half is unreachable from a test. The
    /// webhook half *is* covered behaviourally
    /// (`the_webhook_listener_stops_when_the_token_is_cancelled`); this pins the
    /// two properties of the WS half that a test cannot reach.
    #[test]
    fn the_lark_websocket_listener_honours_cancellation_and_reports_read_faults() {
        let src = include_str!("lark.rs");
        let cancel_arm = format!("() = cancel.{}() => {{", "cancelled");
        let close_frame = format!("write.send(WsMsg::{}(None))", "Close");
        let read_fault = format!(
            "Some(Err(e)) => {{\n                            return {}(",
            "Err"
        );
        assert!(
            src.contains(cancel_arm.as_str()),
            "the WS loop must select on the shutdown token"
        );
        assert!(
            src.contains(close_frame.as_str()),
            "cancellation owes Feishu a close frame; dropping the socket leaves \
             the long connection open until its own timeout"
        );
        assert!(
            src.contains(read_fault.as_str()),
            "a WS read error must return Err, or the supervisor resets its backoff"
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
    fn discord_configuration_close_codes_are_fatal() {
        for code in [4004, 4010, 4011, 4012, 4013, 4014] {
            assert!(
                discord_close_is_fatal(code),
                "{code} needs an operator change, not a retry"
            );
        }
    }

    #[test]
    fn discord_reconnect_close_codes_are_transient() {
        // 4009 (session timed out) and 4007 (invalid seq) are the ones Discord
        // sends most often and are exactly what a reconnect is for; treating
        // them as fatal would give up on a healthy bot.
        for code in [
            1000, 1001, 1006, 4000, 4001, 4002, 4003, 4005, 4007, 4008, 4009,
        ] {
            assert!(
                !discord_close_is_fatal(code),
                "{code} must stay a reconnect"
            );
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
