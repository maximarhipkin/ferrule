//! The bridge between a turn's `claude` and ferrule (design §7.4).
//!
//! Each turn listens on `127.0.0.1:0` with a random token. claude starts
//! the stdio MCP server named in `--mcp-config` (the hidden `ferrule
//! claude-mcp`), which connects here, sends the token as its first line,
//! and then relays newline-delimited JSON-RPC both ways ([`relay`]). A
//! connection whose first line isn't the token is closed unread.
//!
//! The server answers `initialize`, `tools/list` and `ping` itself and
//! hands every `tools/call` to the engine as a [`BridgeCall`]: the
//! permission-prompt tool (`approve`) and the ferrule tools the agent
//! already has. Stdio rather than an HTTP MCP server because claude's HTTP
//! clients honour the proxy variables, and the egress proxy refuses a
//! loopback URL for tools.

use ferrule_core::tool::ToolDefinition;
use serde_json::{json, Value};
use std::io;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// The relay's environment: where the bridge listens, and its token.
pub const PORT_VAR: &str = "FERRULE_BRIDGE_PORT";
pub const TOKEN_VAR: &str = "FERRULE_BRIDGE_TOKEN";
/// The permission-prompt tool's name on the bridge.
pub const APPROVE: &str = "approve";

/// One `tools/call` from claude, for the engine to answer.
#[derive(Debug)]
pub struct BridgeCall {
    /// The tool's name on the bridge (`approve`, `remember`, …).
    pub name: String,
    pub args: Value,
    pub reply: oneshot::Sender<Reply>,
}

/// A tool's answer: its text, and whether it failed.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub text: String,
    pub is_error: bool,
}

impl Reply {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// A running bridge. Dropping it closes the listener and every connection.
pub struct Bridge {
    port: u16,
    token: String,
    task: JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge").field("port", &self.port).finish()
    }
}

/// The `approve` tool's schema: what claude's permission prompt sends.
fn approve_tool() -> Value {
    json!({
        "name": APPROVE,
        "description": "ferrule's permission prompt. Not for the model to call.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tool_name": {"type": "string"},
                "input": {"type": "object"},
                "tool_use_id": {"type": "string"}
            },
            "required": ["tool_name", "input"]
        }
    })
}

impl Bridge {
    /// Listen for this turn: `tools` are the ferrule tools offered (the
    /// permission tool is always there), and each call goes to `calls`.
    pub async fn start(
        tools: Vec<ToolDefinition>,
        calls: mpsc::Sender<BridgeCall>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let token = ferrule_connections::seal::b64(&ferrule_connections::seal::random::<24>());
        let mut listed = vec![approve_tool()];
        listed.extend(tools.iter().map(
            |t| json!({"name": t.name, "description": t.description, "inputSchema": t.parameters}),
        ));
        let listed = Value::Array(listed);
        let expected = token.clone();
        let task = tokio::spawn(async move {
            let mut conns = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        conns.spawn(serve(stream, expected.clone(), listed.clone(), calls.clone()));
                    }
                    Some(_) = conns.join_next(), if !conns.is_empty() => {}
                }
            }
        });
        Ok(Self { port, token, task })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The `--mcp-config` document: one stdio server, `command args…`, with
    /// the port and token in its environment.
    pub fn mcp_config(&self, command: &Path, args: &[String]) -> Value {
        json!({
            "mcpServers": {
                super::env::BRIDGE_SERVER: {
                    "type": "stdio",
                    "command": command.display().to_string(),
                    "args": args,
                    "env": {
                        PORT_VAR: self.port.to_string(),
                        TOKEN_VAR: self.token,
                    }
                }
            }
        })
    }
}

/// Constant-time enough for a per-turn random token on loopback.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

async fn serve(stream: TcpStream, token: String, tools: Value, calls: mpsc::Sender<BridgeCall>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line()).await;
    match first {
        Ok(Ok(Some(line))) if same(line.trim(), &token) => {}
        _ => return,
    }
    // Answers go out through one writer, so parallel calls don't interleave.
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(64);
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let mut line = msg.to_string();
            line.push('\n');
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or("");
        let Some(id) = id.filter(|_| !method.is_empty()) else {
            continue; // a notification or a response: nothing to answer
        };
        let result = match method {
            "initialize" => json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "ferrule", "version": env!("CARGO_PKG_VERSION")}
            }),
            "tools/list" => json!({"tools": tools}),
            "ping" => json!({}),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("").to_string();
                let args = msg["params"]["arguments"].clone();
                let (reply_tx, reply_rx) = oneshot::channel();
                let calls = calls.clone();
                let out = out_tx.clone();
                // Answered when the engine has an answer; others keep flowing.
                tokio::spawn(async move {
                    let call = BridgeCall {
                        name,
                        args,
                        reply: reply_tx,
                    };
                    let reply = match calls.send(call).await {
                        Ok(()) => reply_rx
                            .await
                            .unwrap_or_else(|_| Reply::error("the turn ended")),
                        Err(_) => Reply::error("the turn ended"),
                    };
                    let _ = out
                        .send(json!({"jsonrpc": "2.0", "id": id, "result": {
                            "content": [{"type": "text", "text": reply.text}],
                            "isError": reply.is_error,
                        }}))
                        .await;
                });
                continue;
            }
            other => {
                let _ = out_tx
                    .send(json!({"jsonrpc": "2.0", "id": id, "error": {
                        "code": -32601, "message": format!("method not found: {other}")
                    }}))
                    .await;
                continue;
            }
        };
        let _ = out_tx
            .send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }
    drop(out_tx);
    let _ = writer.await;
}

