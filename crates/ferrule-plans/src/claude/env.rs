//! The `claude` child's argv and environment, in one place (design §7.3).
//!
//! Anything that outranks a plan's credential in Claude Code (an API key,
//! an auth token, a cloud provider, a base URL) would silently turn a plan
//! turn into an API-billed one, so all of it is removed, whatever the
//! sandbox's scrubbing says; the plan's token, when ferrule holds one, is
//! set after. Never `--bare`: it skips the OAuth and keychain reads, so a
//! plan can't sign in under it.

use super::token::{Token, TOKEN_VAR};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// Removed by name, whatever else is in the environment.
pub const REMOVED: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "AWS_BEARER_TOKEN_BEDROCK",
    TOKEN_VAR,
    "CLAUDE_CONFIG_DIR",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_AGENT_SDK_VERSION",
];

/// Also removed: every variable with one of these prefixes (Claude Code's
/// own settings and session markers, and any Anthropic routing).
pub const REMOVED_PREFIXES: &[&str] = &["ANTHROPIC_", "CLAUDE_CODE_"];

/// The variables that, set in a shell, would bill the API instead of the
/// plan for a `claude` run by hand. Doctor warns about them.
pub const OUTRANKING: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "ANTHROPIC_BASE_URL",
];

/// Claude's built-in tools that only read: allowed without asking.
pub const READ_ONLY_TOOLS: &[&str] = &["Read", "Glob", "Grep", "LS", "TodoWrite"];
/// Claude's tools that change files.
pub const WRITE_TOOLS: &[&str] = &["Edit", "MultiEdit", "Write", "NotebookEdit"];
/// The bridge's MCP server name, so its tools are `mcp__ferrule__*`.
pub const BRIDGE_SERVER: &str = "ferrule";
/// The permission-prompt tool.
pub const APPROVE_TOOL: &str = "mcp__ferrule__approve";

/// What one turn's `claude` is started with.
#[derive(Debug, Clone)]
pub struct Launch {
    pub model: String,
    pub resume: Option<String>,
    /// A file with ferrule's system prompt, appended to Claude Code's own.
    pub system_prompt_file: Option<PathBuf>,
    /// The bridge's `--mcp-config` file.
    pub mcp_config: Option<PathBuf>,
    pub config_dir: PathBuf,
    /// The plan's token, when ferrule holds one (exported or pasted).
    pub token: Option<Token>,
    /// `--disallowedTools`, beyond the defaults.
    pub disallowed: Vec<String>,
    /// Have claude strip its credentials from its own subprocesses
    /// (`CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`).
    pub scrub_subprocess_env: bool,
}

/// The argv and the environment changes, to apply to a `Command`.
#[derive(Clone, PartialEq)]
pub struct ChildSpec {
    pub args: Vec<String>,
    pub remove: Vec<String>,
    set: Vec<(String, String)>,
}

impl std::fmt::Debug for ChildSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSpec")
            .field("args", &self.args)
            .field("remove", &self.remove)
            .field("set", &self.set_redacted())
            .finish()
    }
}

impl ChildSpec {
    /// The variables set in the child, the token's value blanked: what a
    /// log or a test may look at.
    pub fn set_redacted(&self) -> Vec<(String, String)> {
        self.set
            .iter()
            .map(|(k, v)| {
                let v = if k == TOKEN_VAR { "<redacted>" } else { v };
                (k.clone(), v.to_string())
            })
            .collect()
    }

    /// The argv for a log line. Nothing secret is ever in it (the token is
    /// only in the environment, the prompt on stdin).
    pub fn argv_line(&self) -> String {
        self.args.join(" ")
    }

    pub fn apply(&self, cmd: &mut tokio::process::Command) {
        self.apply_std(cmd.as_std_mut());
    }

    /// [`ChildSpec::apply`] on a std `Command`: the environment only.
    pub fn apply_std(&self, cmd: &mut std::process::Command) {
        for name in &self.remove {
            cmd.env_remove(name);
        }
        for (k, v) in &self.set {
            cmd.env(k, v);
        }
    }
}

