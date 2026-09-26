//! One remote workspace over the system `ssh` (docs/m34-ssh-local.md §3–§7).
//!
//! On Unix a ControlMaster connection carries every command (14 ms each
//! instead of a handshake), and holds the `-R` forward to the credential
//! proxy. Its remote session is `cat`, reading ferrule's end of a pipe: when
//! ferrule dies, however it dies, the pipe closes and the master goes with
//! it. OpenSSH for Windows has no ControlMaster, so there every command is
//! its own connection.

use crate::classify::{classify, Failure};
use crate::script::{self, Op, READ_CAP};
use crate::target::Target;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// The Unix read denies, both Linux's and macOS' browser dirs: the remote's
/// OS isn't known until it answers, and a path that isn't there costs
/// nothing.
pub const REMOTE_DEFAULT_DENIES: &[&str] = &[
    "~/.ssh",
    "~/.aws",
    "~/.azure",
    "~/.config/gcloud",
    "~/.kube",
    "~/.docker/config.json",
    "~/.netrc",
    "~/.git-credentials",
    "~/.mozilla",
    "~/.config/google-chrome",
    "~/.config/chromium",
    "~/.config/microsoft-edge",
    "~/.config/BraveSoftware",
    "~/Library/Application Support/Google/Chrome",
    "~/Library/Application Support/Chromium",
    "~/Library/Application Support/Microsoft Edge",
    "~/Library/Application Support/BraveSoftware",
    "~/Library/Application Support/Firefox",
    "~/Library/Safari",
    "~/Library/Cookies",
];

/// The sandbox's read policy, applied to the remote file tools: `~` is the
/// remote home, a relative path is under the remote workspace.
#[derive(Debug, Clone, Default)]
pub struct DenySpec {
    /// The credential and browser dirs ([`REMOTE_DEFAULT_DENIES`]).
    pub default: bool,
    pub deny_read: Vec<String>,
    /// Re-opens a default, spelled the same way (`~/.kube`).
    pub allow_read: Vec<String>,
}

impl DenySpec {
    fn specs(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if self.default {
            let allowed = |d: &str| self.allow_read.iter().any(|a| a.trim_end_matches('/') == d);
            out.extend(
                REMOTE_DEFAULT_DENIES
                    .iter()
                    .filter(|d| !allowed(d))
                    .map(|d| d.to_string()),
            );
        }
        out.extend(self.deny_read.iter().cloned());
        out
    }
}

/// The environment a remote command gets for the credential proxy.
pub type EnvFn = Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>;

/// ferrule's credential proxy, reached from the remote through `ssh -R`.
#[derive(Clone)]
pub struct Forward {
    /// The broker's local port.
    pub port: u16,
    /// Its child env (`HTTPS_PROXY`, the CA bundle vars …), asked on every
    /// command so a binding added at runtime is seen. `127.0.0.1:<port>`
    /// is rewritten to the remote end of the forward, and a value that is
    /// a local file (the CA bundle) is uploaded once and pointed at.
    pub env: EnvFn,
}

#[derive(Clone, Default)]
pub struct LinkOptions {
    /// ferrule's own known_hosts (`<data>/ssh/known_hosts`), where `ferrule
    /// ssh trust` writes. Checked after the owner's own files.
    pub known_hosts: Option<PathBuf>,
    /// The owner's known_hosts files; see [`default_user_known_hosts`].
    pub user_known_hosts: Vec<PathBuf>,
    pub deny: DenySpec,
    pub forward: Option<Forward>,
    /// More `ssh` arguments, before the host (tests: `-o
    /// GlobalKnownHostsFile=/dev/null`).
    pub extra_args: Vec<String>,
}

/// `~/.ssh/known_hosts` and `~/.ssh/known_hosts2`, ssh's own defaults.
pub fn default_user_known_hosts() -> Vec<PathBuf> {
    match ferrule_sandbox::home_dir() {
        Some(home) => vec![
            home.join(".ssh").join("known_hosts"),
            home.join(".ssh").join("known_hosts2"),
        ],
        None => Vec::new(),
    }
}

