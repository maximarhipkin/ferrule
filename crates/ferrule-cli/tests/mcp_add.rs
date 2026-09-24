//! The real `ferrule` binary: `ferrule mcp add` into the config a running
//! gateway was started with, and the gateway's next message has the new
//! server's tools, with no restart; the server's secret arrives as a
//! placeholder. A server that fails its smoke test, or a flag that would
//! put a key in the config, writes nothing.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A stdio MCP server with one tool, `echo`, that also says what its
/// `DEMO_TOKEN` is.
const SERVER: &str = r#"
import json, os, sys
for line in sys.stdin:
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "e", "version": "0"}}
    elif method == "tools/list":
        r = {"tools": [{"name": "echo", "description": "Echo the text back.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}
    elif mid is None:
        continue
    elif method == "tools/call":
        text = msg["params"]["arguments"].get("text", "")
        token = os.environ.get("DEMO_TOKEN", "<none>")
        r = {"content": [{"type": "text", "text": "echoed:" + text + " token:" + token}]}
    else:
        r = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": r}) + "\n")
    sys.stdout.flush()
"#;

const REAL_TOKEN: &str = "real-demo-token-7f3a";

fn tool_names(req: &Value) -> Vec<String> {
    req["tools"]
        .as_array()
        .map(|t| {
            t.iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Calls `mcp__demo__echo` when it has it, and says what it got back.
fn reply(req: &Value) -> Value {
    let messages = req["messages"].as_array().unwrap();
    let last = messages.last().unwrap();
    if last["role"] == "tool" {
        let got = last["content"].as_str().unwrap_or_default();
        return answer(&format!("TOOL_SAID {got}"));
    }
    if !tool_names(req).iter().any(|n| n == "mcp__demo__echo") {
        return answer("NO_ECHO_TOOL");
    }
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "mcp__demo__echo",
                        "arguments": json!({"text": "hi"}).to_string(),
                    },
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    })
}

fn answer(text: &str) -> Value {
    json!({
        "choices": [{
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    })
}

fn model_server() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let log = log.clone();
            std::thread::spawn(move || serve(stream, &log));
        }
    });
    (url, seen)
}

fn serve(mut stream: TcpStream, log: &Mutex<Vec<Value>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let req: Value = serde_json::from_slice(&body).unwrap();
    let out = reply(&req).to_string();
    log.lock().unwrap().push(req);
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
        out.len()
    );
}

