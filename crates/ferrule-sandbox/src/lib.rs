//! OS-level sandbox for the commands the agent runs.
//!
//! The shell tool's deny-list only catches the obvious spellings of danger;
//! any command can be rephrased around it. This crate hands enforcement to
//! the kernel instead:
//!
//! - **Linux**: Landlock confines writes to the workspace and a few declared
//!   roots — reads stay open, the agent needs its toolchain — and, when
//!   network is off, a seccomp filter refuses every non-Unix socket. Both are
//!   unprivileged and inherited by every descendant, so `sh -c`, a spawned
//!   interpreter or a backgrounded job all stay inside.
//! - **macOS**: Seatbelt through `/usr/bin/sandbox-exec`, with a profile
//!   adapted from OpenAI Codex's.
//!
//! On every platform, secret-looking environment variables (API keys, bot
//! tokens) are dropped from the child's environment, so a prompt-injected
//! `env | curl …` has nothing to send.
//!
//! MCP servers go through the same path, with their own state dir writable
//! ([`Sandbox::for_helper`]).
//!
//! Not covered: reads, and anything that needs a kernel bug.

#[cfg(target_os = "linux")]
mod linux;
pub mod seatbelt;
mod shell;

pub use shell::{Shell, ShellKind};

use serde::Deserialize;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a refused write prints: Seatbelt denies with EPERM, Landlock with
/// EACCES.
pub const DENIED: &str = if cfg!(target_os = "macos") {
    "Operation not permitted"
} else {
    "Permission denied"
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// No OS sandbox. Env scrubbing and the shell deny-list still apply.
    Off,
    /// Nothing is writable except `/dev/null`.
    ReadOnly,
    /// The workspace, temp dirs and `writable_roots` are writable; the rest
    /// of the filesystem is read-only.
    #[default]
    WorkspaceWrite,
}

/// The `[sandbox]` config section. Unknown keys are an error: a misspelt
/// `netwrok = false` must not quietly leave the network on.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub mode: Mode,
    /// Refuse to start when the sandbox can't be applied, instead of warning
    /// and running commands unsandboxed.
    pub require: bool,
    /// Allow network access. `false` blocks every socket except Unix ones.
    pub network: bool,
    /// Extra writable directories. `~/` is the home dir; relative paths are
    /// relative to the workspace. Missing ones are skipped.
    pub writable_roots: Vec<PathBuf>,
    /// Make `/tmp`, `$TMPDIR` and `/dev/shm` writable.
    pub tmp: bool,
    /// Drop env vars whose name contains KEY, SECRET, TOKEN, PASSWORD,
    /// PASSWD or CREDENTIAL, plus `secret_vars`.
    pub scrub_secret_env: bool,
    /// Env vars kept even though their name looks secret.
    pub env_passthrough: Vec<String>,
    /// Exact env var names to drop — filled in by the host from the config
    /// (provider `api_key_env`, the Telegram token var), not read from it.
    #[serde(skip)]
    pub secret_vars: Vec<String>,
    /// Paths sandboxed commands can neither read nor write, even inside a
    /// writable root — the host's secrets file and the proxy's CA key.
    /// Filled in by the host, not read from the config. Missing ones are
    /// skipped.
    #[serde(skip)]
    pub hidden: Vec<PathBuf>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: Mode::WorkspaceWrite,
            require: false,
            network: true,
            writable_roots: Vec::new(),
            tmp: true,
            scrub_secret_env: true,
            env_passthrough: Vec::new(),
            secret_vars: Vec::new(),
            hidden: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Landlock {
        abi: u32,
    },
    Seatbelt,
    /// Commands run unsandboxed; `Sandbox::degraded` says why.
    None,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Backend::Landlock { abi } => write!(f, "landlock (ABI {abi})"),
            Backend::Seatbelt => write!(f, "seatbelt"),
            Backend::None => write!(f, "none"),
        }
    }
}

const SECRET_MARKERS: &[&str] = &["KEY", "SECRET", "TOKEN", "PASSWORD", "PASSWD", "CREDENTIAL"];

#[derive(Debug, Clone)]
pub struct Sandbox {
    policy: Policy,
    backend: Backend,
    degraded: Option<String>,
    /// Set on every command after scrubbing (the credential proxy's
    /// placeholders and proxy settings).
    extra_env: Vec<(String, String)>,
    /// Writable on top of what the policy grants, even in read-only mode:
    /// a helper's own state dir (see [`Sandbox::for_helper`]). Not
    /// canonical yet; `writable_roots` resolves them.
    helper_roots: Vec<PathBuf>,
    /// Where ferrule's own HTTP clients send HTTPS (the credential proxy),
    /// when one runs.
    egress: Option<Egress>,
}

