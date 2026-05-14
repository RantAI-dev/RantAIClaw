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

    #[error("invalid config: {0}")]
    Config(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("unsupported file type: {0}")]
    UnsupportedFileType(String),

    #[error("extraction failed (extractor={extractor}): {message}")]
    Extraction { extractor: String, message: String },

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("other: {0}")]
    Other(String),
}
