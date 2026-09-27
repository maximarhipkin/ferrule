//! M37: a connection whose tools run inside ferrule rather than on an MCP
//! server — Jira's REST API with an API token, Gmail over IMAP/SMTP with an
//! app password, Google's REST APIs with a service account. They go through
//! the same client as any server, so they are named, gated, hot-reloaded
//! and read-only-hinted exactly like one; only the transport is a call.
//! The credential stays in-process and never reaches the model.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::fmt;
use std::sync::Arc;

/// Answers `tools/list` and `tools/call` without a process or a socket.
#[async_trait]
pub trait LocalServer: Send + Sync {
    /// Stable for one connection, as [`crate::auth::CredentialSource::id`]:
    /// two configs with the same id are the same server.
    fn id(&self) -> String;
    /// Each tool as MCP lists it: `name`, `description`, `inputSchema`,
    /// `annotations`.
    fn tools(&self) -> Vec<Value>;
    /// Runs `name`. `Err` is the text of a failed call (an `isError`
    /// result the model reads), never a credential.
    async fn call(&self, name: &str, arguments: Value) -> Result<String, String>;
}

/// `McpServerConfig::local`: a shared [`LocalServer`].
#[derive(Clone)]
pub struct Local(pub Arc<dyn LocalServer>);

impl fmt::Debug for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Local({})", self.0.id())
    }
}

impl PartialEq for Local {
    fn eq(&self, other: &Self) -> bool {
        self.0.id() == other.0.id()
    }
}

impl Local {
    /// A JSON-RPC `result` for `method`, as a server would send it.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, crate::McpError> {
        match method {
            "tools/list" => Ok(json!({ "tools": self.0.tools() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                if !self.0.tools().iter().any(|t| t["name"] == name) {
                    return Err(crate::McpError::Rpc {
                        code: -32602,
                        message: format!("no tool `{name}`"),
                    });
                }
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = match self.0.call(name, args).await {
                    Ok(text) => (text, false),
                    Err(text) => (text, true),
                };
                Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
            }
            other => Err(crate::McpError::Rpc {
                code: -32601,
                message: format!("`{other}` isn't supported here"),
            }),
        }
    }
}
