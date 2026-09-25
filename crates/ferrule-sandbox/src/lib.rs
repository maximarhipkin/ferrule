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
//! - **Windows**: a restricted token in a job object, started through a
//!   launcher ([`launch`]): writes only where a capability SID is granted,
//!   ferrule's own secrets and process shut, the tree killed with the job.
//!   The network is not enforced there.
//!
//! On every platform, secret-looking environment variables (API keys, bot
//! tokens) are dropped from the child's environment, so a prompt-injected
//! `env | curl …` has nothing to send.
//!
//! MCP servers go through the same path, with their own state dir writable
//! ([`Sandbox::for_helper`]).
//!
//! Reads are open except for [`Sandbox::read_denies`]: ferrule's own
//! secrets, the usual credential dirs and browser profiles, and what the
//! owner adds. The in-process file tools refuse the same list.
//!
//! Not covered: anything that needs a kernel bug.

pub mod launch;
#[cfg(target_os = "linux")]
mod linux;
pub mod seatbelt;
mod shell;
#[cfg(windows)]
mod windows;

pub use shell::{Shell, ShellKind};
/// For tests: whether this process can open `pid` to read its memory.
#[cfg(windows)]
#[doc(hidden)]
pub use windows::can_read_process;

use serde::Deserialize;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a refused write prints: Seatbelt denies with EPERM, Landlock with
/// EACCES, and Git Bash maps Windows' access-denied to EACCES too.
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
    /// Extra paths sandboxed commands and the file tools can't read (or
    /// write). `~/` is the home dir; relative paths are relative to the
    /// workspace.
    pub deny_read: Vec<PathBuf>,
    /// Re-open entries of the default deny list ([`default_read_denies`])
    /// equal to or under one of these. Never `hidden` or `deny_read`.
    pub allow_read: Vec<PathBuf>,
    /// Deny the usual credential dirs and browser profiles
    /// ([`default_read_denies`]).
    pub deny_default_reads: bool,
    /// Windows: most processes a command tree may run at once (the job's
    /// limit). 0 means no limit.
    pub process_limit: u32,
    /// Windows: the job's memory limit for the whole command tree, in MB.
    pub memory_mb: Option<u64>,
    /// Windows: where the launcher keeps the writable roots' capability
    /// SIDs (`<data>/sandbox`). Filled in by the host; without it, under
    /// `%LOCALAPPDATA%`.
    #[serde(skip)]
    pub state_dir: Option<PathBuf>,
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
            deny_read: Vec::new(),
            allow_read: Vec::new(),
            deny_default_reads: true,
            process_limit: 256,
            memory_mb: None,
            state_dir: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Landlock {
        abi: u32,
    },
    Seatbelt,
    /// A restricted token in a job, through the launcher.
    Windows,
    /// Commands run unsandboxed; `Sandbox::degraded` says why.
    None,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Backend::Landlock { abi } => write!(f, "landlock (ABI {abi})"),
            Backend::Seatbelt => write!(f, "seatbelt"),
            Backend::Windows => write!(f, "windows (restricted token)"),
            Backend::None => write!(f, "none"),
        }
    }
}

const SECRET_MARKERS: &[&str] = &["KEY", "SECRET", "TOKEN", "PASSWORD", "PASSWD", "CREDENTIAL"];

/// Whether a variable's name marks it as a credential, the way every
/// sandbox scrubs it by default (before a policy's own lists).
pub fn looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_MARKERS.iter().any(|m| upper.contains(m))
}

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
    /// macOS only: let the helper reach the system's Mach/XPC services, which
    /// a desktop app needs (see [`Sandbox::with_desktop_services`]).
    desktop: bool,
    /// Writes and the network as open as the user's, only the read denies
    /// enforced (see [`Sandbox::unconfined`]).
    hide_only: bool,
}

