//! What the channel wizard reports when a connection test does not succeed.
//!
//! The wizard used to answer every failure with "check your token", whether
//! the service rejected the token, answered with some other status, or was
//! never reached. The test now records what it observed as a
//! [`ConnectionFailure`], and [`connection_failure_message`] turns that into
//! one line that names only what was observed.

use std::error::Error as _;

/// What a channel connection test observed when it did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectionFailure {
    /// The service answered with a non-success HTTP status. `detail` is the
    /// reason the response body gave, when it gave one.
    Status { code: u16, detail: Option<String> },
    /// The service answered with a success status but its body reported a
    /// failure.
    Reported(String),
    /// The request never completed. Holds the transport error text with the
    /// request URL removed: a channel URL can carry the bot token.
    Transport(String),
    /// The thread running the test panicked.
    Panicked,
}

impl From<reqwest::Error> for ConnectionFailure {
    fn from(err: reqwest::Error) -> Self {
        // A reqwest error renders as "error sending request for url (...)",
        // and the Telegram URL holds `bot{token}`. Drop the URL, then follow
        // the source chain for the cause ("connection refused", "dns error").
        let err = err.without_url();
        let mut text = err.to_string();
        let mut source = err.source();
        while let Some(cause) = source {
            text.push_str(": ");
            text.push_str(&cause.to_string());
            source = cause.source();
        }
        Self::Transport(text)
    }
}

impl ConnectionFailure {
    /// `resp` when its status is a success, else the status as a failure.
    pub(crate) fn require_success(
        resp: reqwest::blocking::Response,
    ) -> Result<reqwest::blocking::Response, Self> {
        let status = resp.status();
        if status.is_success() {
            Ok(resp)
        } else {
            Err(Self::Status {
                code: status.as_u16(),
                detail: None,
            })
        }
    }

    /// The outcome of a joined connection-test thread: its own result, or
    /// [`Self::Panicked`] when the thread panicked.
    pub(crate) fn joined<T>(
        thread_result: std::thread::Result<Result<T, Self>>,
    ) -> Result<T, Self> {
        thread_result.unwrap_or(Err(Self::Panicked))
    }
}

/// One line for `failure`. `credential` names what the operator typed ("bot
/// token", "API token") and is used only when the service rejected it (HTTP
/// 401 or 403). Nothing here guesses a cause that was not observed.
pub(crate) fn connection_failure_message(credential: &str, failure: &ConnectionFailure) -> String {
    let with_detail = |detail: &Option<String>| match detail {
        Some(detail) => format!(
            ": {}",
            crate::memory::sanitize::sanitize_for_terminal(detail)
        ),
        None => String::new(),
    };
    match failure {
        ConnectionFailure::Status { code, detail } if matches!(*code, 401 | 403) => format!(
            "Connection failed: the service rejected the {credential} (HTTP {code}){}",
            with_detail(detail)
        ),
        ConnectionFailure::Status { code, detail } => format!(
            "Connection failed: the service answered HTTP {code}{}",
            with_detail(detail)
        ),
        ConnectionFailure::Reported(detail) => format!(
            "Connection failed: the service reported: {}",
            crate::memory::sanitize::sanitize_for_terminal(detail)
        ),
        ConnectionFailure::Transport(text) => format!("Connection failed: {text}"),
        ConnectionFailure::Panicked => {
            "Connection failed: the connection test stopped unexpectedly".to_string()
        }
    }
}

/// Replace the "Testing connection..." line with the failure message.
pub(crate) fn print_connection_failure(credential: &str, failure: &ConnectionFailure) {
    println!(
        "\r  {} {}",
        console::style("❌").red().bold(),
        connection_failure_message(credential, failure)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16) -> ConnectionFailure {
        ConnectionFailure::Status { code, detail: None }
    }

    #[test]
    fn an_auth_status_says_the_credential_was_rejected() {
        for code in [401, 403] {
            let message = connection_failure_message("bot token", &status(code));
            assert!(message.contains("rejected the bot token"), "{message}");
            assert!(message.contains(&format!("HTTP {code}")), "{message}");
        }
    }

    #[test]
    fn another_status_is_printed_without_blaming_the_credential() {
        let message = connection_failure_message("bot token", &status(500));
        assert!(message.contains("HTTP 500"), "{message}");
        assert!(!message.contains("bot token"), "{message}");
        assert!(!message.contains("rejected"), "{message}");
    }

    #[test]
    fn a_status_detail_from_the_response_is_kept() {
        let failure = ConnectionFailure::Status {
            code: 400,
            detail: Some("app secret invalid".into()),
        };
        let message = connection_failure_message("credentials", &failure);
        assert!(
            message.contains("HTTP 400: app secret invalid"),
            "{message}"
        );
    }

    #[test]
    fn a_reported_failure_is_quoted_as_the_service_reported_it() {
        let message = connection_failure_message(
            "credentials",
            &ConnectionFailure::Reported("invalid param".into()),
        );
        assert!(
            message.contains("the service reported: invalid param"),
            "{message}"
        );
        assert!(!message.contains("check your"), "{message}");
    }

    #[test]
    fn a_panicked_test_is_reported_as_such() {
        let message = connection_failure_message("token", &ConnectionFailure::Panicked);
        assert!(message.contains("stopped unexpectedly"), "{message}");
        let joined: Result<(), ConnectionFailure> = ConnectionFailure::joined(
            std::thread::spawn(|| -> Result<(), ConnectionFailure> { panic!("fixture panic") })
                .join(),
        );
        assert_eq!(joined, Err(ConnectionFailure::Panicked));
    }

    /// A transport error names the cause and carries neither the request URL
    /// nor the token in it. The URL is the Telegram shape, `bot{token}` in
    /// the path, and the port is closed so the request fails to connect.
    #[test]
    fn a_transport_error_carries_the_cause_and_no_token() {
        let token = "123456:fixture-secret-token";
        let url = format!("http://127.0.0.1:1/bot{token}/getMe");
        let err = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .expect("client builds without proxy settings")
            .get(&url)
            .send()
            .expect_err("nothing listens on port 1");

        let failure = ConnectionFailure::from(err);
        let message = connection_failure_message("bot token", &failure);

        assert!(message.starts_with("Connection failed: "), "{message}");
        assert!(!message.contains(token), "{message}");
        assert!(!message.contains("/bot"), "{message}");
        assert!(!message.contains("check your"), "{message}");
        assert!(matches!(failure, ConnectionFailure::Transport(ref t) if !t.is_empty()));
    }
}
