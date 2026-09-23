use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("provider returned malformed response: {0}")]
    MalformedResponse(String),
    #[error("tool `{0}` not found")]
    ToolNotFound(String),
    #[error("tool `{tool}` failed: {message}")]
    ToolFailed { tool: String, message: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("agent exceeded max iterations ({0})")]
    MaxIterations(usize),
    #[error("run aborted: {0}")]
    Aborted(String),
}
