use crate::config::McpServerConfig;
use crate::error::McpError;
use crate::http::HttpTransport;
use ferrule_sandbox::Sandbox;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, watch, Mutex as AsyncMutex};

pub(crate) const PROTOCOL_VERSION: &str = "2025-06-18";

type PendingMap = Arc<StdMutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// The parts of a live connection an in-flight call needs. Cloned out from
/// under the client's connection lock, so one slow call never blocks other
/// calls to the same server.
#[derive(Clone)]
struct Handle {
    stdin: Arc<AsyncMutex<ChildStdin>>,
    pending: PendingMap,
    alive: Arc<AtomicBool>,
}

/// One live stdio connection to an MCP server: the child process plus the
/// background tasks draining its stdout (routing responses) and stderr
/// (into `tracing`).
struct Connection {
    handle: Handle,
    // Kept alive only for `kill_on_drop`; never read/written directly again
    // once the reader/writer handles above are split off.
    _child: Child,
    _reader: tokio::task::JoinHandle<()>,
    _stderr: tokio::task::JoinHandle<()>,
}

/// Where the host runs a server: its shared sandbox (already carrying the
/// credential proxy's env), the agent's workspace, which is the server's
/// working directory, and the server's own state dir (`<data dir>/mcp/<name>`
/// in the CLI), which must exist.
#[derive(Clone)]
pub struct ServerHost {
    pub sandbox: Arc<Sandbox>,
    pub workspace: PathBuf,
    pub state_dir: PathBuf,
}

/// A client for one MCP server: a command spoken to over stdio, or a URL.
/// For a command it owns lazy respawn: a dead connection is only
/// reconnected on the next call, and only once — if that attempt also
/// fails, the call reports an error rather than looping.
pub struct McpClient {
    cfg: McpServerConfig,
    /// Set for a server reached by URL; `conn` is then never used.
    http: Option<HttpTransport>,
    /// This server's own sandbox: the host's, plus its state dir and
    /// `writable_roots`, or unconfined for `sandbox = false`. Either way
    /// secret-looking env is scrubbed and the credential proxy's env set.
    sandbox: Sandbox,
    workspace: PathBuf,
    state_dir: PathBuf,
    conn: AsyncMutex<Option<Connection>>,
    next_id: AtomicU64,
    /// Bumped on every `notifications/tools/list_changed` from any of this
    /// server's connections, respawns included. M13 re-lists and re-scans on
    /// it; nothing else reads it.
    list_changed: watch::Sender<u64>,
    /// Set by `shutdown`: no respawn after that.
    closed: AtomicBool,
}

pub(crate) fn extract_result(resp: Value) -> Result<Value, McpError> {
    if let Some(err) = resp.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("mcp error")
            .to_string();
        return Err(McpError::Rpc { code, message });
    }
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

impl McpClient {
    /// Fails only on a config that can't work: both or neither of
    /// `command` and `url`, a bad URL or header, or a `${VAR}` in a header
    /// that isn't set. Nothing is started yet.
    pub fn new(cfg: McpServerConfig, host: ServerHost) -> Result<Self, McpError> {
        let http = match (cfg.url.as_deref(), cfg.command.is_empty()) {
            _ if cfg.local.is_some() => {
                if cfg.url.is_some() || !cfg.command.is_empty() {
                    return Err(McpError::Config(
                        "a built-in connection has no `command` or `url`".into(),
                    ));
                }
                None
            }
            (Some(_), false) => {
                return Err(McpError::Config("set `command` or `url`, not both".into()))
            }
            (None, true) => return Err(McpError::Config("set `command` or `url`".into())),
            (Some(url), true) => Some(HttpTransport::new(
                url,
                &cfg.headers,
                |name| host.sandbox.child_env_var(name),
                host.sandbox.egress(),
                cfg.startup_timeout(),
                cfg.auth.clone(),
            )?),
            (None, false) => None,
        };
        let sandbox = if cfg.sandbox {
            let helper = host
                .sandbox
                .for_helper(&host.state_dir, &cfg.writable_roots);
            if cfg.desktop_services {
                helper.with_desktop_services()
            } else {
                helper
            }
        } else {
            host.sandbox
                .unconfined(format!("mcp.servers `{}` has sandbox = false", cfg.name))
        };
        Ok(Self {
            cfg,
            http,
            sandbox,
            workspace: host.workspace,
            state_dir: host.state_dir,
            conn: AsyncMutex::new(None),
            next_id: AtomicU64::new(1),
            list_changed: watch::channel(0).0,
            closed: AtomicBool::new(false),
        })
    }

    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    pub fn config(&self) -> &McpServerConfig {
        &self.cfg
    }