/// What the remote said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub home: String,
    /// The workspace's real path.
    pub workspace: String,
    /// `uname -s`.
    pub os: String,
    pub writable: bool,
    /// The read denies, resolved there.
    pub denies: Vec<String>,
    pub has_curl: bool,
    pub has_git: bool,
}

impl Remote {
    /// macOS' file system ignores case, so the path checks do too.
    pub fn icase(&self) -> bool {
        self.os == "Darwin"
    }
}

/// How a script ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Exit(i32),
    /// The script refused: escape, hidden, missing …
    Fail(String),
    /// It started and the link dropped before it ended.
    Interrupted,
}

#[derive(Debug, Clone)]
pub struct Ran {
    pub stdout: Vec<u8>,
    /// Whether stdout went past the cap and the rest was dropped.
    pub stdout_cut: bool,
    /// The script's stderr, without the markers.
    pub stderr: String,
    pub status: Status,
}

/// What a script's stdin carries after the parameters.
pub(crate) enum Data {
    /// This, then EOF.
    Close(Vec<u8>),
    /// Nothing, and stdin stays open until ssh exits: closing it is how
    /// the remote learns to kill the command.
    HoldOpen,
}

#[derive(Debug, Clone, Default)]
struct Health {
    connected: bool,
    since: Option<Instant>,
    last_error: Option<String>,
    reconnects: u32,
    forward_note: Option<String>,
}

#[cfg(unix)]
struct Master {
    child: tokio::process::Child,
    /// Held: its EOF is what ends the master if ferrule dies.
    _lifeline: tokio::process::ChildStdin,
    dir: tempfile::TempDir,
    rport: Option<u16>,
}

#[cfg(unix)]
impl Master {
    fn socket(&self) -> PathBuf {
        self.dir.path().join("m")
    }
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None)) && self.socket().exists()
    }
}

/// Retries for a connection that didn't come up, then give up.
const BACKOFF: &[u64] = &[500, 1000, 2000, 4000];

pub struct Link {
    target: Target,
    opts: LinkOptions,
    remote: OnceLock<Remote>,
    hello: tokio::sync::Mutex<()>,
    #[cfg(unix)]
    master: tokio::sync::Mutex<Option<Master>>,
    /// Where the forward listens on the remote; kept across reconnects.
    rport: Mutex<Option<u16>>,
    /// A changed host key: nothing more goes to this host.
    poisoned: Mutex<Option<String>>,
    uploads: tokio::sync::Mutex<HashMap<PathBuf, String>>,
    health: Mutex<Health>,
}

