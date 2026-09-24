use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentsError {
    #[error("agents.db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Core(#[from] ferrule_core::CoreError),
    /// A limit refused the request; the text says which and what to do.
    #[error("{0}")]
    Limit(String),
    /// The request itself doesn't make sense (an unknown id, not your
    /// child, a child that's still running).
    #[error("{0}")]
    Invalid(String),
    /// The embedding program couldn't build the agent.
    #[error("couldn't build the agent: {0}")]
    Build(String),
}
