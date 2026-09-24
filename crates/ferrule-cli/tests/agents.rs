//! The real `ferrule` binary with sub-agents, against a scripted
//! OpenAI-compatible server: `ferrule run` starts a child, ends its first
//! run, is run again when the child reports, and closes the tree before it
//! exits; the ledger and `ferrule agents list` show the child.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the scripted model says, from what it's asked.
fn reply(req: &Value) -> Value {
    let messages = req["messages"].as_array().unwrap();
    let text = |m: &Value| m["content"].as_str().unwrap_or_default().to_string();
    let system = messages.first().map(text).unwrap_or_default();
    let last = messages.last().unwrap();
    if system.contains("## You are agent") {
        // Slow enough that the root's first run has ended.
        std::thread::sleep(Duration::from_millis(1500));
        return answer("CHILD_REPORT the answer is 42");
    }
    if last["role"] == "user" && text(last).contains("agent_notice") {
        return answer("ROOT_DONE the child says 42");
    }
    if last["role"] == "tool" {
        return answer("ROOT_WAITING for the child");
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
                        "name": "spawn_agent",
                        "arguments": json!({"task": "find the answer", "worktree": false}).to_string(),
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

/// Serves `reply` on a thread per connection; returns the base URL and
/// every request body it saw.
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

/// Output without its colours.
fn plain(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn ferrule(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args(args)
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_KEY", "sk-test");
    // Nothing from the machine running the tests: its config, skills or proxy.
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
    cmd.output().unwrap()
}

#[test]
fn a_run_is_woken_by_its_childs_report_and_closes_the_tree_before_exiting() {
    let (url, seen) = model_server();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    for d in ["work", "data", "home"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    std::fs::write(
        home.join("ferrule.toml"),
        format!(
            r#"default_provider = "mock"

[providers.mock]
base_url = "{url}"
api_key_env = "FERRULE_TEST_KEY"
model = "scripted"

[skills]
enabled = false

[sandbox]
mode = "off"
"#
        ),
    )
    .unwrap();

    let out = ferrule(home, &["run", "ask a child for the answer"]);
    let stdout = plain(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("ROOT_WAITING"), "{stdout}");
    assert!(
        stdout.contains("the agents it started reported back"),
        "{stdout}"
    );
    assert!(
        stdout.contains("final: ROOT_DONE the child says 42"),
        "{stdout}"
    );
    assert!(stdout.contains("[sub-agents: 1 closed,"), "{stdout}");

    // The child was told it's one, and its report reached the root fenced.
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.iter()
            .filter(|r| r["messages"][0]["content"]
                .as_str()
                .is_some_and(|s| s.contains("## You are agent")))
            .count(),
        1
    );
    let woken = seen.last().unwrap().to_string();
    assert!(woken.contains("agent_notice"), "{woken}");

    // Its model calls are in the ledger, tagged with its parent.
    let ledger = std::fs::read_to_string(home.join("data/ledger.jsonl")).unwrap();
    let rows: Vec<Value> = ledger
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let child: Vec<&Value> = rows.iter().filter(|r| r["task_shape"] == "agent").collect();
    assert_eq!(child.len(), 1, "{ledger}");
    let origin = child[0]["origin"].as_str().unwrap();
    let root = origin.strip_prefix("agent:").unwrap();
    assert_eq!(
        rows.iter().filter(|r| r["task_shape"] == "run").count(),
        3,
        "{ledger}"
    );

    // Closed with the run: only `--all` shows the tree.
    let out = ferrule(home, &["agents", "list"]);
    let list = plain(&out.stdout);
    assert!(list.contains("No agents open"), "{list}");
    let out = ferrule(home, &["agents", "list", "--all"]);
    let list = plain(&out.stdout);
    assert!(list.starts_with(&format!("{root} (root)\n")), "{list}");
    assert!(list.contains("[worker] closed, 110 tokens"), "{list}");

    let out = ferrule(home, &["agents", "close", "nope"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("there is no agent nope"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
