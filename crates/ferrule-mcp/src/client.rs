use crate::config::McpServerConfig;
use crate::error::McpError;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

const PROTOCOL_VERSION: &str = "2025-06-18";

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

/// A client for one stdio MCP server. Owns lazy respawn: a dead connection
/// is only reconnected on the next call, and only once — if that attempt
/// also fails, the call reports an error rather than looping.
pub struct McpClient {
    cfg: McpServerConfig,
    conn: AsyncMutex<Option<Connection>>,
    next_id: AtomicU64,
}

fn extract_result(resp: Value) -> Result<Value, McpError> {
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
    pub fn new(cfg: McpServerConfig) -> Self {
        Self {
            cfg,
            conn: AsyncMutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn name(&self) -> &str {
        &self.cfg.name
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

    async fn spawn_and_handshake(&self) -> Result<Connection, McpError> {
        let mut cmd = tokio::process::Command::new(&self.cfg.command);
        cmd.args(&self.cfg.args)
            .envs(&self.cfg.env)
            .stdin(Stdio::piped())
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
}

impl CallToolResult {
    /// Concatenate every `text` content part — the model only ever sees text.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| c.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
