use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("failed to spawn mcp server: {0}")]
    Spawn(std::io::Error),
    #[error("mcp io error: {0}")]
    Io(std::io::Error),
    #[error("mcp call timed out")]
    Timeout,
    #[error("mcp connection closed")]
    ConnectionClosed,
    #[error("mcp server not connected")]
    NotConnected,
    #[error("mcp handshake failed: {0}")]
    Handshake(String),
    #[error("mcp rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("mcp response decode error: {0}")]
    Serde(#[from] serde_json::Error),
}