impl Link {
    pub fn new(target: Target, opts: LinkOptions) -> Arc<Link> {
        Arc::new(Link {
            target,
            opts,
            remote: OnceLock::new(),
            hello: tokio::sync::Mutex::new(()),
            #[cfg(unix)]
            master: tokio::sync::Mutex::new(None),
            rport: Mutex::new(None),
            poisoned: Mutex::new(None),
            uploads: tokio::sync::Mutex::new(HashMap::new()),
            health: Mutex::new(Health::default()),
        })
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    /// The remote, once [`Link::connect`] has succeeded.
    pub fn remote(&self) -> Option<&Remote> {
        self.remote.get()
    }

    /// The `ssh` program: the target's, else `FERRULE_SSH`, else `ssh`.
    pub fn program(&self) -> PathBuf {
        ssh_program(&self.target)
    }

    /// Connect (once) and learn the remote. Later calls return what the
    /// first one learned.
    pub async fn connect(&self) -> Result<Remote, String> {
        if let Some(r) = self.remote.get() {
            return Ok(r.clone());
        }
        let _one = self.hello.lock().await;
        if let Some(r) = self.remote.get() {
            return Ok(r.clone());
        }
        let specs = self.opts.deny.specs();
        let mut params = vec![self.target.path.clone(), specs.len().to_string()];
        params.extend(specs);
        let ran = self
            .exec(
                Op::Hello,
                params,
                Data::Close(Vec::new()),
                1 << 20,
                Some(Duration::from_secs(60)),
                None,
            )
            .await?;
        match &ran.status {
            Status::Exit(0) => {}
            Status::Fail(code) if code == "nows" => {
                return Err(format!(
                    "{}: the remote workspace `{}` isn't a directory on {} (or can't be entered); create it first, e.g. `ssh {} mkdir -p {}`",
                    self.target.label, self.target.path, self.target.host, self.target.host, self.target.path
                ))
            }
            other => return Err(self.unexpected("connecting", other, &ran)),
        }
        let mut remote = Remote {
            home: String::new(),
            workspace: String::new(),
            os: String::new(),
            writable: false,
            denies: Vec::new(),
            has_curl: false,
            has_git: false,
        };
        for line in String::from_utf8_lossy(&ran.stdout).lines() {
            let Some((k, v)) = line.split_once('\t') else {
                continue;
            };
            match k {
                "home" => remote.home = v.to_string(),
                "os" => remote.os = v.to_string(),
                "ws" => remote.workspace = v.to_string(),
                "writable" => remote.writable = true,
                "deny" => remote.denies.push(v.to_string()),
                "has" if v == "curl" => remote.has_curl = true,
                "has" if v == "git" => remote.has_git = true,
                _ => {}
            }
        }
        if !remote.workspace.starts_with('/') {
            return Err(format!(
                "{}: the remote didn't say where the workspace is; is its shell POSIX (sh)?",
                self.target.label
            ));
        }
        let _ = self.remote.set(remote.clone());
        Ok(remote)
    }

    /// One line for `/status` and the dashboard.
    pub fn status_line(&self) -> String {
        let h = self.health.lock().unwrap().clone();
        let mut s = self.target.describe();
        if let Some(p) = self.poisoned.lock().unwrap().as_deref() {
            let _ = p;
            s.push_str(" — STOPPED: the host key changed");
            return s;
        }
        match (&h.last_error, h.connected) {
            (Some(e), _) => s.push_str(&format!(" — not connected: {e}")),
            (None, true) => {
                let how = if cfg!(unix) {
                    "multiplexed"
                } else {
                    "one connection per command"
                };
                s.push_str(&format!(" — connected ({how})"));
                if let Some(since) = h.since {
                    s.push_str(&format!(" for {}s", since.elapsed().as_secs()));
                }
            }
            (None, false) => s.push_str(" — not connected yet"),
        }
        if h.reconnects > 0 {
            s.push_str(&format!(", {} reconnect(s)", h.reconnects));
        }
        if let Some(r) = self.remote.get() {
            s.push_str(&format!(", {} at {}", r.os, r.workspace));
            if !r.writable {
                s.push_str(" (read-only for this account)");
            }
        }
        if self.opts.forward.is_some() {
            match &h.forward_note {
                Some(n) => s.push_str(&format!("; credential proxy: {n}")),
                None if cfg!(unix) && h.connected => s.push_str("; credential proxy forwarded"),
                None => {}
            }
        }
        s
    }

    /// The last connection error, if the link is down.
    pub fn last_error(&self) -> Option<String> {
        if let Some(p) = self.poisoned.lock().unwrap().clone() {
            return Some(p);
        }
        self.health.lock().unwrap().last_error.clone()
    }

    /// The master's pid (tests kill it to drop the link).
    #[cfg(unix)]
    pub async fn master_pid(&self) -> Option<u32> {
        self.master.lock().await.as_ref().and_then(|m| m.child.id())
    }

    /// Stop the master (and with it the forward). The next call reconnects.
    pub async fn disconnect(&self) {
        #[cfg(unix)]
        {
            if let Some(mut m) = self.master.lock().await.take() {
                let _ = m.child.kill().await;
            }
        }
        self.health.lock().unwrap().connected = false;
    }

    fn base_command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(self.program());
        // The local ssh gets no credential-looking env (it has no use for
        // it, and SendEnv could pass it on). SSH_AUTH_SOCK stays.
        for (name, _) in std::env::vars_os() {
            if ferrule_sandbox::looks_secret(&name.to_string_lossy()) {
                c.env_remove(&name);
            }
        }
        c.arg("-T");
        for o in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "UpdateHostKeys=no",
            "ConnectTimeout=10",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
            "LogLevel=ERROR",
            "ForwardAgent=no",
            "ForwardX11=no",
            "PermitLocalCommand=no",
        ] {
            c.arg("-o").arg(o);
        }
        c.arg("-o").arg(known_hosts_option(&self.opts));
        if let Some(port) = self.target.port {
            c.arg("-p").arg(port.to_string());
        }
        if let Some(user) = &self.target.user {
            c.arg("-l").arg(user);
        }
        if let Some(id) = &self.target.identity_file {
            c.arg("-i").arg(id).arg("-o").arg("IdentitiesOnly=yes");
        }
        if let Some(cfg) = &self.target.ssh_config {
            c.arg("-F").arg(cfg);
        }
        c.args(&self.opts.extra_args);
        #[cfg(unix)]
        c.process_group(0);
        c.kill_on_drop(true);
        c
    }

    fn fail(&self, f: &Failure) -> String {
        let msg = f.message(&self.target);
        if let Failure::HostKeyChanged { .. } = f {
            *self.poisoned.lock().unwrap() = Some(msg.clone());
        }
        let mut h = self.health.lock().unwrap();
        h.connected = false;
        h.last_error = Some(f.kind().to_string());
        msg
    }

    fn up(&self) {
        let mut h = self.health.lock().unwrap();
        if !h.connected {
            h.connected = true;
            h.since = Some(Instant::now());
        }
        h.last_error = None;
    }

    fn poison_check(&self) -> Result<(), String> {
        match self.poisoned.lock().unwrap().clone() {
            Some(p) => Err(p),
            None => Ok(()),
        }
    }

    fn pick_port(&self, fresh: bool) -> u16 {
        let mut g = self.rport.lock().unwrap();
        if let (Some(p), false) = (*g, fresh) {
            return p;
        }
        let r = uuid::Uuid::new_v4().as_u128();
        let p = 20_000 + (r % 40_000) as u16;
        *g = Some(p);
        p
    }

    /// The master, started or restarted as needed. Returns its socket and
    /// the forward's remote port.
    #[cfg(unix)]
    async fn ensure_master(&self) -> Result<(PathBuf, Option<u16>), Failure> {
        let mut g = self.master.lock().await;
        if let Some(m) = g.as_mut() {
            if m.alive() {
                return Ok((m.socket(), m.rport));
            }
            *g = None;
        }
        // Up before: this is a reconnect.
        let again = self.health.lock().unwrap().since.is_some();
        let mut tries = 0;
        let mut fresh = false;
        loop {
            let forward = self.opts.forward.as_ref().filter(|_| tries < 3);
            let rport = forward.map(|_| self.pick_port(fresh));
            match self.start_master(forward, rport).await {
                Ok(m) => {
                    let out = (m.socket(), m.rport);
                    if again {
                        self.health.lock().unwrap().reconnects += 1;
                    }
                    if forward.is_some() {
                        self.health.lock().unwrap().forward_note = None;
                    }
                    *g = Some(m);
                    return Ok(out);
                }
                Err(Failure::Forward) if forward.is_some() => {
                    tries += 1;
                    fresh = true;
                    if tries == 3 {
                        self.health.lock().unwrap().forward_note = Some(
                            "unavailable (the server refused the port forward; is AllowTcpForwarding off?)"
                                .into(),
                        );
                    }
                }
                Err(f) => return Err(f),
            }
        }
    }

    #[cfg(unix)]
    async fn start_master(
        &self,
        forward: Option<&Forward>,
        rport: Option<u16>,
    ) -> Result<Master, Failure> {
        // A short path: macOS caps a socket's at 104 bytes.
        let dir = tempfile::Builder::new()
            .prefix("fssh")
            .tempdir_in("/tmp")
            .or_else(|_| tempfile::Builder::new().prefix("fssh").tempdir())
            .map_err(|e| Failure::Other(format!("no temp dir for the ssh socket: {e}")))?;
        let sock = dir.path().join("m");
        let err_path = dir.path().join("err");
        let err_file = std::fs::File::create(&err_path)
            .map_err(|e| Failure::Other(format!("no temp file for ssh's errors: {e}")))?;
        let mut c = self.base_command();
        c.arg("-M")
            .arg("-o")
            .arg("ControlPersist=no")
            .arg("-o")
            .arg(format!("ControlPath={}", sock.display()));
        match (forward, rport) {
            (Some(f), Some(r)) => {
                c.arg("-R")
                    .arg(format!("127.0.0.1:{r}:127.0.0.1:{}", f.port))
                    .arg("-o")
                    .arg("ExitOnForwardFailure=yes");
            }
            _ => {
                c.arg("-o").arg("ClearAllForwardings=yes");
            }
        }
        c.arg("--")
            .arg(&self.target.host)
            .arg("exec sh -c 'cat >/dev/null'")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err_file));
        let mut child = c.spawn().map_err(|e| {
            Failure::Other(format!("can't run `{}`: {e}", self.program().display()))
        })?;
        let lifeline = child.stdin.take().expect("piped");
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            if sock.exists() {
                return Ok(Master {
                    child,
                    _lifeline: lifeline,
                    dir,
                    rport: forward.and(rport),
                });
            }
            if let Ok(Some(_)) = child.try_wait() {
                let err = std::fs::read_to_string(&err_path).unwrap_or_default();
                return Err(classify(&err));
            }
            if Instant::now() > deadline {
                return Err(Failure::Unreachable(
                    "timed out setting up the connection".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The remote port for the proxy forward, if there is one, connecting
    /// first on Unix (the master holds it). On Windows, a fresh one per
    /// command.
    pub(crate) async fn forward_port(&self) -> Result<Option<u16>, String> {
        if self.opts.forward.is_none() {
            return Ok(None);
        }
        #[cfg(unix)]
        {
            self.poison_check()?;
            match self.ensure_master().await {
                Ok((_, rport)) => Ok(rport),
                Err(f) => Err(self.fail(&f)),
            }
        }
        #[cfg(not(unix))]
        {
            Ok(Some(self.pick_port(true)))
        }
    }

    /// The proxy env for a remote command, pointed at the remote end of
    /// the forward. Empty without a forward.
    pub(crate) async fn forward_env(
        &self,
        rport: Option<u16>,
    ) -> Result<Vec<(String, String)>, String> {
        let (Some(fwd), Some(rport)) = (&self.opts.forward, rport) else {
            return Ok(Vec::new());
        };
        let local = format!("127.0.0.1:{}", fwd.port);
        let there = format!("127.0.0.1:{rport}");
        let mut out = Vec::new();
        for (name, value) in (fwd.env)() {
            let mut value = value
                .replace(&local, &there)
                .replace(&format!("localhost:{}", fwd.port), &there);
            let path = Path::new(&value);
            if path.is_absolute() && path.is_file() {
                value = self.upload(path).await?;
            }
            out.push((name, value));
        }
        Ok(out)
    }

    /// Copy a local file (the proxy's CA bundle) to a private temp dir on
    /// the remote, once per path.
    async fn upload(&self, path: &Path) -> Result<String, String> {
        let mut cache = self.uploads.lock().await;
        if let Some(r) = cache.get(path) {
            return Ok(r.clone());
        }
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.contains('/') && !n.starts_with('.'))
            .unwrap_or_else(|| "file".into());
        let ran = self
            .exec(
                Op::Upload,
                vec![name],
                Data::Close(bytes),
                1 << 16,
                Some(Duration::from_secs(60)),
                None,
            )
            .await?;
        if ran.status != Status::Exit(0) {
            return Err(self.unexpected("uploading the proxy's CA", &ran.status, &ran));
        }
        let remote = String::from_utf8_lossy(&ran.stdout).trim_end().to_string();
        cache.insert(path.to_path_buf(), remote.clone());
        Ok(remote)
    }

    pub(crate) fn unexpected(&self, doing: &str, status: &Status, ran: &Ran) -> String {
        let said = ran.stderr.trim();
        let said = if said.is_empty() {
            String::new()
        } else {
            format!(": {}", tail(said, 2000))
        };
        match status {
            Status::Interrupted => format!(
                "{}: the connection dropped while {doing}{said}",
                self.target.label
            ),
            Status::Fail(code) => format!(
                "{}: refused ({code}) while {doing}{said}",
                self.target.label
            ),
            Status::Exit(rc) => {
                format!("{}: exit code {rc} while {doing}{said}", self.target.label)
            }
        }
    }

    /// Run `op` on the remote. Retries a connection that didn't come up
    /// (backoff 0.5, 1, 2, 4 s), and an idempotent op the link dropped
    /// under; anything else that drops mid-way is [`Status::Interrupted`].
    pub(crate) async fn exec(
        &self,
        op: Op,
        params: Vec<String>,
        data: Data,
        cap: usize,
        timeout: Option<Duration>,
        cmd_rport: Option<u16>,
    ) -> Result<Ran, String> {
        let mut attempt = 0usize;
        let data_bytes = match &data {
            Data::Close(b) => Some(b.clone()),
            Data::HoldOpen => None,
        };
        loop {
            self.poison_check()?;
            let backoff =
                |attempt: usize| BACKOFF.get(attempt).map(|ms| Duration::from_millis(*ms));
            let mut cmd = self.base_command();
            #[cfg(unix)]
            {
                let _ = cmd_rport;
                match self.ensure_master().await {
                    Ok((sock, _)) => {
                        cmd.arg("-o")
                            .arg("ControlMaster=no")
                            .arg("-o")
                            .arg(format!("ControlPath={}", sock.display()));
                    }
                    Err(f) => {
                        let msg = self.fail(&f);
                        match (f.retryable(), backoff(attempt)) {
                            (true, Some(wait)) => {
                                attempt += 1;
                                tokio::time::sleep(wait).await;
                                continue;
                            }
                            _ => return Err(msg),
                        }
                    }
                }
                cmd.arg("-o").arg("ClearAllForwardings=yes");
            }
            #[cfg(not(unix))]
            match (&self.opts.forward, cmd_rport) {
                (Some(f), Some(r)) => {
                    cmd.arg("-R")
                        .arg(format!("127.0.0.1:{r}:127.0.0.1:{}", f.port));
                }
                _ => {
                    cmd.arg("-o").arg("ClearAllForwardings=yes");
                }
            }
            let nonce = uuid::Uuid::new_v4().to_string();
            let header = script::stdin_header(op, &nonce, &params)?;
            cmd.arg("--")
                .arg(&self.target.host)
                .arg(script::BOOTSTRAP)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd
                .spawn()
                .map_err(|e| format!("can't run `{}`: {e}", self.program().display()))?;
            let mut stdin = child.stdin.take().expect("piped");
            let stdout = child.stdout.take().expect("piped");
            let stderr = child.stderr.take().expect("piped");
            let data_now = data_bytes.clone();
            let run = async move {
                let writer = async move {
                    let _ = stdin.write_all(&header).await;
                    match data_now {
                        Some(bytes) => {
                            let _ = stdin.write_all(&bytes).await;
                            drop(stdin);
                            None
                        }
                        None => {
                            let _ = stdin.flush().await;
                            Some(stdin)
                        }
                    }
                };
                let (held, (out, cut), (err, _)) = tokio::join!(
                    writer,
                    read_capped(stdout, cap),
                    read_capped(stderr, 1 << 20)
                );
                let status = child.wait().await;
                drop(held);
                (status, out, cut, err)
            };
            let (status, out, cut, err) = match timeout {
                Some(t) => match tokio::time::timeout(t, run).await {
                    Ok(r) => r,
                    Err(_) => return Err(format!("timeout after {t:?}")),
                },
                None => run.await,
            };
            let code = status.ok().and_then(|s| s.code());
            let (markers, stderr) = script::strip_markers(&String::from_utf8_lossy(&err), &nonce);
            let ran = |status| Ran {
                stdout: out.clone(),
                stdout_cut: cut,
                stderr: stderr.clone(),
                status,
            };
            if let Some(f) = markers.fail {
                self.up();
                return Ok(ran(Status::Fail(f)));
            }
            if let Some(rc) = markers.exit {
                self.up();
                return Ok(ran(Status::Exit(rc)));
            }
            if markers.started {
                // It ran and the link dropped under it.
                self.drop_master().await;
                self.health.lock().unwrap().last_error = Some("the connection dropped".into());
                self.health.lock().unwrap().connected = false;
                if op.idempotent() {
                    if let Some(wait) = backoff(attempt) {
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                }
                return Ok(ran(Status::Interrupted));
            }
            if code == Some(255) || code.is_none() {
                let f = classify(&stderr);
                let msg = self.fail(&f);
                self.drop_master().await;
                match (f.retryable(), backoff(attempt)) {
                    (true, Some(wait)) => {
                        attempt += 1;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    _ => return Err(msg),
                }
            }
            return Err(format!(
                "{}: the remote login shell didn't run ferrule's script (exit {}){}; ferrule needs a POSIX `sh` there",
                self.target.label,
                code.unwrap_or(-1),
                match stderr.trim() {
                    "" => String::new(),
                    s => format!(": {}", tail(s, 500)),
                }
            ));
        }
    }

    async fn drop_master(&self) {
        #[cfg(unix)]
        {
            let mut g = self.master.lock().await;
            if let Some(m) = g.as_mut() {
                if !m.alive() {
                    *g = None;
                }
            }
        }
    }

    /// The common file-op params: workspace, case flag, denies.
    pub(crate) fn file_params(&self, remote: &Remote) -> Vec<String> {
        let mut p = vec![
            remote.workspace.clone(),
            if remote.icase() { "1" } else { "0" }.to_string(),
            remote.denies.len().to_string(),
        ];
        p.extend(remote.denies.iter().cloned());
        p
    }

    /// Read a remote file: `None` when it doesn't exist.
    pub(crate) async fn read_file(&self, remote: &Remote, path: &str) -> Result<Ran, String> {
        let mut params = self.file_params(remote);
        params.push(path.to_string());
        self.exec(
            Op::Read,
            params,
            Data::Close(Vec::new()),
            READ_CAP + 1,
            Some(Duration::from_secs(120)),
            None,
        )
        .await
    }
}

/// `-o UserKnownHostsFile="a" "b" "c"`: the owner's files, then ferrule's.
fn known_hosts_option(opts: &LinkOptions) -> String {
    let files: Vec<String> = opts
        .user_known_hosts
        .iter()
        .chain(opts.known_hosts.iter())
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.contains('"'))
        .map(|p| format!("\"{}\"", p.replace('%', "%%")))
        .collect();
    if files.is_empty() {
        "UserKnownHostsFile=none".into()
    } else {
        format!("UserKnownHostsFile={}", files.join(" "))
    }
}

/// The `ssh` a target uses: its own, `FERRULE_SSH`, or `ssh` on `PATH`.
pub fn ssh_program(target: &Target) -> PathBuf {
    if let Some(p) = &target.ssh {
        return p.clone();
    }
    match std::env::var_os("FERRULE_SSH") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from("ssh"),
    }
}

/// Read all of `r`, keeping the first `cap` bytes. Keeps draining past the
/// cap, so the remote never blocks on a full pipe.
async fn read_capped<R: AsyncRead + Unpin>(mut r: R, cap: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut cut = false;
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(out.len());
                if n > room {
                    cut = true;
                }
                out.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    (out, cut)
}

/// The last `max` characters of `text`, marked when something was cut.
pub(crate) fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let rest: String = text.chars().skip(count - max).collect();
    format!("[... {} earlier characters cut]\n{rest}", count - max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_drop_what_allow_read_reopens() {
        let spec = DenySpec {
            default: true,
            deny_read: vec!["secrets".into()],
            allow_read: vec!["~/.kube/".into()],
        };
        let s = spec.specs();
        assert!(s.contains(&"~/.ssh".to_string()) && s.contains(&"secrets".to_string()));
        assert!(!s.contains(&"~/.kube".to_string()));
        assert_eq!(DenySpec::default().specs(), Vec::<String>::new());
    }

    #[test]
    fn known_hosts_are_quoted_and_percent_safe() {
        let opts = LinkOptions {
            known_hosts: Some("/d/ssh/known_hosts".into()),
            user_known_hosts: vec!["/h/my 100%/known_hosts".into()],
            ..Default::default()
        };
        assert_eq!(
            known_hosts_option(&opts),
            "UserKnownHostsFile=\"/h/my 100%%/known_hosts\" \"/d/ssh/known_hosts\""
        );
        assert_eq!(
            known_hosts_option(&LinkOptions::default()),
            "UserKnownHostsFile=none"
        );
    }
}
