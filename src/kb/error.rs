use thiserror::Error;

pub type KbResult<T> = Result<T, KbError>;

#[derive(Debug, Error)]
pub enum KbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

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
