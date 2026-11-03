//! The `agy` (Antigravity CLI) / `gemini` child's argv, environment, and
//! `.gemini/settings.json`.
//!
//! Anything that outranks a Google OAuth sign-in (`GEMINI_API_KEY`,
//! `GOOGLE_API_KEY`, `GOOGLE_GENAI_USE_VERTEXAI`, etc.) is stripped so a turn
//! never silently bills an API key instead of the user's Google plan.

use crate::claude::env::ChildSpec;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Removed by name from the child environment.
pub const REMOVED: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "GOOGLE_GENAI_USE_GCA",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GEMINI_CLI_HOME",
    "GEMINI_SYSTEM_MD",
    "GEMINI_MODEL",
    "GEMINI_SANDBOX",
];

/// Prefixes removed from the child environment.
pub const REMOVED_PREFIXES: &[&str] = &["GEMINI_"];

/// Variables that, set in a shell, would bill an API key or Vertex AI
/// instead of the Google sign-in when running `agy` / `gemini` by hand.
pub const OUTRANKING: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GENAI_USE_VERTEXAI",
];

/// The bridge's MCP server name in `.gemini/settings.json`.
pub const BRIDGE_SERVER: &str = "ferrule";

/// True when `binary` is Antigravity CLI (`agy`), false only when explicitly
/// pointed at a legacy `gemini` binary.
pub fn is_agy(binary: &Path) -> bool {
    let stem = binary
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("agy")
        .to_ascii_lowercase();
    stem != "gemini"
}

/// Maps legacy Gemini model aliases onto Antigravity CLI's model catalog while
/// passing explicit Antigravity model IDs (`gemini-4-argon-*`, `gemini-3.8-flash-*`,
/// `claude-*`, `gpt-oss-*`) through unchanged.
pub fn normalize_agy_model(model: &str) -> &str {
    match model.trim() {
        "gemini-2.5-pro" | "pro" | "argon" => "gemini-4-argon-medium",
        "gemini-3-pro-preview" | "argon-high" => "gemini-4-argon-high",
        "gemini-2.5-flash" | "flash" => "gemini-3.8-flash-medium",
        "gemini-3-flash-preview" | "flash-high" => "gemini-3.8-flash-high",
        "gemini-2.5-flash-lite" | "flash-low" => "gemini-3.8-flash-low",
        other => other,
    }
}

/// Where `.gemini/` lives for `config_dir`: if `config_dir` is already named
/// `.gemini`, it is used directly; otherwise `<config_dir>/.gemini`.
pub fn dot_gemini(config_dir: &Path) -> PathBuf {
    if config_dir.file_name().is_some_and(|n| n == ".gemini") {
        config_dir.to_path_buf()
    } else {
        config_dir.join(".gemini")
    }
}

/// The directory `GEMINI_CLI_HOME` should be set to so `gemini` uses
/// [`dot_gemini(config_dir)`].
pub fn cli_home(config_dir: &Path) -> PathBuf {
    if config_dir.file_name().is_some_and(|n| n == ".gemini") {
        config_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| config_dir.to_path_buf())
    } else {
        config_dir.to_path_buf()
    }
}

/// What one turn's `agy` / `gemini` process is started with.
#[derive(Debug, Clone)]
pub struct Launch {
    pub is_agy: bool,
    pub model: String,
    pub resume: Option<String>,
    pub prompt: String,
    pub mode: Option<String>,
    pub system_prompt_file: Option<PathBuf>,
    pub config_dir: PathBuf,
    pub has_bridge: bool,
}