    /// A child process on a pipe rather than a URL. Its calls are kept one
    /// at a time (M27): a timeout counts from the send, and a server that
    /// reads one request at a time would time out the second.
    pub fn is_stdio(&self) -> bool {
        self.http.is_none() && self.cfg.local.is_none()
    }

    /// Wakes whenever the server sends `notifications/tools/list_changed`.
    /// Only stdio servers can: a URL server would need the server-initiated
    /// stream this client doesn't open.
    pub fn subscribe_list_changed(&self) -> watch::Receiver<u64> {
        self.list_changed.subscribe()
    }

    /// Stop the server for good: kill the process, wait for it, and refuse
    /// to respawn. Calls in flight end with `ConnectionClosed`.
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let conn = self.conn.lock().await.take();
        if let Some(mut conn) = conn {
            let _ = conn._child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), conn._child.wait()).await;
            conn._reader.abort();
            conn._stderr.abort();
        }
    }

    /// Why this server runs without the OS sandbox, if it does:
    /// `sandbox = false` in its config, or none available here. `None` for
    /// a server reached by URL, which runs nowhere near this machine.
    pub fn sandbox_degraded(&self) -> Option<&str> {
        match self.http {
            _ if self.cfg.local.is_some() => None,
            Some(_) => None,
            None => self.sandbox.degraded(),
        }
    }

    /// Whether the OS keeps this server out of the read denies, even when
    /// it's otherwise unconfined.
    pub fn hides_reads(&self) -> bool {
        self.is_stdio() && self.sandbox.hides_reads()
    }

    /// Ensure a live connection exists (spawning + handshaking if needed),
    /// send `method`/`params`, and wait up to `timeout` for the matching
    /// response. A dead or absent connection triggers exactly one respawn
    /// attempt per call — never a retry loop.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        if let Some(local) = &self.cfg.local {
            return tokio::time::timeout(timeout, local.request(method, params))
                .await
                .map_err(|_| McpError::Timeout)?;
        }
        if let Some(http) = &self.http {
            let resp = http.request(&self.next_id, method, params, timeout).await?;
            return extract_result(resp);
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::NotConnected);
        }
        let handle = {
            let mut guard = self.conn.lock().await;
            let needs_connect = match guard.as_ref() {
                Some(c) => !c.handle.alive.load(Ordering::SeqCst),
                None => true,
            };
            if needs_connect {
                *guard = None;
                let fresh = self.spawn_and_handshake().await?;
                *guard = Some(fresh);
            }
            guard.as_ref().expect("just connected").handle.clone()
        };
        let result = Self::send_and_wait(&handle, &self.next_id, method, params, timeout).await;
        extract_result(result?)
    }

    /// Fire-and-forget notification (no response expected). Only used for
    /// `notifications/initialized`, sent directly against a fresh connection
    /// during the handshake — never through `request`.
    async fn send_notification(conn: &Handle, method: &str) -> Result<(), McpError> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": {}});
        let mut line = serde_json::to_string(&msg)?;
        line.push('\n');
        let mut w = conn.stdin.lock().await;
        w.write_all(line.as_bytes()).await.map_err(McpError::Io)?;
        w.flush().await.map_err(McpError::Io)?;
        Ok(())
    }

    async fn send_and_wait(
        conn: &Handle,
        next_id: &AtomicU64,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        let id = next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        conn.pending.lock().unwrap().insert(id, tx);

        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_string(&msg)?;
        line.push('\n');
        {
            let mut w = conn.stdin.lock().await;
            if let Err(e) = w.write_all(line.as_bytes()).await {
                conn.pending.lock().unwrap().remove(&id);
                conn.alive.store(false, Ordering::SeqCst);
                return Err(McpError::Io(e));
            }
            if let Err(e) = w.flush().await {
                conn.pending.lock().unwrap().remove(&id);
                conn.alive.store(false, Ordering::SeqCst);
                return Err(McpError::Io(e));
            }
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(value)) => Ok(value),
            // Sender was dropped — the reader task saw EOF/error and gave up
            // on every in-flight call.
            Ok(Err(_)) => Err(McpError::ConnectionClosed),
            Err(_) => {
                conn.pending.lock().unwrap().remove(&id);
                Err(McpError::Timeout)
            }
        }
    }

    /// The server's command, spawned the way the shell tool spawns one:
    /// through `Sandbox::command`, so secret-looking env is scrubbed and
    /// the credential proxy's env set, and when the sandbox is active,
    /// writes confined to the workspace, temp dirs and the server's state
    /// dir. The network stays open. A confined server also gets its home,
    /// caches and temp dir inside the state dir, since `npx` and `uvx`
    /// can't start without writing somewhere. Its `env_remove` config is
    /// applied next and its `env` last, overriding any of it.
    fn build_command(&self, args: &[String]) -> Result<tokio::process::Command, McpError> {
        let program = resolve_program(&self.cfg.command);
        let std_cmd = self
            .sandbox
            .command(program, args, &self.workspace)
            .map_err(McpError::Spawn)?;
        let mut cmd = tokio::process::Command::from(std_cmd);
        if self.sandbox.is_active() {
            let tmp = self.state_dir.join("tmp");
            std::fs::create_dir_all(&tmp).map_err(McpError::Spawn)?;
            let state = &self.state_dir;
            cmd.env("HOME", state)
                .env("XDG_CACHE_HOME", state.join(".cache"))
                .env("XDG_CONFIG_HOME", state.join(".config"))
                .env("XDG_DATA_HOME", state.join(".local/share"))
                .env("XDG_STATE_HOME", state.join(".local/state"))
                .env("npm_config_cache", state.join(".npm"))
                .env("UV_CACHE_DIR", state.join(".cache/uv"))
                .env("TMPDIR", tmp);
            // macOS apps find their home (and `~/Library/Application
            // Support`) through CoreFoundation, which ignores `HOME` and
            // honours this instead.
            #[cfg(target_os = "macos")]
            cmd.env("CFFIXED_USER_HOME", state);
        }
        for name in &self.cfg.env_remove {
            match name.strip_suffix('*') {
                Some(prefix) => {
                    for (var, _) in std::env::vars_os() {
                        if var.to_string_lossy().starts_with(prefix) {
                            cmd.env_remove(var);
                        }
                    }
                }
                None => {
                    cmd.env_remove(name);
                }
            }
        }
        cmd.envs(&self.cfg.env);
        Ok(cmd)
    }

    /// Runs the `warm_up` command, if the config has one, and waits for it
    /// with no stdio attached. A failure is only logged: the tool call
    /// that follows reports what is wrong.
    async fn warm_up(&self) {
        if self.cfg.warm_up.is_empty() || !self.is_stdio() {
            return;
        }
        let run = async {
            let mut cmd = self.build_command(&self.cfg.warm_up)?;
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            cmd.status().await.map_err(McpError::Spawn)
        };
        match tokio::time::timeout(self.cfg.startup_timeout(), run).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => {
                tracing::warn!(server = %self.cfg.name, "warm-up exited with {status}")
            }
            Ok(Err(e)) => tracing::warn!(server = %self.cfg.name, "warm-up failed: {e}"),
            Err(_) => tracing::warn!(server = %self.cfg.name, "warm-up timed out"),
        }
    }

    async fn spawn_and_handshake(&self) -> Result<Connection, McpError> {
        let mut cmd = self.build_command(&self.cfg.args)?;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(McpError::Spawn)?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let server = self.cfg.name.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(server = %server, "mcp stderr: {line}");
            }
        });

        let stdin = Arc::new(AsyncMutex::new(stdin));
        let pending: PendingMap = Arc::new(StdMutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let reader_stdin = stdin.clone();
        let reader_pending = pending.clone();
        let reader_alive = alive.clone();
        let reader_changed = self.list_changed.clone();
        let server = self.cfg.name.clone();
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let value: Value = match serde_json::from_str(trimmed) {
                            Ok(v) => v,
                            // A log line on stdout, not JSON-RPC — skip it
                            // rather than choking the connection.
                            Err(_) => {
                                tracing::debug!(server = %server, "mcp non-json stdout line: {trimmed}");
                                continue;
                            }
                        };
                        if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
                            // Server-initiated. Must never be routed into
                            // `pending`: its id is in the server's id space
                            // and can collide with one of ours.
                            match value.get("id").cloned() {
                                Some(id) => {
                                    // We advertise no client capabilities, so
                                    // only `ping` is legitimate; reject the
                                    // rest so the server never hangs waiting.
                                    let reply = if method == "ping" {
                                        json!({"jsonrpc": "2.0", "id": id, "result": {}})
                                    } else {
                                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not supported by client"}})
                                    };
                                    // Written from a separate task so a full
                                    // stdin pipe can't stall stdout draining.
                                    let w = reader_stdin.clone();
                                    tokio::spawn(async move {
                                        let line = format!("{reply}\n");
                                        let mut w = w.lock().await;
                                        let _ = w.write_all(line.as_bytes()).await;
                                        let _ = w.flush().await;
                                    });
                                }
                                None if method == "notifications/tools/list_changed" => {
                                    tracing::info!(server = %server, "mcp tool list changed");
                                    reader_changed.send_modify(|n| *n += 1);
                                }
                                None => {
                                    tracing::debug!(server = %server, "mcp notification: {value}")
                                }
                            }
                            continue;
                        }
                        let Some(id) = value.get("id").and_then(|i| i.as_u64()) else {
                            tracing::debug!(server = %server, "mcp response without numeric id: {value}");
                            continue;
                        };
                        if let Some(tx) = reader_pending.lock().unwrap().remove(&id) {
                            let _ = tx.send(value);
                        }
                    }
                    Ok(None) => break, // EOF: server exited or closed stdout.
                    Err(e) => {
                        tracing::warn!(server = %server, "mcp stdout read error: {e}");
                        break;
                    }
                }
            }
            reader_alive.store(false, Ordering::SeqCst);
            // Wake any still-pending waiters by dropping their senders —
            // each becomes a `ConnectionClosed` on the caller side.
            reader_pending.lock().unwrap().clear();
        });

        let conn = Connection {
            handle: Handle {
                stdin,
                pending,
                alive,
            },
            _child: child,
            _reader: reader_task,
            _stderr: stderr_task,
        };

        let init_params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "ferrule", "version": env!("CARGO_PKG_VERSION") },
        });
        let resp = Self::send_and_wait(
            &conn.handle,
            &self.next_id,
            "initialize",
            init_params,
            self.cfg.startup_timeout(),
        )
        .await;
        match resp {
            Ok(v) => {
                extract_result(v).map_err(|e| McpError::Handshake(e.to_string()))?;
            }
            Err(e) => return Err(McpError::Handshake(e.to_string())),
        }
        Self::send_notification(&conn.handle, "notifications/initialized").await?;
        Ok(conn)
    }

    /// `tools/list`, following `nextCursor` until the server stops sending one.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, McpError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self
                .request("tools/list", params, self.cfg.startup_timeout())
                .await?;
            let tools = result
                .get("tools")
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default();
            for t in tools {
                all.push(serde_json::from_value(t)?);
            }
            cursor = result
                .get("nextCursor")
                .and_then(|c| c.as_str())
                .map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        Ok(all)
    }

    /// `tools/call` for one remote tool. `timeout` is per-call so the
    /// wrapping `Tool` can honor a server- or tool-specific override.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        self.warm_up().await;
        let params = json!({ "name": name, "arguments": arguments });
        let result = self.request("tools/call", params, timeout).await?;
        Ok(serde_json::from_value(result)?)
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_schema", rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default)]
    pub annotations: ToolAnnotations,
}

