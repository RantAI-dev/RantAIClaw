use thiserror::Error;

pub type KbResult<T> = Result<T, KbError>;

#[derive(Debug, Error)]
pub enum KbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A transport failure talking to an upstream service. Built only through
    /// [`From<reqwest::Error>`], which drops the request URL: an endpoint may
    /// carry userinfo or a query key, and this error reaches logs and answers.
    #[error("http error: {0}")]
    Http(reqwest::Error),

    #[error("embedding api error: status={status} body={body}")]
    EmbeddingApi { status: u16, body: String },

    #[error("chat completion api error: status={status} body={body}")]
    ChatApi { status: u16, body: String },

    #[error("dimension mismatch: expected={expected} got={got} (chunk_index={index})")]
    DimensionMismatch {
        expected: usize,
        got: usize,
        index: usize,
    },

    #[error("array length mismatch: {kind} ({left} vs {right})")]
    LengthMismatch {
        kind: &'static str,
        left: usize,
        right: usize,
    },

    #[error("invalid config: {0}")]
    Config(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("unsupported file type: {0}")]
    UnsupportedFileType(String),

    /// The extractor read the caller's document and could not make sense of
    /// it: a corrupt or unsupported file. The caller's own input is at fault.
    #[error("extraction failed (extractor={extractor}): {message}")]
    Extraction { extractor: String, message: String },

    /// An extractor service the document was sent to (MinerU, an OCR or
    /// vision endpoint) was unreachable or answered with an error. The
    /// document may be fine; the upstream is not.
    #[error("extractor unavailable (extractor={extractor}): {message}")]
    ExtractorUnavailable { extractor: String, message: String },

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("other: {0}")]
    Other(String),
}

/// The fixed words for a transport failure, in an answer or on the CLI. They
/// name the upstream without its URL, so the endpoint never leaves the process.
pub const UPSTREAM_UNREACHABLE_DETAIL: &str =
    "the upstream model service did not answer; check its base URL and that it is running";

/// Fixed sentences for a rejected API key, in a gateway answer or on the CLI.
/// They name the service and the key and never the upstream body, which can
/// echo the credential back.
pub const EMBEDDING_KEY_REJECTED_DETAIL: &str =
    "the embedding provider rejected the API key; check the key";
pub const CHAT_KEY_REJECTED_DETAIL: &str = "the chat provider rejected the API key; check the key";

/// Whether an upstream answer status means the service refused the API key it
/// was sent. Those answers carry their own code, so the gateway and the
/// console can tell the operator to fix the key instead of reporting a
/// generic upstream failure.
pub fn is_auth_rejection_status(status: u16) -> bool {
    matches!(status, 401 | 403)
}

impl From<reqwest::Error> for KbError {
    /// Keeps the kind of failure (connect, timeout, decode) and drops the URL
    /// that reqwest prints with it.
    fn from(err: reqwest::Error) -> Self {
        KbError::Http(err.without_url())
    }
}

impl KbError {
    /// The error for an extractor that failed on every attempt, built from
    /// the errors of the extractors it tried.
    ///
    /// - An upstream failure when any cause was one.
    /// - A failure of the caller's document when every cause was one.
    /// - Otherwise the first cause that is neither, as itself, so that a
    ///   missing key stays a configuration error and an internal fault stays
    ///   internal instead of being blamed on the document.
    pub fn extraction_failed(extractor: String, message: String, causes: Vec<KbError>) -> Self {
        if causes
            .iter()
            .any(|c| matches!(c, KbError::ExtractorUnavailable { .. }))
        {
            return KbError::ExtractorUnavailable { extractor, message };
        }
        match causes
            .into_iter()
            .find(|c| !matches!(c, KbError::Extraction { .. }))
        {
            Some(other) => other,
            None => KbError::Extraction { extractor, message },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KbError;

    /// A transport error from a request to a closed loopback port, sent to a
    /// URL that carries userinfo and a query key.
    async fn closed_port_request_error() -> reqwest::Error {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind a loopback port")
            .local_addr()
            .expect("local address")
            .port();
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client")
            .get(format!("http://user:secret@127.0.0.1:{port}/v1?key=abc"))
            .send()
            .await
            .expect_err("nothing listens on the port")
    }

    #[tokio::test]
    async fn transport_error_converts_without_the_url_it_was_sent_to() {
        let raw = closed_port_request_error().await;
        assert!(
            raw.to_string().contains("key=abc"),
            "precondition: the raw error names the URL: {raw}"
        );

        let err = KbError::from(raw);
        let text = err.to_string();
        assert!(
            !text.contains("secret") && !text.contains("key=abc") && !text.contains("127.0.0.1"),
            "the KB error names the endpoint URL: {text}"
        );
        assert!(
            matches!(&err, KbError::Http(e) if e.is_connect()),
            "the kind of failure is kept: {err:?}"
        );
    }
}