/// The child's argv and environment for `launch`.
pub fn spec(launch: &Launch, parent_vars: impl IntoIterator<Item = String>) -> ChildSpec {
    let mut args: Vec<String> = if launch.is_agy {
        let model = normalize_agy_model(&launch.model);
        let mut a = vec![
            "--output-format".into(),
            "stream-json".into(),
            "--disable-slash-commands".into(),
            "--model".into(),
            model.to_string(),
        ];
        if let Some(id) = &launch.resume {
            a.push("--conversation".into());
            a.push(id.clone());
        }
        match launch.mode.as_deref() {
            Some("plan") => {
                a.push("--mode".into());
                a.push("plan".into());
            }
            Some("accept-edits") => {
                a.push("--mode".into());
                a.push("accept-edits".into());
            }
            _ => {
                a.push("--dangerously-skip-permissions".into());
            }
        }
        let p = if launch.prompt.is_empty() {
            " "
        } else {
            launch.prompt.as_str()
        };
        a.push(format!("--print={p}"));
        a
    } else {
        let mut a: Vec<String> = [
            "-p",
            "",
            "--output-format",
            "stream-json",
            "--model",
            launch.model.as_str(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(id) = &launch.resume {
            a.push("--resume".into());
            a.push(id.clone());
        }
        if launch.has_bridge {
            a.push("--allowed-mcp-server-names".into());
            a.push(BRIDGE_SERVER.into());
        }
        a
    };
    let _ = &mut args;

    let mut remove: Vec<String> = REMOVED.iter().map(|s| s.to_string()).collect();
    for name in parent_vars {
        if REMOVED_PREFIXES.iter().any(|p| name.starts_with(p)) && !remove.contains(&name) {
            remove.push(name);
        }
    }
    let home = cli_home(&launch.config_dir);
    let mut set = vec![
        ("GEMINI_CLI_HOME".to_string(), home.display().to_string()),
        ("GEMINI_CLI_NO_RELAUNCH".into(), "true".into()),
        ("NO_COLOR".into(), "1".into()),
    ];
    if let Some(f) = &launch.system_prompt_file {
        set.push(("GEMINI_SYSTEM_MD".into(), f.display().to_string()));
    }
    ChildSpec::from_parts(args, remove, set)
}

/// Any other `agy` / `gemini` run (`--version`, interactive login): the same
/// scrubbed environment with `args`.
pub fn command(
    args: Vec<String>,
    config_dir: &Path,
    parent_vars: impl IntoIterator<Item = String>,
) -> ChildSpec {
    let mut remove: Vec<String> = REMOVED.iter().map(|s| s.to_string()).collect();
    for name in parent_vars {
        if REMOVED_PREFIXES.iter().any(|p| name.starts_with(p)) && !remove.contains(&name) {
            remove.push(name);
        }
    }
    let home = cli_home(config_dir);
    let set = vec![
        ("GEMINI_CLI_HOME".to_string(), home.display().to_string()),
        ("GEMINI_CLI_NO_RELAUNCH".into(), "true".into()),
        ("NO_COLOR".into(), "1".into()),
    ];
    ChildSpec::from_parts(args, remove, set)
}

/// Updates `<dot_gemini>/settings.json` with ferrule's loopback MCP bridge
/// server entry while preserving existing auth settings (for legacy `gemini`).
pub fn write_bridge_settings(config_dir: &Path, mcp_server: Option<Value>) -> Result<()> {
    let dot = dot_gemini(config_dir);
    std::fs::create_dir_all(&dot)?;
    let path = dot.join("settings.json");
    let mut root: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    if !root.is_object() {
        root = json!({});
    }
    if root.pointer("/security/auth/selectedType").is_none()
        && root.get("selectedAuthType").is_none()
    {
        root["security"] = json!({
            "auth": {
                "selectedType": "oauth-personal"
            }
        });
    }
    if let Some(server) = mcp_server {
        let mut servers = serde_json::Map::new();
        servers.insert(BRIDGE_SERVER.into(), server);
        root["mcpServers"] = Value::Object(servers);
    }
    ferrule_connections::seal::write_private(&path, serde_json::to_string_pretty(&root)?.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_strips_api_keys_and_sets_agy_args() {
        let launch = Launch {
            is_agy: true,
            model: "gemini-2.5-pro".into(),
            resume: Some("sess-42".into()),
            prompt: "hello".into(),
            mode: None,
            system_prompt_file: Some("/tmp/sys.md".into()),
            config_dir: "/data/gemini-cli".into(),
            has_bridge: false,
        };
        let s = spec(
            &launch,
            [
                "PATH",
                "HOME",
                "GEMINI_API_KEY",
                "GOOGLE_API_KEY",
                "GEMINI_CUSTOM",
            ]
            .map(String::from),
        );
        assert!(s.remove.contains(&"GEMINI_API_KEY".to_string()));
        assert!(s.remove.contains(&"GOOGLE_API_KEY".to_string()));
        assert!(s.remove.contains(&"GEMINI_CUSTOM".to_string()));
        assert!(!s.remove.contains(&"PATH".to_string()));
        let set = s.set_redacted();
        let get = |k: &str| set.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("GEMINI_CLI_HOME").as_deref(), Some("/data/gemini-cli"));
        assert_eq!(get("GEMINI_SYSTEM_MD").as_deref(), Some("/tmp/sys.md"));
        assert!(s.args.contains(&"stream-json".to_string()));
        assert!(s.args.contains(&"gemini-4-argon-medium".to_string()));
        assert!(s.args.contains(&"--conversation".to_string()));
        assert!(s.args.contains(&"sess-42".to_string()));
        assert!(s.args.contains(&"--print=hello".to_string()));
    }
}