/// The hints a server may give about a tool. Only a hint: nothing enforces
/// them, so they steer bookkeeping, never a security decision.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ToolAnnotations {
    #[serde(default, rename = "readOnlyHint")]
    pub read_only: bool,
}

fn default_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<ContentPart>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    /// base64 payload of an `image` or `audio` part.
    #[serde(default)]
    pub data: Option<String>,
    /// An embedded `resource` part's contents.
    #[serde(default)]
    pub resource: Option<ResourceContents>,
}

/// The `resource` object of an embedded-resource part: text, or a base64
/// blob, plus what it is and where it came from.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ResourceContents {
    #[serde(default)]
    pub uri: String,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>,
}

/// Where [`CallToolResult::text_saving_media`] writes non-text parts:
/// `.ferrule/mcp-content/` under the workspace — inside the room, so
/// `read_file` and `shell` can open what was saved. Files stay until the
/// owner cleans the dir; nothing prunes them yet.
pub const MEDIA_DIR: &str = ".ferrule/mcp-content";
/// The most one saved part may hold after decoding: 16 MiB. A screenshot
/// is a few hundred KB; this refuses a server that streams garbage.
pub const MAX_PART_BYTES: usize = 16 * 1024 * 1024;
/// The most one tool result may save across all its parts: 64 MiB.
pub const MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;