/// The hidden `ferrule claude-mcp`: connect to the bridge named in the
/// environment and relay `input` to it and its answers to `output`, until
/// either side closes.
pub async fn relay_from_env(
    input: impl AsyncRead + Unpin,
    output: impl AsyncWrite + Unpin,
) -> io::Result<()> {
    let port: u16 = std::env::var(PORT_VAR)
        .ok()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| {
            io::Error::other(format!(
                "{PORT_VAR} isn't set: this runs inside a claude-code turn only"
            ))
        })?;
    let token =
        std::env::var(TOKEN_VAR).map_err(|_| io::Error::other(format!("{TOKEN_VAR} isn't set")))?;
    relay(port, &token, input, output).await
}

pub async fn relay(
    port: u16,
    token: &str,
    mut input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> io::Result<()> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let (mut from_bridge, mut to_bridge) = stream.into_split();
    to_bridge.write_all(format!("{token}\n").as_bytes()).await?;
    let up = async {
        tokio::io::copy(&mut input, &mut to_bridge).await?;
        to_bridge.shutdown().await
    };
    let down = async {
        tokio::io::copy(&mut from_bridge, &mut output).await?;
        output.flush().await
    };
    // Either side closing ends the relay: claude closes stdin at exit, the
    // bridge closes at turn end.
    tokio::select! {
        r = up => r,
        r = down => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn line(r: &mut (impl AsyncBufReadExt + Unpin)) -> Value {
        let mut s = String::new();
        r.read_line(&mut s).await.unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[tokio::test]
    async fn the_relay_reaches_the_bridge_with_its_token_and_a_stranger_does_not() {
        let (tx, mut rx) = mpsc::channel(4);
        let tools = vec![ToolDefinition {
            name: "remember".into(),
            description: "keep a note".into(),
            parameters: json!({"type": "object"}),
        }];
        let bridge = Bridge::start(tools, tx).await.unwrap();
        let cfg = bridge.mcp_config(Path::new("/bin/ferrule"), &["claude-mcp".into()]);
        let server = &cfg["mcpServers"]["ferrule"];
        assert_eq!(server["args"][0], "claude-mcp");
        let token = server["env"][TOKEN_VAR].as_str().unwrap().to_string();
        assert_eq!(server["env"][PORT_VAR], bridge.port().to_string());

        // A stranger without the token is closed without an answer.
        let mut s = TcpStream::connect(("127.0.0.1", bridge.port()))
            .await
            .unwrap();
        s.write_all(b"wrong\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();
        let mut buf = String::new();
        let n = BufReader::new(&mut s).read_line(&mut buf).await.unwrap();
        assert_eq!(n, 0, "closed unanswered: {buf}");

        // The relay, with pipes standing in for claude's stdio.
        let (mut claude_out, relay_in) = tokio::io::duplex(4096);
        let (relay_out, claude_in) = tokio::io::duplex(4096);
        let port = bridge.port();
        let relay = tokio::spawn(async move { relay(port, &token, relay_in, relay_out).await });
        let mut claude_in = BufReader::new(claude_in);
        claude_out
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();
        let init = line(&mut claude_in).await;
        assert_eq!(init["result"]["serverInfo"]["name"], "ferrule");
        let list = line(&mut claude_in).await;
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["approve", "remember"]);

        claude_out
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"remember\",\"arguments\":{\"note\":\"x\"}}}\n")
            .await
            .unwrap();
        let call = rx.recv().await.unwrap();
        assert_eq!(call.name, "remember");
        assert_eq!(call.args["note"], "x");
        call.reply.send(Reply::ok("kept")).unwrap();
        let answer = line(&mut claude_in).await;
        assert_eq!(answer["id"], 3);
        assert_eq!(answer["result"]["content"][0]["text"], "kept");
        assert_eq!(answer["result"]["isError"], false);

        // The bridge going away ends the relay.
        drop(bridge);
        drop(claude_out);
        tokio::time::timeout(std::time::Duration::from_secs(5), relay)
            .await
            .unwrap()
            .unwrap()
            .ok();
    }
}
