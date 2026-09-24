use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExtError {
    /// A policy refusal: the allow-list, a pin, a name, the scan. Shown to
    /// the model as is, so it never carries flagged text.
    #[error("refused: {0}")]
    Refused(String),
    /// The lockfile is held by someone else past the wait, or unreadable.
    #[error("extensions lock: {0}")]
    Lock(String),
    #[error("git: {0}")]
    Git(String),
    #[error("mcp: {0}")]
    Mcp(#[from] ferrule_mcp::McpError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, ExtError>;

pub(crate) fn refused(msg: impl Into<String>) -> ExtError {
    ExtError::Refused(msg.into())
}