/// A non-text part written to disk: where it landed (absolute), its MIME
/// type, and its decoded size.
#[derive(Debug, Clone)]
pub struct SavedContent {
    pub path: PathBuf,
    pub mime_type: String,
    pub size_bytes: usize,
}

impl CallToolResult {
    /// Every `text` content part, joined. Any other part (an image, audio,
    /// a resource) becomes a line saying it was left out, rather than
    /// vanishing. Used for error results; successful calls go through
    /// [`text_saving_media`](Self::text_saving_media), which saves the
    /// parts that carry bytes.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .map(|c| match &c.text {
                Some(text) if c.kind == "text" || c.kind.is_empty() => text.clone(),
                _ => {
                    let what = c.mime_type.as_deref().unwrap_or(&c.kind);
                    format!(
                        "[{what} content left out: tools can only return text to the model so far]"
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Like [`text`](Self::text), but a part that carries bytes — an
    /// `image`/`audio` `data` payload, a `resource` blob — is decoded and
    /// written under `<workspace>/.ferrule/mcp-content/`, and its stand-in
    /// line names the file, MIME type and size so the model can open the
    /// file. A resource's *text* is text, so it is inlined like any other.
    /// Text parts come out byte-for-byte as in [`text`](Self::text); a
    /// part without bytes, over the caps, or that fails to decode or write
    /// keeps a "left out" line saying why. Saving is best-effort: a write
    /// failure never fails the tool call.
    pub fn text_saving_media(&self, workspace: &Path) -> (String, Vec<SavedContent>) {
        self.text_saving_media_within(workspace, MAX_PART_BYTES, MAX_RESULT_BYTES)
    }

    /// `text_saving_media` with the caps as arguments, so tests can use
    /// small ones.
    fn text_saving_media_within(
        &self,
        workspace: &Path,
        max_part: usize,
        max_result: usize,
    ) -> (String, Vec<SavedContent>) {
        let mut saved = Vec::new();
        let mut saved_bytes = 0usize;
        let lines = self
            .content
            .iter()
            .map(|c| {
                c.line(
                    workspace,
                    max_part,
                    max_result,
                    &mut saved,
                    &mut saved_bytes,
                )
            })
            .collect::<Vec<_>>();
        (lines.join("\n"), saved)
    }
}

impl ContentPart {
    /// The part's line for the model, saving its bytes under the workspace
    /// when it carries any (see [`CallToolResult::text_saving_media`]).
    fn line(
        &self,
        workspace: &Path,
        max_part: usize,
        max_result: usize,
        saved: &mut Vec<SavedContent>,
        saved_bytes: &mut usize,
    ) -> String {
        if let Some(text) = &self.text {
            if self.kind == "text" || self.kind.is_empty() {
                return text.clone();
            }
        }
        // A resource's text is text too: inline it rather than naming it.
        if let Some(text) = self.resource.as_ref().and_then(|r| r.text.as_ref()) {
            return text.clone();
        }
        let Some((bytes64, mime)) = self.payload() else {
            let what = self
                .mime_type
                .as_deref()
                .or(self.resource.as_ref().and_then(|r| r.mime_type.as_deref()))
                .unwrap_or(&self.kind);
            return format!("[{what} content left out: it has no data to save]");
        };
        match save_media(
            workspace,
            &self.kind,
            mime,
            bytes64,
            *saved_bytes,
            max_part,
            max_result,
        ) {
            Ok(one) => {
                *saved_bytes += one.size_bytes;
                let rel = one
                    .path
                    .strip_prefix(workspace)
                    .unwrap_or(&one.path)
                    .to_path_buf();
                let line = format!(
                    "[{mime} saved to {} ({} bytes)]",
                    rel.display(),
                    one.size_bytes
                );
                saved.push(one);
                line
            }
            Err(why) => format!("[{mime} content left out: {why}]"),
        }
    }

    /// The part's bytes and MIME type, if it carries any: an
    /// `image`/`audio` `data` payload, or a `resource`'s blob. An empty
    /// payload is none.
    fn payload(&self) -> Option<(&str, &str)> {
        if let Some(data) = self.data.as_deref().filter(|d| !d.is_empty()) {
            return Some((
                data,
                self.mime_type
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
            ));
        }
        let resource = self.resource.as_ref()?;
        let blob = resource.blob.as_deref().filter(|b| !b.is_empty())?;
        let mime = resource
            .mime_type
            .as_deref()
            .or(self.mime_type.as_deref())
            .unwrap_or("application/octet-stream");
        Some((blob, mime))
    }
}

/// Decode `bytes64` (base64) and write it under
/// `<workspace>/.ferrule/mcp-content/`, within the per-part cap and the
/// per-result cap (`already` bytes saved so far). The filename is ours —
/// kind, clock and a counter — never anything the server sent.
fn save_media(
    workspace: &Path,
    kind: &str,
    mime: &str,
    bytes64: &str,
    already: usize,
    max_part: usize,
    max_result: usize,
) -> Result<SavedContent, String> {
    use base64::Engine;
    // ~3/4 of the base64 length: refuse a monster before decoding it. The
    // estimate can exceed the real size by the padding, so a part exactly
    // at the cap may be refused here — a cap is a guardrail, not a quota.
    let estimate = bytes64.len() / 4 * 3;
    if estimate > max_part {
        return Err(format!(
            "{} is over the {} per-file limit",
            human_bytes(estimate),
            human_bytes(max_part)
        ));
    }
    if already + estimate > max_result {
        return Err(format!(
            "the {} per-result limit is reached",
            human_bytes(max_result)
        ));
    }
    let clean: String = bytes64
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(clean)
        .map_err(|_| "the base64 payload would not decode".to_string())?;
    if bytes.len() > max_part {
        return Err(format!(
            "{} is over the {} per-file limit",
            human_bytes(bytes.len()),
            human_bytes(max_part)
        ));
    }
    let dir = workspace.join(MEDIA_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let path = dir.join(media_file_name(kind, mime));
    std::fs::write(&path, &bytes)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(SavedContent {
        path,
        mime_type: mime.to_string(),
        size_bytes: bytes.len(),
    })
}

static MEDIA_SEQ: AtomicU64 = AtomicU64::new(0);

/// `image-1759056123456-7.png`: the part's kind, the clock and a
/// process-wide counter, so two calls — or two ferrule processes sharing a
/// workspace — never pick the same name.
fn media_file_name(kind: &str, mime: &str) -> String {
    let kind = sanitize(kind);
    let kind = if kind.is_empty() { "part".into() } else { kind };
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = MEDIA_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{kind}-{millis}-{seq}.{}", extension_for(mime))
}

/// Lowercase ASCII alphanumerics only, length-capped.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .take(24)
        .collect()
}

/// The file extension for a MIME type. Common types map by hand; anything
/// else falls back to its sanitized subtype (`image/x-icon` → `xicon`,
/// `application/xhtml+xml` → `xhtml`), or `bin`.
fn extension_for(mime: &str) -> String {
    let mime = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    let known = match mime.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/x-icon" => "ico",
        "audio/mpeg" => "mp3",
        "audio/wav" => "wav",
        "audio/ogg" => "ogg",
        "audio/webm" => "webm",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "application/pdf" => "pdf",
        "application/json" => "json",
        "text/html" => "html",
        "text/css" => "css",
        "text/csv" => "csv",
        "text/markdown" => "md",
        "text/plain" => "txt",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    let subtype = mime.rsplit('/').next().unwrap_or("");
    let subtype = subtype.split('+').next().map(sanitize).unwrap_or_default();
    if subtype.is_empty() {
        "bin".into()
    } else {
        subtype
    }
}

fn human_bytes(n: usize) -> String {
    const MIB: usize = 1 << 20;
    const KIB: usize = 1 << 10;
    if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} bytes")
    }
}