/// The credential proxy as seen by an HTTP client inside ferrule
/// (`web_fetch`, MCP over HTTP): its URL, credentials included, and the CA
/// it signs its certificates with. Plain HTTP never goes through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Egress {
    pub proxy_url: String,
    pub ca_cert_pem: String,
}

impl Sandbox {
    /// No OS sandbox, default env scrubbing — what a bare `ShellTool` uses.
    pub fn off() -> Self {
        Self {
            policy: Policy {
                mode: Mode::Off,
                ..Policy::default()
            },
            backend: Backend::None,
            degraded: Some("disabled (sandbox.mode = \"off\")".into()),
            extra_env: Vec::new(),
            helper_roots: Vec::new(),
            egress: None,
        }
    }

    /// Detect a backend and prove it works by running `sh -c 'exit 0'` under
    /// it. When it doesn't, `policy.require` decides between an error and a
    /// logged warning plus unsandboxed execution.
    pub fn new(policy: Policy) -> Result<Self, String> {
        if policy.mode == Mode::Off {
            return Ok(Self {
                policy,
                ..Self::off()
            });
        }
        let attempt = detect(&policy).and_then(|backend| {
            let sandbox = Self {
                policy: policy.clone(),
                backend,
                degraded: None,
                ..Self::off()
            };
            sandbox.probe()?;
            Ok(sandbox)
        });
        match attempt {
            Ok(sandbox) => Ok(sandbox),
            Err(reason) if policy.require => Err(format!(
                "sandbox.require is set but the sandbox can't be applied: {reason}"
            )),
            Err(reason) => {
                tracing::warn!("shell commands will run UNSANDBOXED: {reason}");
                Ok(Self {
                    policy,
                    backend: Backend::None,
                    degraded: Some(reason),
                    ..Self::off()
                })
            }
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Why commands run unsandboxed, if they do.
    pub fn degraded(&self) -> Option<&str> {
        self.degraded.as_deref()
    }

    pub fn is_active(&self) -> bool {
        self.backend != Backend::None
    }

    /// Variables to set on every command, after secrets are scrubbed — so a
    /// scrubbed name can come back holding a placeholder.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.extra_env = env;
        self
    }

    /// The credential proxy for ferrule's own HTTP clients. Set together
    /// with `with_env(broker.child_env())`, so commands and in-process
    /// clients take the same way out.
    pub fn with_egress(mut self, egress: Option<Egress>) -> Self {
        self.egress = egress;
        self
    }

    pub fn egress(&self) -> Option<&Egress> {
        self.egress.as_ref()
    }

    /// For a long-lived helper process that ferrule starts from its own
    /// config (an MCP server), rather than a command the model wrote: the
    /// same scrubbing and credential env, `state_dir` and `extra` writable
    /// on top of what commands get (even in read-only mode, which is about
    /// the model's commands), and the network always on, since reaching a
    /// service is what most helpers are for. Relative `extra` paths are
    /// relative to the workspace, like `writable_roots`.
    pub fn for_helper(&self, state_dir: &Path, extra: &[PathBuf]) -> Self {
        let mut helper = self.clone();
        helper.policy.network = true;
        helper.helper_roots.push(state_dir.to_path_buf());
        helper.helper_roots.extend(extra.iter().cloned());
        helper
    }

    /// The same secret scrubbing and credential env as `self`, with the OS
    /// confinement off: the escape hatch for a helper its config opted out
    /// of the sandbox. `reason` is what `degraded()` reports.
    pub fn unconfined(&self, reason: impl Into<String>) -> Self {
        Self {
            policy: Policy {
                mode: Mode::Off,
                ..self.policy.clone()
            },
            backend: Backend::None,
            degraded: Some(reason.into()),
            extra_env: self.extra_env.clone(),
            helper_roots: Vec::new(),
            egress: self.egress.clone(),
        }
    }

    /// What `name` holds in a sandboxed command's environment: a
    /// credential placeholder, the parent's value, or nothing when it's
    /// scrubbed. For expanding `${NAME}` in config that ferrule sends on a
    /// helper's behalf (MCP HTTP headers), so it gets exactly what a
    /// spawned helper would.
    pub fn child_env_var(&self, name: &str) -> Option<String> {
        if let Some((_, v)) = self.extra_env.iter().rev().find(|(k, _)| k == name) {
            return Some(v.clone());
        }
        if self.policy.scrub_secret_env && self.is_secret_var(name) {
            return None;
        }
        std::env::var(name).ok()
    }

    /// A `Command` for `program args…` that runs in `workspace` under the
    /// sandbox with a scrubbed environment. Callers add stdio and spawn.
    pub fn command<I, S>(
        &self,
        program: impl AsRef<OsStr>,
        args: I,
        workspace: &Path,
    ) -> io::Result<Command>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let roots = self.writable_roots(workspace);
        let hidden = self.hidden_paths();
        let mut cmd = match self.backend {
            Backend::Seatbelt => {
                seatbelt::command(self.policy.network, &roots, &hidden, program, args)
            }
            _ => {
                let mut c = Command::new(program);
                c.args(args);
                c
            }
        };
        cmd.current_dir(workspace);
        for name in self.scrubbed_vars() {
            cmd.env_remove(name);
        }
        cmd.envs(self.extra_env.iter().map(|(k, v)| (k, v)));
        #[cfg(target_os = "linux")]
        if let Backend::Landlock { abi } = self.backend {
            linux::apply(&mut cmd, abi, self.policy.network, &roots, &hidden)?;
        }
        Ok(cmd)
    }

