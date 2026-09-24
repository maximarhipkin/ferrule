use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// `[[mcp.servers]]` entry in `ferrule.toml`: a command to spawn and speak
/// to over stdio, or the URL of a server speaking Streamable HTTP. Its
/// tools all become ferrule tools named `mcp__<name>__<tool>`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct McpServerConfig {
    /// Namespace for this server's tools; must be stable across restarts.
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// A remote server's endpoint, instead of `command`. HTTPS goes through
    /// the credential proxy when there is one.
    #[serde(default)]
    pub url: Option<String>,
    /// Sent with every request to `url`. `${VAR}` expands to the variable
    /// as a sandboxed command would see it, so a `[secrets]` entry arrives
    /// as its placeholder and the proxy swaps the real value in for the
    /// hosts it is bound to.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Per-call timeout for `tools/call`. Defaults to 60s if unset. Startup
    /// gets at least 60s regardless, see [`Self::startup_timeout`].
    pub timeout_secs: Option<u64>,
    /// Run this server through the OS sandbox like shell commands: it can
    /// write the workspace, temp dirs, `[sandbox] writable_roots` and its
    /// own state dir (even in read-only mode), and always has the network.
    /// `false` is an escape hatch — logged loudly and flagged by `doctor`,
    /// since the server can then write anywhere its OS user can and read
    /// ferrule's secrets file. Secret env is scrubbed either way.
    #[serde(default = "default_true")]
    pub sandbox: bool,
    /// Extra writable paths for this server only. `~/` is the home dir;
    /// relative paths are relative to the workspace. Missing ones are
    /// skipped, same as `[sandbox] writable_roots`. Ignored when `sandbox =
    /// false`.
    #[serde(default)]
    pub writable_roots: Vec<PathBuf>,
    /// Variables removed from the server's environment before `env` is
    /// applied, e.g. `CI`, which some servers read as "turn safety off".
    /// A trailing `*` removes every inherited variable with that prefix.
    #[serde(default)]
    pub env_remove: Vec<String>,
    /// Tool arguments the model never sees and can't send: removed from
    /// every tool's input schema and dropped from each call's arguments.
    /// For arguments that would let the model loosen what the owner set.
    #[serde(default)]
    pub hide_args: Vec<String>,
    /// Offer only these of the server's tools: exact names, or a prefix
    /// ending in `*`. Empty: all of them. A tool left out is never
    /// scanned, offered or called.
    #[serde(default)]
    pub enabled_tools: Vec<String>,
    /// Cap on one tool result from this server, in chars. Only ever
    /// tightens the session's own cap.
    #[serde(default)]
    pub max_output_chars: Option<usize>,
    /// Caps for single tools (remote names), over `max_output_chars`.
    #[serde(default)]
    pub output_caps: HashMap<String, usize>,
    /// macOS: open the Seatbelt profile to the system's Mach/XPC services
    /// (see `Sandbox::with_desktop_services`). Set only for the built-in
    /// browser; not reachable from a config file.
    #[serde(skip)]
    pub desktop_services: bool,
    /// Arguments for a short run of the same command, env and sandbox, with
    /// no stdio, before each tool call. Set only for the built-in browser
    /// on Windows (see `browser::WINDOWS_WARM_UP`); not reachable from a
    /// config file.
    #[serde(skip)]
    pub warm_up: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            timeout_secs: None,
            sandbox: true,
            writable_roots: Vec::new(),
            env_remove: Vec::new(),
            hide_args: Vec::new(),
            enabled_tools: Vec::new(),
            max_output_chars: None,
            output_caps: HashMap::new(),
            desktop_services: false,
            warm_up: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
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

    /// Whether `enabled_tools` lets the tool named `tool` (its remote
    /// name) through.
    pub fn tool_enabled(&self, tool: &str) -> bool {
        self.enabled_tools.is_empty()
            || self
                .enabled_tools
                .iter()
                .any(|p| match p.strip_suffix('*') {
                    Some(prefix) => tool.starts_with(prefix),
                    None => p == tool,
                })
    }

    /// The cap on `tool`'s results given the session's own: the smallest
    /// of the three.
    pub fn output_cap(&self, tool: &str, session: usize) -> usize {
        let own = self
            .output_caps
            .get(tool)
            .copied()
            .or(self.max_output_chars);
        own.map_or(session, |c| c.min(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_tools_match_names_and_prefixes() {
        let mut cfg = McpServerConfig::default();
        assert!(cfg.tool_enabled("anything"));
        cfg.enabled_tools = vec!["search_*".into(), "get_issue".into()];
        assert!(cfg.tool_enabled("search_code"));
        assert!(cfg.tool_enabled("get_issue"));
        assert!(!cfg.tool_enabled("get_issues"));
        assert!(!cfg.tool_enabled("delete_repo"));
    }

    #[test]
    fn caps_only_tighten() {
        let mut cfg = McpServerConfig::default();
        assert_eq!(cfg.output_cap("a", 1000), 1000);
        cfg.max_output_chars = Some(500);
        assert_eq!(cfg.output_cap("a", 1000), 500);
        cfg.output_caps.insert("a".into(), 50);
        assert_eq!(cfg.output_cap("a", 1000), 50);
        assert_eq!(cfg.output_cap("b", 1000), 500);
        cfg.output_caps.insert("c".into(), 5000);
        assert_eq!(cfg.output_cap("c", 1000), 1000);
    }

    #[test]
    fn filters_and_caps_parse() {
        let cfg: McpServerConfig = serde_json::from_value(serde_json::json!({
            "name": "x", "command": "y", "enabled_tools": ["a"],
            "max_output_chars": 10, "output_caps": {"a": 5},
        }))
        .unwrap();
        assert_eq!(cfg.enabled_tools, ["a"]);
        assert_eq!(cfg.output_cap("a", 100), 5);
    }
}