/// Windows only runs `npx` if told it's `npx.cmd`: look a bare name up in
/// `PATH` with each `PATHEXT` extension, as `cmd.exe` would. Elsewhere, and
/// for anything with a directory or an extension, the name is used as is.
fn resolve_program(command: &str) -> PathBuf {
    if cfg!(windows) {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        if let Some(found) =
            std::env::var_os("PATH").and_then(|path| find_in_path(command, &path, &exts))
        {
            return found;
        }
    }
    PathBuf::from(command)
}

fn find_in_path(command: &str, path: &std::ffi::OsStr, exts: &str) -> Option<PathBuf> {
    let name = Path::new(command);
    if name.extension().is_some() || name.components().count() != 1 {
        return None;
    }
    std::env::split_paths(path).find_map(|dir| {
        exts.split(';')
            .filter(|e| !e.is_empty())
            .map(|ext| dir.join(format!("{command}{}", ext.to_ascii_lowercase())))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_the_model_cant_see_is_named_not_dropped() {
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [
                { "type": "text", "text": "Screenshot taken" },
                { "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" },
                { "type": "audio", "data": "" }
            ]
        }))
        .unwrap();
        let text = result.text();
        assert!(text.starts_with("Screenshot taken\n"), "{text}");
        assert!(text.contains("[image/png content left out"), "{text}");
        assert!(text.contains("[audio content left out"), "{text}");
        assert!(!text.contains("iVBOR"), "no base64 in the context");
    }

    #[test]
    fn a_bare_name_is_found_with_a_pathext_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("npx.cmd"), "").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(
            find_in_path("npx", &path, ".EXE;.CMD"),
            Some(dir.path().join("npx.cmd"))
        );
        assert_eq!(
            find_in_path("npx.cmd", &path, ".EXE;.CMD"),
            None,
            "has an extension"
        );
        assert_eq!(find_in_path("uvx", &path, ".EXE;.CMD"), None, "not there");
    }
}