    /// Canonical directories (or files) the sandboxed command may write,
    /// missing ones skipped. Empty in read-only mode.
    pub fn writable_roots(&self, workspace: &Path) -> Vec<PathBuf> {
        let read_only = self.policy.mode == Mode::ReadOnly;
        if read_only && self.helper_roots.is_empty() {
            return Vec::new();
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut wanted = Vec::new();
        let configured: &[PathBuf] = if read_only {
            &[]
        } else {
            wanted.push(workspace.to_path_buf());
            &self.policy.writable_roots
        };
        for root in configured.iter().chain(&self.helper_roots) {
            let expanded = match (root.strip_prefix("~"), &home) {
                (Ok(rest), Some(home)) => home.join(rest),
                _ if root.is_relative() => workspace.join(root),
                _ => root.clone(),
            };
            wanted.push(expanded);
        }
        if self.policy.tmp && !read_only {
            wanted.push(PathBuf::from("/tmp"));
            wanted.extend(std::env::var_os("TMPDIR").map(PathBuf::from));
            if cfg!(target_os = "linux") {
                wanted.push(PathBuf::from("/dev/shm"));
            }
        }
        let mut roots: Vec<PathBuf> = Vec::new();
        for path in wanted {
            match path.canonicalize() {
                Ok(p) if !roots.contains(&p) => roots.push(p),
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!("sandbox: skipping writable root {}: {e}", path.display())
                }
            }
        }
        roots
    }