fn command(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args(args)
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_KEY", "sk-test")
        .env_remove("DEMO_TOKEN")
        .stdin(Stdio::null());
    for var in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        cmd.env(var, home.join("home"));
    }
    for var in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn ferrule(home: &Path, args: &[&str]) -> Output {
    command(home, args).output().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const CONFIG: &str = r#"# the owner's own comment
default_provider = "mock"

[providers.mock]
base_url = "URL"
api_key_env = "FERRULE_TEST_KEY"
model = "scripted"

[gateway]
local = true

[skills]
enabled = false

[sandbox]
mode = "off"
"#;

fn setup(url: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    for d in ["work", "data", "home"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    std::fs::write(home.join("ferrule.toml"), CONFIG.replace("URL", url)).unwrap();
    std::fs::write(home.join("server.py"), SERVER).unwrap();
    (dir, home)
}

#[test]
fn a_server_added_while_the_gateway_runs_serves_its_next_message() {
    let (url, seen) = model_server();
    let (_dir, home) = setup(&url);

    let mut gateway = command(&home, &["gateway"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = gateway.stdin.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let stdout = gateway.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let next_reply = |want: &str| -> String {
        let mut got = Vec::new();
        while let Ok(line) = rx.recv_timeout(Duration::from_secs(60)) {
            if line.contains(want) {
                return line;
            }
            got.push(line);
        }
        panic!("no reply with {want}; got {got:?}");
    };

    writeln!(stdin, "first message").unwrap();
    next_reply("NO_ECHO_TOOL");

    let script = home.join("server.py");
    let out = command(
        &home,
        &[
            "mcp",
            "add",
            "demo",
            "--yes",
            "--secret",
            "DEMO_TOKEN=api.example.com",
            "--",
            "python3",
            script.to_str().unwrap(),
        ],
    )
    .env("DEMO_TOKEN", REAL_TOKEN)
    .output()
    .unwrap();
    let log = text(&out);
    assert!(out.status.success(), "{log}");
    assert!(log.contains("`demo` offers 1 tool(s)"), "{log}");
    assert!(log.contains("echo"), "{log}");
    assert!(log.contains("no restart"), "{log}");

    // The owner's comment stays; the key is in the secrets file, not the config.
    let config = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(
        config.starts_with("# the owner's own comment\n"),
        "{config}"
    );
    assert!(config.contains("name = \"demo\""), "{config}");
    assert!(
        config.contains("DEMO_TOKEN = [\"api.example.com\"]"),
        "{config}"
    );
    assert!(!config.contains(REAL_TOKEN), "{config}");
    let saved = std::fs::read_to_string(home.join("data/private/secrets.env")).unwrap();
    assert!(saved.contains(REAL_TOKEN), "{saved}");

    // The follower polls every 2 s.
    std::thread::sleep(Duration::from_secs(5));
    writeln!(stdin, "second message").unwrap();
    let said = next_reply("TOOL_SAID");
    assert!(said.contains("echoed:hi"), "{said}");
    assert!(!said.contains("token:<none>"), "a placeholder: {said}");
    assert!(!said.contains(REAL_TOKEN), "never the key itself: {said}");

    drop(stdin);
    let _ = gateway.kill();
    let _ = gateway.wait();

    let seen = seen.lock().unwrap();
    assert!(!tool_names(&seen[0]).iter().any(|n| n.starts_with("mcp__")));
    assert!(seen
        .iter()
        .any(|r| tool_names(r).contains(&"mcp__demo__echo".to_string())));

    // `mcp list` shows it; `mcp remove` takes it out again.
    let out = ferrule(&home, &["mcp", "list"]);
    assert!(
        text(&out).contains("server demo [configured] python3"),
        "{}",
        text(&out)
    );
    let out = ferrule(&home, &["mcp", "remove", "demo"]);
    let log = text(&out);
    assert!(out.status.success(), "{log}");
    let config = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(!config.contains("name = \"demo\""), "{config}");
    assert!(config.contains("DEMO_TOKEN"), "[secrets] stay: {config}");
    let out = ferrule(&home, &["mcp", "remove", "demo"]);
    assert!(!out.status.success());
}

#[test]
fn a_failed_smoke_test_or_a_key_in_a_flag_writes_nothing() {
    let (url, _) = model_server();
    let (_dir, home) = setup(&url);
    let before = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    let unchanged = |log: &str| {
        let now = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
        assert_eq!(now, before, "{log}");
        assert!(!home.join("data/private/secrets.env").exists(), "{log}");
        assert!(!home.join("data/mcp/broken").exists(), "{log}");
    };

    // The server exits before it answers `initialize`.
    let out = command(
        &home,
        &[
            "mcp",
            "add",
            "broken",
            "--yes",
            "--secret",
            "DEMO_TOKEN=api.example.com",
            "--",
            "python3",
            "-c",
            "import sys; sys.exit(3)",
        ],
    )
    .env("DEMO_TOKEN", REAL_TOKEN)
    .output()
    .unwrap();
    let log = text(&out);
    assert!(!out.status.success(), "{log}");
    assert!(log.contains("nothing was written"), "{log}");
    unchanged(&log);

    // A key where the config would keep it.
    let script = home.join("server.py");
    let out = ferrule(
        &home,
        &[
            "mcp",
            "add",
            "broken",
            "--yes",
            "--env",
            "GITHUB_TOKEN=ghp_abc",
            "--",
            "python3",
            script.to_str().unwrap(),
        ],
    );
    let log = text(&out);
    assert!(!out.status.success(), "{log}");
    assert!(log.contains("--secret GITHUB_TOKEN"), "{log}");
    unchanged(&log);

    // Only tools the server doesn't have.
    let out = ferrule(
        &home,
        &[
            "mcp",
            "add",
            "broken",
            "--yes",
            "--enabled-tool",
            "nope",
            "--",
            "python3",
            script.to_str().unwrap(),
        ],
    );
    let log = text(&out);
    assert!(!out.status.success(), "{log}");
    assert!(log.contains("it offers: echo"), "{log}");
    unchanged(&log);
}
