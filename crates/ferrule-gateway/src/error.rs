use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("core error: {0}")]
    Core(#[from] ferrule_core::CoreError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("channel `{0}` not registered")]
    UnknownChannel(String),
    #[error("session `{0}` queue closed")]
    SessionClosed(String),
    #[error("{0} not supported by this channel")]
    Unsupported(&'static str),
    #[error("channel error: {0}")]
    Channel(String),
}