/// The child's argv and environment for `launch`, given the parent's
/// variable names (`std::env::vars_os`'s keys in real use).
pub fn spec(launch: &Launch, parent_vars: impl IntoIterator<Item = String>) -> ChildSpec {
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--model",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(launch.model.clone());
    if let Some(id) = &launch.resume {
        args.push("--resume".into());
        args.push(id.clone());
    }
    if let Some(f) = &launch.system_prompt_file {
        args.push("--append-system-prompt-file".into());
        args.push(f.display().to_string());
    }
    args.extend(["--setting-sources", "user", "--strict-mcp-config"].map(String::from));
    if let Some(f) = &launch.mcp_config {
        args.push("--mcp-config".into());
        args.push(f.display().to_string());
        args.extend(["--permission-prompt-tool", APPROVE_TOOL].map(String::from));
    }
    args.extend(["--permission-mode", "default"].map(String::from));
    let mut allowed: Vec<String> = READ_ONLY_TOOLS.iter().map(|s| s.to_string()).collect();
    if launch.mcp_config.is_some() {
        allowed.push(format!("mcp__{BRIDGE_SERVER}__*"));
    }
    args.push("--allowedTools".into());
    args.push(allowed.join(","));
    if !launch.disallowed.is_empty() {
        args.push("--disallowedTools".into());
        args.push(launch.disallowed.join(","));
    }

    let mut spec = command(args, &launch.config_dir, launch.token.as_ref(), parent_vars);
    if launch.scrub_subprocess_env {
        spec.set
            .push(("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB".into(), "1".into()));
    }
    spec
}

/// Any other `claude` run (`--version`, `auth status`, `auth login`): the
/// same environment as a turn's, with `args`.
pub fn command(
    args: Vec<String>,
    config_dir: &Path,
    token: Option<&Token>,
    parent_vars: impl IntoIterator<Item = String>,
) -> ChildSpec {
    let mut remove: Vec<String> = REMOVED.iter().map(|s| s.to_string()).collect();
    for name in parent_vars {
        if REMOVED_PREFIXES.iter().any(|p| name.starts_with(p)) && !remove.contains(&name) {
            remove.push(name);
        }
    }
    let mut set = vec![
        (
            "CLAUDE_CONFIG_DIR".to_string(),
            config_dir.display().to_string(),
        ),
        ("DISABLE_AUTOUPDATER".into(), "1".into()),
        (
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
            "1".into(),
        ),
    ];
    if let Some(t) = token {
        set.push((TOKEN_VAR.into(), t.expose().to_string()));
    }
    ChildSpec { args, remove, set }
}

/// The engine refuses to start a turn when its config dir's settings set
/// an `apiKeyHelper`: claude would bill whatever key it prints.
pub fn check_config_dir(dir: &Path) -> Result<()> {
    let settings = dir.join("settings.json");
    let Ok(text) = std::fs::read_to_string(&settings) else {
        return Ok(());
    };
    let helper = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("apiKeyHelper").cloned())
        .is_some_and(|v| !v.is_null());
    if helper {
        bail!(
            "{} sets an apiKeyHelper, which would bill an API key instead of the Claude plan; remove it",
            settings.display()
        );
    }
    Ok(())
}

