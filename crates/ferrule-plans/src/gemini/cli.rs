//! `agy` (Antigravity CLI) / `gemini` run for something other than a turn:
//! finding the binary (including `~/.gemini/bin/agy`), its version, and
//! checking whether `~/.gemini/antigravity-cli/antigravity-oauth-token` or
//! `.gemini/oauth_creds.json` is present. Ferrule never reads or exfiltrates
//! the OAuth token values themselves.

use super::env;
pub use crate::claude::cli::{find_in, QUICK};
use anyhow::{anyhow, bail, Context, Result};
use ferrule_sandbox::Sandbox;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Install hint for messages that say the CLI is missing.
pub const INSTALL: &str = "install Antigravity CLI (`agy`) in ~/.gemini/bin/agy";

/// Finds `binary` on `PATH`, or in `~/.gemini/bin/agy` when `binary` is the
/// default `agy` / `gemini` name and `~/.gemini/bin` isn't on `PATH`.
pub fn find(binary: &Path) -> Option<PathBuf> {
    if let Some(found) = crate::claude::cli::find(binary) {
        return Some(found);
    }
    let name = binary.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if matches!(name, "agy" | "agy.exe" | "gemini" | "gemini.cmd") {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        if let Some(home) = home {
            let exe = if cfg!(windows) { "agy.exe" } else { "agy" };
            let candidate = home.join(".gemini").join("bin").join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if matches!(name, "agy" | "agy.exe") {
            if let Some(found) = crate::claude::cli::find(Path::new("gemini")) {
                return Some(found);
            }
        }
    }
    None
}

/// Resolves `binary` to its discovered full path (e.g. `~/.gemini/bin/agy`)
/// when found, or returns `binary` unchanged.
pub fn resolve(binary: &Path) -> PathBuf {
    find(binary).unwrap_or_else(|| binary.to_path_buf())
}

/// `agy` / `gemini` with the engine's environment: nothing that outranks the
/// Google sign-in, and `GEMINI_CLI_HOME` pointed at the config dir.
pub fn command(binary: &Path, config_dir: &Path, args: &[&str]) -> Command {
    let resolved = resolve(binary);
    let spec = env::command(
        args.iter().map(|a| a.to_string()).collect(),
        config_dir,
        std::env::vars_os().map(|(k, _)| k.to_string_lossy().into_owned()),
    );
    let mut cmd = Command::new(resolved);
    cmd.args(&spec.args);
    spec.apply_std(&mut cmd);
    cmd
}

/// [`command`], started through `sandbox` when one is given (owner mode).
pub fn command_in(
    binary: &Path,
    config_dir: &Path,
    args: &[&str],
    sandbox: Option<&Sandbox>,
) -> Result<Command> {
    let resolved = resolve(binary);
    let Some(sandbox) = sandbox else {
        return Ok(command(&resolved, config_dir, args));
    };
    let spec = env::command(
        args.iter().map(|a| a.to_string()).collect(),
        config_dir,
        std::env::vars_os().map(|(k, _)| k.to_string_lossy().into_owned()),
    );
    let mut cmd = sandbox
        .command(&resolved, &spec.args, config_dir)
        .map_err(|e| anyhow!("couldn't sandbox {}: {e}", resolved.display()))?;
    spec.apply_std(&mut cmd);
    Ok(cmd)
}

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

/// `agy --version` / `gemini --version`'s version word, run under `sandbox`
/// when one is given.
pub fn version(binary: &Path, config_dir: &Path, sandbox: Option<&Sandbox>) -> Result<String> {
    let cmd = command_in(binary, config_dir, &["--version"], sandbox)?;
    let out = run(cmd, QUICK).with_context(|| format!("running {} --version", binary.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .last()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{} --version printed nothing", binary.display()))
}

/// Sign-in state in `.gemini/`, inspected without reading token values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthStatus {
    pub logged_in: bool,
    pub auth_method: Option<String>,
    pub email: Option<String>,
}

impl AuthStatus {
    /// True when signed in with Google OAuth (`antigravity-oauth` or
    /// `oauth-personal`), not an API key or Vertex AI project.
    pub fn is_plan(&self) -> bool {
        self.logged_in
            && !matches!(
                self.auth_method.as_deref(),
                Some("gemini-api-key" | "api-key" | "vertex-ai" | "cloud-shell")
            )
    }
}

fn non_empty_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// Candidate directories under `config_dir` that may hold Antigravity or
/// Gemini CLI OAuth tokens.
pub(crate) fn candidate_dirs(config_dir: &Path) -> Vec<PathBuf> {
    let dot = env::dot_gemini(config_dir);
    vec![
        config_dir.to_path_buf(),
        config_dir.join("antigravity-cli"),
        dot.join("antigravity-cli"),
        dot,
    ]
}

/// Checks `antigravity-cli/antigravity-oauth-token`, `.gemini/oauth_creds.json`,
/// `.gemini/google_accounts.json`, and `settings.json` under `config_dir`
/// (without reading raw OAuth token strings).
pub fn auth_status(config_dir: &Path) -> AuthStatus {
    let dot = env::dot_gemini(config_dir);
    let agy_token = non_empty_file(&config_dir.join("antigravity-oauth-token"))
        || non_empty_file(
            &config_dir
                .join("antigravity-cli")
                .join("antigravity-oauth-token"),
        )
        || non_empty_file(&dot.join("antigravity-cli").join("antigravity-oauth-token"));

    let creds_path = if config_dir.join("oauth_creds.json").is_file() {
        config_dir.join("oauth_creds.json")
    } else {
        dot.join("oauth_creds.json")
    };
    let has_oauth = std::fs::read_to_string(&creds_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .is_some_and(|v| {
            v.get("refresh_token")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
                || v.get("access_token")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
        });

    let accounts_path = if config_dir.join("google_accounts.json").is_file() {
        config_dir.join("google_accounts.json")
    } else {
        dot.join("google_accounts.json")
    };
    let email = std::fs::read_to_string(accounts_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("active")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });

    let agy_provider = std::fs::read_to_string(dot.join("antigravity-cli").join("settings.json"))
        .or_else(|_| std::fs::read_to_string(config_dir.join("settings.json")))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("modelProvider")
                .and_then(Value::as_str)
                .map(str::to_string)
        });

    let selected_auth = std::fs::read_to_string(dot.join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.pointer("/security/auth/selectedType")
                .or_else(|| v.get("selectedAuthType"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });

    if agy_token && agy_provider.as_deref() != Some("gemini") {
        return AuthStatus {
            logged_in: true,
            auth_method: Some("antigravity-oauth".into()),
            email,
        };
    }

    let logged_in = has_oauth
        || matches!(
            selected_auth.as_deref(),
            Some("gemini-api-key" | "vertex-ai")
        );
    AuthStatus {
        logged_in,
        auth_method: selected_auth.or_else(|| has_oauth.then(|| "oauth-personal".into())),
        email,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_status_detects_oauth_and_email_without_exposing_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let st = auth_status(tmp.path());
        assert!(!st.logged_in);
        assert!(!st.is_plan());

        let dot = env::dot_gemini(tmp.path());
        std::fs::create_dir_all(&dot).unwrap();
        std::fs::write(
            dot.join("oauth_creds.json"),
            r#"{"refresh_token":"1//secret","access_token":"ya29.secret"}"#,
        )
        .unwrap();
        std::fs::write(
            dot.join("google_accounts.json"),
            r#"{"active":"user@example.com","old":[]}"#,
        )
        .unwrap();
        let st = auth_status(tmp.path());
        assert!(st.logged_in);
        assert!(st.is_plan());
        assert_eq!(st.email.as_deref(), Some("user@example.com"));
        assert_eq!(st.auth_method.as_deref(), Some("oauth-personal"));

        // If settings.json selects gemini-api-key, is_plan() is false.
        std::fs::write(
            dot.join("settings.json"),
            r#"{"security":{"auth":{"selectedType":"gemini-api-key"}}}"#,
        )
        .unwrap();
        let st = auth_status(tmp.path());
        assert!(st.logged_in);
        assert!(!st.is_plan());

        // Antigravity CLI token in antigravity-cli/antigravity-oauth-token is detected as plan.
        let agy_dir = dot.join("antigravity-cli");
        std::fs::create_dir_all(&agy_dir).unwrap();
        std::fs::write(agy_dir.join("antigravity-oauth-token"), "opaque-token").unwrap();
        let st = auth_status(tmp.path());
        assert!(st.logged_in);
        assert!(st.is_plan());
        assert_eq!(st.auth_method.as_deref(), Some("antigravity-oauth"));
    }
}
