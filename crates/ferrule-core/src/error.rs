use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("provider error: {0}")]
    Provider(String),
    /// A provider failure that may pass if the call is repeated: a timeout,
    /// a dropped connection, HTTP 408, 429 or 5xx. The agent retries these
    /// with backoff; any other provider error is final.
    #[error("provider temporarily unavailable: {message}")]
    Transient { message: String, retry_after: Option<std::time::Duration> },
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
    /// The run was stopped before it finished: it went in circles (see
    /// `crate::stuck`) or its check kept failing, and even the status
    /// answer couldn't be had.
    #[error("agent stopped before finishing: {0}")]
    Stopped(String),
    #[error("run aborted: {0}")]
    Aborted(String),
}

impl CoreError {
    pub fn is_transient(&self) -> bool {
        matches!(self, CoreError::Transient { .. })
    }
}