/// The credential proxy as seen by an HTTP client inside ferrule
/// (`web_fetch`, MCP over HTTP): its URL, credentials included, and the CA
/// it signs its certificates with. Plain HTTP goes through it too.
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
            desktop: false,
            hide_only: false,
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
            Ok(sandbox) => {
                // Ferrule's own memory holds the real secrets: shut its
                // process to the sandbox token before anything runs in it.
                #[cfg(windows)]
                if let Err(e) = windows::harden_self() {
                    tracing::warn!("sandbox: couldn't protect ferrule's own process: {e}");
                }
                Ok(sandbox)
            }
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

    /// Whether commands are confined: false for no backend, and for a
    /// hide-only sandbox, which only keeps reads out of the denied paths.
    pub fn is_active(&self) -> bool {
        self.backend != Backend::None && !self.hide_only
    }

    /// Whether the read denies are enforced by the OS: a confining sandbox,
    /// or a hide-only one.
    pub fn hides_reads(&self) -> bool {
        self.backend != Backend::None
    }

    /// See [`Sandbox::unconfined`].
    pub fn is_hide_only(&self) -> bool {
        self.hide_only
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

    /// For a sub-agent: `extra` writable on top of what its commands get
    /// (a worktree child's git dir, so it can commit), and read-only when
    /// its role only reads. With the sandbox off there is nothing to
    /// narrow; the caller takes its file-writing tools away instead.
    pub fn for_child(&self, extra: &[PathBuf], read_only: bool) -> Self {
        let mut child = self.clone();
        if read_only {
            if child.policy.mode == Mode::WorkspaceWrite {
                child.policy.mode = Mode::ReadOnly;
            }
        } else {
            child.policy.writable_roots.extend(extra.iter().cloned());
        }
        child
    }

    /// For an M19 plan-mode run, which may look but not touch: read-only
    /// (unless there's no OS sandbox at all, which the caller checks with
    /// `is_active`), no extra writable roots and no network.
    pub fn for_planning(&self) -> Self {
        let mut p = self.for_child(&[], true);
        p.policy.writable_roots.clear();
        p.helper_roots.clear();
        p.policy.network = false;
        p
    }

    /// For a helper that is a desktop app (the browser): on macOS the
    /// Seatbelt profile also allows Mach/XPC lookups, IOKit, shared memory
    /// and the like, without which Chrome aborts at startup. Writes and the
    /// hidden paths stay confined. No effect on the other backends.
    pub fn with_desktop_services(mut self) -> Self {
        self.desktop = true;
        self
    }

    /// The same secret scrubbing and credential env as `self`, with the
    /// confinement off: the escape hatch for a helper its config opted out
    /// of the sandbox. `reason` is what `degraded()` reports.
    ///
    /// Where there is a backend it stays on, hide-only: writes and the
    /// network as open as the user's, but the read denies still hold, so
    /// the helper can't read ferrule's secrets, the credential dirs, or
    /// (Landlock, Windows) ferrule's own process. Windows keeps the job
    /// too. [`Sandbox::is_active`] is false either way.
    pub fn unconfined(&self, reason: impl Into<String>) -> Self {
        Self {
            policy: Policy {
                network: true,
                ..self.policy.clone()
            },
            backend: self.backend,
            degraded: Some(reason.into()),
            extra_env: self.extra_env.clone(),
            helper_roots: Vec::new(),
            egress: self.egress.clone(),
            desktop: false,
            hide_only: self.backend != Backend::None,
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
        // Hide-only: all of `/` writable, carved around the denies.
        let roots = match self.hide_only {
            true if cfg!(target_os = "linux") => vec![PathBuf::from("/")],
            true => Vec::new(),
            false => self.writable_roots(workspace),
        };
        let hidden = self.read_denies(workspace);
        let mut cmd = match self.backend {
            Backend::Seatbelt => {
                let profile = if self.hide_only {
                    seatbelt::open_profile(&hidden)
                } else {
                    seatbelt::profile(self.policy.network, self.desktop, &roots, &hidden)
                };
                seatbelt::command(profile, program, args)
            }
            Backend::Windows => {
                let spec = launch::Spec {
                    write_roots: roots.clone(),
                    protect: self.owned_denies(workspace),
                    confine_writes: !self.hide_only,
                    process_limit: self.policy.process_limit,
                    memory_mb: self.policy.memory_mb,
                    state_file: self
                        .policy
                        .state_dir
                        .as_ref()
                        .map(|d| d.join("windows-caps.json")),
                };
                let launcher = launch::launcher().map_err(io::Error::other)?;
                let mut c = Command::new(launcher);
                c.arg(launch::LAUNCH_ARG).arg(program).args(args);
                c.env(launch::SPEC_VAR, spec.to_json());
                c
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
        let home = home_dir();
        let mut wanted = Vec::new();
        let configured: &[PathBuf] = if read_only {
            &[]
        } else {
            wanted.push(workspace.to_path_buf());
            &self.policy.writable_roots
        };
        for root in configured.iter().chain(&self.helper_roots) {
            wanted.push(expand(root, home.as_deref(), workspace));
        }
        if self.policy.tmp && !read_only {
            if cfg!(windows) {
                wanted.extend(
                    ["TEMP", "TMP"]
                        .into_iter()
                        .filter_map(std::env::var_os)
                        .filter(|v| !v.is_empty())
                        .map(PathBuf::from),
                );
            } else {
                wanted.push(PathBuf::from("/tmp"));
                wanted.extend(std::env::var_os("TMPDIR").map(PathBuf::from));
            }
            if cfg!(target_os = "linux") {
                wanted.push(PathBuf::from("/dev/shm"));
            }
        }
        let mut roots: Vec<PathBuf> = Vec::new();
        for path in wanted {
            match dunce::canonicalize(&path) {
                Ok(p) if !roots.contains(&p) => roots.push(p),
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!("sandbox: skipping writable root {}: {e}", path.display())
                }
            }
        }
        roots
    }

    /// `policy.hidden`, canonical, missing ones skipped: ferrule's own
    /// secrets, the part of [`Sandbox::read_denies`] no config can open.
    pub fn hidden_paths(&self) -> Vec<PathBuf> {
        canonical_existing(&self.policy.hidden)
    }

    /// Everything commands and the file tools may not read, as configured:
    /// `hidden`, the default credential dirs unless `allow_read` re-opens
    /// them, and `deny_read`. Expanded (`~/`, relative to `workspace`) but
    /// not resolved, so a path created later still matches — what the
    /// in-process file tools want.
    pub fn read_deny_list(&self, workspace: &Path) -> Vec<PathBuf> {
        let home = home_dir();
        let expand = |p: &PathBuf| expand(p, home.as_deref(), workspace);
        let allowed: Vec<PathBuf> = self.policy.allow_read.iter().map(expand).collect();
        let mut out: Vec<PathBuf> = self.policy.hidden.clone();
        if self.policy.deny_default_reads {
            out.extend(
                default_read_denies()
                    .iter()
                    .map(expand)
                    .filter(|d| !allowed.iter().any(|a| within(d, a))),
            );
        }
        out.extend(self.policy.deny_read.iter().map(expand));
        out
    }

    /// [`Sandbox::read_deny_list`], canonical, missing ones skipped — what
    /// the OS backends enforce for a command starting now.
    pub fn read_denies(&self, workspace: &Path) -> Vec<PathBuf> {
        canonical_existing(&self.read_deny_list(workspace))
    }

    /// The part of [`Sandbox::read_denies`] ferrule may rewrite the ACLs of
    /// on Windows: its own secrets (`hidden`) and the owner's `deny_read`.
    /// The default credential dirs belong to other programs, which check
    /// their ACLs (OpenSSH) or own them (browsers); only the file tools
    /// refuse those on Windows.
    pub fn owned_denies(&self, workspace: &Path) -> Vec<PathBuf> {
        let home = home_dir();
        let mut out = self.policy.hidden.clone();
        out.extend(
            self.policy
                .deny_read
                .iter()
                .map(|p| expand(p, home.as_deref(), workspace)),
        );
        canonical_existing(&out)
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
        let net = match (self.policy.network, self.backend) {
            (true, _) => "Network access is allowed.",
            // Not enforced there (doctor says so); the model is still told.
            (false, Backend::Windows) => "Don't use the network.",
            (false, _) => "Network access is blocked.",
        };
        Some(format!("Commands run in a sandbox. {fs} {net}"))
    }

    fn probe(&self) -> Result<(), String> {
        let dir = std::env::temp_dir();
        // On Windows, the shell commands will actually use: Git Bash or
        // PowerShell failing under the token is what matters.
        let (program, args): (std::ffi::OsString, Vec<std::ffi::OsString>) =
            if self.backend == Backend::Windows {
                let shell = Shell::get();
                (shell.program.clone().into_os_string(), shell.args("exit 0"))
            } else {
                ("/bin/sh".into(), vec!["-c".into(), "exit 0".into()])
            };
        let status = self
            .command(&program, &args, &dir)
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

/// The home dir: `HOME`, or on Windows `USERPROFILE` first (Git Bash sets
/// a `HOME` of its own spelling).
pub fn home_dir() -> Option<PathBuf> {
    let vars: &[&str] = if cfg!(windows) {
        &["USERPROFILE", "HOME"]
    } else {
        &["HOME"]
    };
    vars.iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `~/x` against the home dir, a relative path against the workspace.
fn expand(path: &Path, home: Option<&Path>, workspace: &Path) -> PathBuf {
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ if path.is_relative() => workspace.join(path),
        _ => path.to_path_buf(),
    }
}

/// Whether `path` is `dir` or under it, ignoring case where the
/// filesystem usually does.
fn within(path: &Path, dir: &Path) -> bool {
    if !cfg!(any(target_os = "macos", windows)) {
        return path.starts_with(dir);
    }
    let fold = |p: &Path| {
        p.components()
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
            .collect::<Vec<_>>()
    };
    fold(path).starts_with(&fold(dir))
}

fn canonical_existing(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in paths {
        if let Ok(p) = dunce::canonicalize(path) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// Where credentials and browser profiles usually live on this OS,
/// unexpanded (`~/…`, or absolute under `%APPDATA%`/`%LOCALAPPDATA%` on
/// Windows). Denied to commands and the file tools unless the policy says
/// `deny_default_reads = false` or re-opens one with `allow_read`.
pub fn default_read_denies() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = [
        "~/.ssh",
        "~/.aws",
        "~/.azure",
        "~/.config/gcloud",
        "~/.kube",
        "~/.docker/config.json",
        "~/.netrc",
        "~/.git-credentials",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    let browsers: &[&str] = if cfg!(target_os = "macos") {
        &[
            "~/Library/Application Support/Google/Chrome",
            "~/Library/Application Support/Chromium",
            "~/Library/Application Support/Microsoft Edge",
            "~/Library/Application Support/BraveSoftware",
            "~/Library/Application Support/Firefox",
            "~/Library/Safari",
            "~/Library/Cookies",
        ]
    } else if cfg!(windows) {
        &[]
    } else {
        &[
            "~/.mozilla",
            "~/.config/google-chrome",
            "~/.config/chromium",
            "~/.config/microsoft-edge",
            "~/.config/BraveSoftware",
        ]
    };
    out.extend(browsers.iter().map(PathBuf::from));
    if cfg!(windows) {
        for (var, rel) in [
            ("LOCALAPPDATA", "Google\\Chrome\\User Data"),
            ("LOCALAPPDATA", "Chromium\\User Data"),
            ("LOCALAPPDATA", "Microsoft\\Edge\\User Data"),
            ("LOCALAPPDATA", "BraveSoftware"),
            ("APPDATA", "Mozilla\\Firefox"),
            ("APPDATA", "gcloud"),
        ] {
            if let Some(base) = std::env::var_os(var).filter(|v| !v.is_empty()) {
                out.push(PathBuf::from(base).join(rel));
            }
        }
    }
    out
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
        // `network = false` isn't enforced here (it needs WFP, so admin);
        // writes and reads still are, and doctor warns about the rest.
        let _ = policy;
        launch::launcher()?;
        Ok(Backend::Windows)
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
        let ws_c = dunce::canonicalize(ws.path()).unwrap();
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
        let c = |p: &Path| dunce::canonicalize(p).unwrap();
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
    fn planning_is_read_only_with_no_network() {
        let ws = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let base = Sandbox {
            policy: Policy {
                network: true,
                writable_roots: vec![extra.path().to_path_buf()],
                ..policy(Mode::WorkspaceWrite)
            },
            ..Sandbox::off()
        };
        let plan = base.for_planning();
        assert_eq!(plan.policy.mode, Mode::ReadOnly);
        assert!(!plan.policy.network);
        assert!(plan.writable_roots(ws.path()).is_empty());
        // The run it plans for keeps its own policy.
        assert_eq!(base.policy.mode, Mode::WorkspaceWrite);
        assert!(base.policy.network);
        assert_eq!(Sandbox::off().for_planning().policy.mode, Mode::Off);
    }

    #[test]
    fn a_child_gets_its_git_dir_or_goes_read_only() {
        let ws = tempfile::tempdir().unwrap();
        let git = tempfile::tempdir().unwrap();
        let c = |p: &Path| dunce::canonicalize(p).unwrap();
        let base = Sandbox {
            policy: policy(Mode::WorkspaceWrite),
            ..Sandbox::off()
        };
        let worker = base.for_child(&[git.path().to_path_buf()], false);
        assert_eq!(
            worker.writable_roots(ws.path()),
            vec![c(ws.path()), c(git.path())]
        );
        let reader = base.for_child(&[git.path().to_path_buf()], true);
        assert_eq!(reader.policy.mode, Mode::ReadOnly);
        assert!(reader.writable_roots(ws.path()).is_empty());
        assert_eq!(base.writable_roots(ws.path()), vec![c(ws.path())]);
        // Off stays off: there's no OS sandbox to make read-only.
        assert_eq!(Sandbox::off().for_child(&[], true).policy.mode, Mode::Off);
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
    fn unconfined_keeps_scrubbing_env_and_the_read_denies() {
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
        assert_eq!(un.backend(), Backend::Seatbelt, "still hides reads");
        assert!(!un.is_active());
        assert!(un.is_hide_only() && un.hides_reads());
        assert!(un.policy().network, "the network is the user's");
        assert_eq!(un.degraded(), Some("mcp.servers.foo: sandbox = false"));
        // With no backend there is nothing to hide with.
        let none = Sandbox::off().unconfined("x");
        assert!(!none.hides_reads() && !none.is_hide_only());
        assert!(
            un.is_secret_var("MY_PROVIDER"),
            "kept the caller's secret_vars"
        );
        assert_eq!(un.extra_env, sb.extra_env, "credential env is preserved");
    }

    #[test]
    fn read_policy_parses_and_defaults_on() {
        let p: Policy = serde_json::from_str(
            r#"{"deny_read": [".env", "~/work/secret"], "allow_read": ["~/.kube"], "process_limit": 64, "memory_mb": 2048}"#,
        )
        .unwrap();
        assert_eq!(p.deny_read, [PathBuf::from(".env"), "~/work/secret".into()]);
        assert!(p.deny_default_reads);
        assert_eq!((p.process_limit, p.memory_mb), (64, Some(2048)));
        let d = Policy::default();
        assert!(d.deny_default_reads && d.deny_read.is_empty() && d.memory_mb.is_none());
        assert!(serde_json::from_str::<Policy>(r#"{"hidden": []}"#).is_err());
    }

    #[test]
    fn read_denies_merge_hidden_defaults_and_config() {
        let ws = tempfile::tempdir().unwrap();
        let Some(home) = home_dir() else { return };
        let sb = |policy: Policy| Sandbox {
            policy,
            ..Sandbox::off()
        };
        let base = Policy {
            hidden: vec!["/data/private".into()],
            deny_read: vec![".env".into(), "~/work/secret".into()],
            ..Policy::default()
        };
        let list = sb(base.clone()).read_deny_list(ws.path());
        assert!(list.contains(&PathBuf::from("/data/private")));
        assert!(
            list.contains(&ws.path().join(".env")),
            "relative to the workspace"
        );
        assert!(list.contains(&home.join("work/secret")));
        assert!(list.contains(&home.join(".ssh")));
        assert!(list.contains(&home.join(".aws")));

        // allow_read re-opens a default entry at or under it, never the rest.
        let opened = sb(Policy {
            allow_read: vec![
                "~/.ssh".into(),
                "~/.config".into(),
                "/data".into(),
                "~/work".into(),
            ],
            ..base.clone()
        })
        .read_deny_list(ws.path());
        assert!(!opened.contains(&home.join(".ssh")));
        assert!(!opened.contains(&home.join(".config/gcloud")));
        assert!(opened.contains(&home.join(".aws")));
        assert!(
            opened.contains(&PathBuf::from("/data/private")),
            "hidden stays"
        );
        assert!(
            opened.contains(&home.join("work/secret")),
            "deny_read stays"
        );
        // Allowing something under a denied dir doesn't open the dir.
        let partial = sb(Policy {
            allow_read: vec!["~/.ssh/known_hosts".into()],
            ..base.clone()
        })
        .read_deny_list(ws.path());
        assert!(partial.contains(&home.join(".ssh")));

        let no_defaults = sb(Policy {
            deny_default_reads: false,
            ..base
        })
        .read_deny_list(ws.path());
        assert!(!no_defaults.contains(&home.join(".ssh")));
        assert_eq!(no_defaults.len(), 3);
    }

    #[test]
    fn read_denies_resolve_and_skip_missing() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join(".env"), "K=v").unwrap();
        let sb = Sandbox {
            policy: Policy {
                deny_read: vec![".env".into(), "missing".into(), ".env".into()],
                deny_default_reads: false,
                ..Policy::default()
            },
            ..Sandbox::off()
        };
        assert_eq!(
            sb.read_denies(ws.path()),
            [dunce::canonicalize(ws.path().join(".env")).unwrap()]
        );
    }

    #[test]
    fn default_denies_cover_keys_clouds_and_browsers() {
        let list = default_read_denies();
        for want in ["~/.ssh", "~/.aws", "~/.config/gcloud", "~/.azure"] {
            assert!(list.contains(&PathBuf::from(want)), "{want}");
        }
        let text = format!("{list:?}").to_lowercase();
        assert!(text.contains("chrome") && text.contains("firefox") || text.contains("mozilla"));
        assert!(text.contains("edge"));
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
