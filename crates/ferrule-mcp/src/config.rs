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
    /// Per-call timeout for `tools/call`. Defaults to 60s if unset. Startup
    /// gets at least 60s regardless, see [`Self::startup_timeout`].
    pub timeout_secs: Option<u64>,
}

impl McpServerConfig {
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs.unwrap_or(60))
    }

    /// Spawn + `initialize` + `tools/list`. Never shorter than 60s: a short
    /// per-call timeout shouldn't fail a server that is slow to start (an
    /// `npx` first run, a cold Python on Windows).
    pub fn startup_timeout(&self) -> std::time::Duration {
        self.timeout().max(std::time::Duration::from_secs(60))
    }
}