    /// `policy.hidden`, canonical, missing ones skipped.
    pub fn hidden_paths(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for path in &self.policy.hidden {
            if let Ok(p) = path.canonicalize() {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        out
    }

    /// Names of the variables in this process's environment that sandboxed
    /// commands won't see. Names only — values are never exposed.
    pub fn scrubbed_vars(&self) -> Vec<String> {
        if !self.policy.scrub_secret_env {
            return Vec::new();
        }
        let mut names: Vec<String> = std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .filter(|k| self.is_secret_var(k))
            .collect();
        names.sort();
        names
    }

    fn is_secret_var(&self, name: &str) -> bool {
        if self.policy.env_passthrough.iter().any(|p| p == name) {
            return false;
        }
        let upper = name.to_ascii_uppercase();
        self.policy.secret_vars.iter().any(|s| s == name)
            || SECRET_MARKERS.iter().any(|m| upper.contains(m))
    }

    /// One line for the shell tool's description, so the model knows why a
    /// write is refused instead of retrying it forever.
    pub fn model_note(&self) -> Option<String> {
        if !self.is_active() {
            return None;
        }
        let fs = match self.policy.mode {
            Mode::ReadOnly => "The filesystem is read-only.".to_string(),
            _ => {
                let mut places = vec!["the workspace"];
                if self.policy.tmp {
                    places.push("temp dirs");
                }
                if !self.policy.writable_roots.is_empty() {
                    places.push("a few configured dirs");
                }
                let last = places.pop().unwrap_or_default();
                let places = if places.is_empty() {
                    last.to_string()
                } else {
                    format!("{} and {last}", places.join(", "))
                };
                format!(
                    "Writes are only allowed in {places}; elsewhere they fail with \"{DENIED}\"."
                )
            }
        };
        let net = if self.policy.network {
            "Network access is allowed."
        } else {
            "Network access is blocked."
        };
        Some(format!("Commands run in a sandbox. {fs} {net}"))
    }

    fn probe(&self) -> Result<(), String> {
        let dir = std::env::temp_dir();
        let status = self
            .command("/bin/sh", ["-c", "exit 0"], &dir)
            .and_then(|mut c| c.stdin(std::process::Stdio::null()).status())
            .map_err(|e| format!("{} failed to start a probe command: {e}", self.backend))?;
        if !status.success() {
            return Err(format!(
                "{} probe command exited with {status}",
                self.backend
            ));
        }
        Ok(())
    }
}

fn detect(policy: &Policy) -> Result<Backend, String> {
    #[cfg(target_os = "linux")]
    {
        let abi = linux::abi()?;
        if !policy.network && !linux::SECCOMP_SUPPORTED {
            return Err("network = false needs the seccomp filter, which is only built for x86_64 and aarch64".into());
        }
        Ok(Backend::Landlock { abi })
    }
    #[cfg(target_os = "macos")]
    {
        let _ = policy;
        if Path::new(seatbelt::SANDBOX_EXEC).exists() {
            Ok(Backend::Seatbelt)
        } else {
            Err(format!("{} not found", seatbelt::SANDBOX_EXEC))
        }
    }
    #[cfg(windows)]
    {
        let _ = policy;
        Err("Windows has no sandbox backend yet (under WSL2, the Linux build has one)".into())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = policy;
        Err("no sandbox backend for this OS".into())
    }
}

/// SIGKILL a whole process group — for a timed-out command started with
/// `process_group(0)`, whose children would otherwise outlive it.
#[cfg(unix)]
pub fn kill_process_group(pgid: u32) {
    // SAFETY: plain syscall; a stale pgid just returns ESRCH.
    unsafe {
        libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub fn kill_process_group(_pgid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: Mode) -> Policy {
        Policy {
            mode,
            tmp: false,
            ..Policy::default()
        }
    }

    #[test]
    fn policy_parses_from_toml_shaped_config() {
        let p: Policy = serde_json::from_str(
            r#"{"mode": "read-only", "network": false, "writable_roots": ["~/.cargo"]}"#,
        )
        .unwrap();
        assert_eq!(p.mode, Mode::ReadOnly);
        assert!(!p.network);
        assert!(
            p.tmp && p.scrub_secret_env,
            "unset fields keep their defaults"
        );
        let d: Policy = serde_json::from_str("{}").unwrap();
        assert_eq!(d.mode, Mode::WorkspaceWrite);
        assert!(d.network);
        assert!(serde_json::from_str::<Policy>(r#"{"netwrok": false}"#).is_err());
        assert!(
            serde_json::from_str::<Policy>(r#"{"secret_vars": []}"#).is_err(),
            "host-filled, not config"
        );
    }

    #[test]
    fn secret_env_names_are_detected_and_passthrough_wins() {
        let sb = Sandbox {
            policy: Policy {
                secret_vars: vec!["MY_PROVIDER".into()],
                env_passthrough: vec!["GIT_ASKPASS_TOKEN".into()],
                ..Policy::default()
            },
            backend: Backend::None,
            degraded: None,
            extra_env: Vec::new(),
            ..Sandbox::off()
        };
        for secret in [
            "OPENAI_API_KEY",
            "TELEGRAM_BOT_TOKEN",
            "aws_secret_access_key",
            "DB_PASSWORD",
            "MY_PROVIDER",
        ] {
            assert!(sb.is_secret_var(secret), "{secret}");
        }
        for plain in ["PATH", "HOME", "SSH_AUTH_SOCK", "GIT_ASKPASS_TOKEN", "LANG"] {
            assert!(!sb.is_secret_var(plain), "{plain}");
        }
    }

    #[test]
    fn extra_env_is_set_after_scrubbing() {
        // HOME is always set; listing it as a secret makes it scrubbed.
        let sb = Sandbox {
            policy: Policy {
                secret_vars: vec!["HOME".into()],
                ..Policy::default()
            },
            backend: Backend::None,
            degraded: None,
            extra_env: Vec::new(),
            ..Sandbox::off()
        }
        .with_env(vec![("HOME".into(), "placeholder".into())]);
        let ws = tempfile::tempdir().unwrap();
        let cmd = sb.command("true", [""; 0], ws.path()).unwrap();
        let home: Vec<_> = cmd.get_envs().filter(|(k, _)| *k == "HOME").collect();
        assert_eq!(
            home,
            [(OsStr::new("HOME"), Some(OsStr::new("placeholder")))]
        );
    }

    #[test]
    fn writable_roots_resolve_and_skip_missing() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir(ws.path().join("out")).unwrap();
        let sb = Sandbox {
            policy: Policy {
                writable_roots: vec!["out".into(), "/definitely/not/here".into()],
                ..policy(Mode::WorkspaceWrite)
            },
            backend: Backend::None,
            degraded: None,
            extra_env: Vec::new(),
            ..Sandbox::off()
        };
        let ws_c = ws.path().canonicalize().unwrap();
        assert_eq!(
            sb.writable_roots(ws.path()),
            vec![ws_c.clone(), ws_c.join("out")]
        );

        let ro = Sandbox {
            policy: policy(Mode::ReadOnly),
            ..sb
        };
        assert!(ro.writable_roots(ws.path()).is_empty());
    }

    #[test]
    fn a_helper_gets_its_state_dir_even_read_only_and_always_the_network() {
        let ws = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(ws.path().join("extra")).unwrap();
        let base = Sandbox {
            policy: Policy {
                network: false,
                ..policy(Mode::WorkspaceWrite)
            },
            ..Sandbox::off()
        };
        let helper = base.for_helper(state.path(), &["extra".into()]);
        let c = |p: &Path| p.canonicalize().unwrap();
        assert_eq!(
            helper.writable_roots(ws.path()),
            vec![c(ws.path()), c(state.path()), c(&ws.path().join("extra"))]
        );
        assert!(helper.policy.network && !base.policy.network);
        assert_eq!(
            base.writable_roots(ws.path()),
            vec![c(ws.path())],
            "original untouched"
        );

        let ro = Sandbox {
            policy: Policy {
                writable_roots: vec!["extra".into()],
                ..policy(Mode::ReadOnly)
            },
            ..Sandbox::off()
        };
        assert_eq!(
            ro.for_helper(state.path(), &[]).writable_roots(ws.path()),
            vec![c(state.path())],
            "read-only keeps the workspace and configured roots closed, not the helper's own dir"
        );
    }

    #[test]
    fn child_env_var_sees_placeholders_and_not_secrets() {
        std::env::set_var("FERRULE_SB_TEST_TOKEN", "real");
        std::env::set_var("FERRULE_SB_TEST_PLAIN", "plain");
        let sb = Sandbox::off().with_env(vec![("GH_TOKEN".into(), "ghp_placeholder".into())]);
        assert_eq!(
            sb.child_env_var("GH_TOKEN").as_deref(),
            Some("ghp_placeholder")
        );
        assert_eq!(sb.child_env_var("FERRULE_SB_TEST_TOKEN"), None);
        assert_eq!(
            sb.child_env_var("FERRULE_SB_TEST_PLAIN").as_deref(),
            Some("plain")
        );
    }

    #[test]
    fn unconfined_keeps_scrubbing_and_env_but_drops_the_backend() {
        let sb = Sandbox {
            policy: Policy {
                secret_vars: vec!["MY_PROVIDER".into()],
                ..policy(Mode::WorkspaceWrite)
            },
            backend: Backend::Seatbelt,
            degraded: None,
            extra_env: vec![("HTTPS_PROXY".into(), "http://127.0.0.1:1".into())],
            ..Sandbox::off()
        };
        let un = sb.unconfined("mcp.servers.foo: sandbox = false");
        assert_eq!(un.backend(), Backend::None);
        assert!(!un.is_active());
        assert_eq!(un.degraded(), Some("mcp.servers.foo: sandbox = false"));
        assert!(
            un.is_secret_var("MY_PROVIDER"),
            "kept the caller's secret_vars"
        );
        assert_eq!(un.extra_env, sb.extra_env, "credential env is preserved");
    }

    #[test]
    fn off_mode_never_probes_and_reports_why() {
        let sb = Sandbox::new(policy(Mode::Off)).unwrap();
        assert_eq!(sb.backend(), Backend::None);
        assert!(sb.degraded().unwrap().contains("off"));
        assert!(sb.model_note().is_none());
    }

    #[test]
    fn model_note_says_where_writes_go() {
        let note = |policy| {
            Sandbox {
                policy,
                backend: Backend::Seatbelt,
                degraded: None,
                extra_env: Vec::new(),
                ..Sandbox::off()
            }
            .model_note()
            .unwrap()
        };
        assert!(note(policy(Mode::WorkspaceWrite)).contains("allowed in the workspace;"));
        let all = note(Policy {
            tmp: true,
            writable_roots: vec!["~/.cargo".into()],
            network: false,
            ..policy(Mode::WorkspaceWrite)
        });
        assert!(
            all.contains("the workspace, temp dirs and a few configured dirs;"),
            "{all}"
        );
        assert!(all.contains("Network access is blocked."));
        assert!(note(policy(Mode::ReadOnly)).contains("read-only"));
    }
}