/// The permission and hook settings in the config dir's `settings.json`
/// that would route around ferrule's gates, for doctor.
pub fn risky_settings(dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(dir.join("settings.json")) else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return vec!["settings.json doesn't parse".into()];
    };
    let mut out = vec![];
    if v.get("apiKeyHelper").is_some_and(|h| !h.is_null()) {
        out.push("apiKeyHelper".into());
    }
    if v.get("hooks").is_some_and(|h| !h.is_null()) {
        out.push("hooks".into());
    }
    let perms = v.get("permissions");
    if perms
        .and_then(|p| p.get("allow"))
        .and_then(|a| a.as_array())
        .is_some_and(|a| !a.is_empty())
    {
        out.push("permissions.allow".into());
    }
    if perms
        .and_then(|p| p.get("defaultMode"))
        .and_then(|m| m.as_str())
        .is_some_and(|m| m == "bypassPermissions" || m == "acceptEdits")
    {
        out.push("permissions.defaultMode".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> Launch {
        Launch {
            model: "haiku".into(),
            resume: Some("sess-1".into()),
            system_prompt_file: Some("/t/system.md".into()),
            mcp_config: Some("/t/mcp.json".into()),
            config_dir: "/data/claude-code".into(),
            token: Some(Token::new("sk-ant-oat01-SECRET")),
            disallowed: vec!["Bash".into()],
            scrub_subprocess_env: false,
        }
    }

    #[test]
    fn the_child_gets_the_plan_and_nothing_that_outranks_it() {
        let parent = [
            "PATH",
            "HOME",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_MODEL",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "CLAUDECODE",
        ]
        .map(String::from);
        let s = spec(&launch(), parent);
        assert!(!s.args.iter().any(|a| a == "--bare"), "never --bare");
        for flag in [
            "-p",
            "--strict-mcp-config",
            "--include-partial-messages",
            "--verbose",
        ] {
            assert!(s.args.iter().any(|a| a == flag), "{flag}");
        }
        let after = |flag: &str| {
            let i = s.args.iter().position(|a| a == flag).unwrap();
            s.args[i + 1].clone()
        };
        assert_eq!(after("--output-format"), "stream-json");
        assert_eq!(after("--setting-sources"), "user");
        assert_eq!(after("--resume"), "sess-1");
        assert_eq!(after("--model"), "haiku");
        assert_eq!(after("--permission-prompt-tool"), APPROVE_TOOL);
        assert_eq!(after("--permission-mode"), "default");
        assert_eq!(
            after("--allowedTools"),
            "Read,Glob,Grep,LS,TodoWrite,mcp__ferrule__*"
        );
        assert_eq!(after("--disallowedTools"), "Bash");
        assert!(!s.argv_line().contains("SECRET"));

        for name in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_MODEL",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "CLAUDECODE",
            "CLAUDE_CONFIG_DIR",
            TOKEN_VAR,
        ] {
            assert!(s.remove.iter().any(|r| r == name), "{name} removed");
        }
        assert!(!s.remove.iter().any(|r| r == "PATH" || r == "HOME"));
        let set = s.set_redacted();
        let get = |k: &str| set.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get(TOKEN_VAR).as_deref(), Some("<redacted>"));
        assert_eq!(
            get("CLAUDE_CONFIG_DIR").as_deref(),
            Some("/data/claude-code")
        );
        assert_eq!(get("DISABLE_AUTOUPDATER").as_deref(), Some("1"));
        assert!(get("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB").is_none());
        assert!(!format!("{s:?}").contains("SECRET"), "Debug");

        // Claude's own login: no token variable at all, not even an empty one.
        let mut own = launch();
        own.token = None;
        own.resume = None;
        own.scrub_subprocess_env = true;
        let s = spec(&own, []);
        assert!(!s.set_redacted().iter().any(|(k, _)| k == TOKEN_VAR));
        assert!(s.remove.iter().any(|r| r == TOKEN_VAR));
        assert!(!s.args.iter().any(|a| a == "--resume"));
        assert!(s
            .set_redacted()
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB" && v == "1"));
    }

    #[test]
    fn an_api_key_helper_in_the_config_dir_stops_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        check_config_dir(dir.path()).unwrap();
        std::fs::write(dir.path().join("settings.json"), r#"{"model":"haiku"}"#).unwrap();
        check_config_dir(dir.path()).unwrap();
        assert!(risky_settings(dir.path()).is_empty());
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"apiKeyHelper":"echo sk","hooks":{"PreToolUse":[]},"permissions":{"allow":["Bash"],"defaultMode":"bypassPermissions"}}"#,
        )
        .unwrap();
        let e = check_config_dir(dir.path()).unwrap_err().to_string();
        assert!(e.contains("apiKeyHelper"), "{e}");
        assert_eq!(
            risky_settings(dir.path()),
            [
                "apiKeyHelper",
                "hooks",
                "permissions.allow",
                "permissions.defaultMode"
            ]
        );
    }
}
