use thiserror::Error;

#[derive(Debug, Error)]
pub enum KanbanError {
    #[error("invalid board slug {0:?}: must be 1-64 chars, lowercase alphanumerics / hyphens / underscores, not starting with '-' or '_'")]
    InvalidBoardSlug(String),

    #[error("board {0:?} does not exist")]
    UnknownBoard(String),

    #[error(
        "invalid task status {0:?}: must be one of triage|todo|ready|running|blocked|done|archived"
    )]
    InvalidStatus(String),

    #[error("invalid workspace kind {0:?}: must be one of scratch|worktree|dir")]
    InvalidWorkspaceKind(String),

    #[error("unknown parent task(s): {0}")]
    UnknownParents(String),

    #[error("task {0:?} not found")]
    UnknownTask(String),

    #[error("title is required")]
    MissingTitle,

    #[error("hallucinated child cards on completion: {0:?}")]
    HallucinatedCards(Vec<String>),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("rusqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, KanbanError>;
