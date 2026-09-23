use serde::Deserialize;
use std::collections::HashMap;

/// `[[mcp.servers]]` entry in `ferrule.toml`. One entry spawns one stdio MCP
/// server whose tools all become ferrule tools named `mcp__<name>__<tool>`.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    /// Namespace for this server's tools; must be stable across restarts.
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Per-call timeout. Defaults to 60s if unset.
    pub timeout_secs: Option<u64>,
}

impl McpServerConfig {
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs.unwrap_or(60))
    }
}
