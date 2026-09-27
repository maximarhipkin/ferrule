//! `claude` run for something other than a turn: finding the binary, its
//! version, and `claude auth status|login|logout` against the engine's
//! config dir. Ferrule never reads claude's credentials; it asks claude.

use super::env;
use super::token::Token;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long `--version` or `auth status` may take.
pub const QUICK: Duration = Duration::from_secs(20);

/// `binary` as a path: itself when it has a directory in it, else the
/// first match on `PATH` (with `PATHEXT` on Windows).
pub fn find(binary: &Path) -> Option<PathBuf> {
    if binary.components().count() > 1 || binary.is_absolute() {
        return binary.is_file().then(|| binary.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|e| e.to_ascii_lowercase())
            .chain([String::new()])
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let mut name = binary.as_os_str().to_owned();
            name.push(ext);
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// What `npm` installs it with, for the messages that say it's missing.
pub const INSTALL: &str = "npm install -g @anthropic-ai/claude-code";

/// `claude` with the engine's environment: nothing that outranks the
/// plan, `CLAUDE_CONFIG_DIR` set, and `token` only when one is given.
pub fn command(binary: &Path, config_dir: &Path, token: Option<&Token>, args: &[&str]) -> Command {
    let spec = env::command(
        args.iter().map(|a| a.to_string()).collect(),
        config_dir,
        token,
        std::env::vars_os().map(|(k, _)| k.to_string_lossy().into_owned()),
    );
    let mut cmd = Command::new(binary);
    cmd.args(&spec.args);
    spec.apply_std(&mut cmd);
    cmd
}

/// Runs `cmd` to the end with no input, killing it after `limit`.
fn run(mut cmd: Command, limit: Duration) -> Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("took more than {} s", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// `claude --version`'s first word ("2.1.283").
pub fn version(binary: &Path, config_dir: &Path) -> Result<String> {
    let out = run(command(binary, config_dir, None, &["--version"]), QUICK)
        .with_context(|| format!("running {} --version", binary.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{} --version printed nothing", binary.display()))
}

/// `claude auth status --json`, the parts ferrule shows.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    #[serde(default)]
    pub logged_in: bool,
    #[serde(default)]
    pub auth_method: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub subscription_type: Option<String>,
}

impl AuthStatus {
    /// An API key or a cloud provider isn't the plan.
    pub fn is_plan(&self) -> bool {
        self.logged_in
            && !matches!(
                self.auth_method.as_deref(),
                Some("api_key" | "apiKey" | "console" | "bedrock" | "vertex" | "foundry")
            )
    }
}

/// Asks claude whether it's signed in under `config_dir` (with `token`
/// when ferrule holds one). No model call is made.
pub fn auth_status(binary: &Path, config_dir: &Path, token: Option<&Token>) -> Result<AuthStatus> {
    let out = run(
        command(binary, config_dir, token, &["auth", "status", "--json"]),
        QUICK,
    )
    .with_context(|| format!("running {} auth status", binary.display()))?;
    // Not signed in exits 1 with the same JSON.
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.trim()).with_context(|| {
        format!(
            "{} auth status didn't answer in JSON (Claude Code too old?): {}",
            binary.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    })
}
