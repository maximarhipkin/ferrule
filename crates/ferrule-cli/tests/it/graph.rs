//! M42 part 6 graph runs, through the real `ferrule` binary against a
//! scripted OpenAI-compatible server: a linear implement→check run, a
//! rollback on a failed check, a verifier agent's verdict, fan-out/fan-in
//! with a join, and approval nodes approved and denied. Unix only: the
//! check nodes are `sh` commands.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

type Script = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

fn text(m: &Value) -> String {
    m["content"].as_str().unwrap_or_default().to_string()
}

fn last(req: &Value) -> Value {
    req["messages"].as_array().unwrap().last().unwrap().clone()
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

fn call(name: &str, args: Value) -> Value {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": name, "arguments": args.to_string()},
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    })
}

/// Every child's task says TASK:<what>; the script does that, then reports
/// done. A task whose `{{prev}}` carries a failed check's word
/// ("verify (fail)") writes fixed.txt — the repair path. `TASK:verdict-*`
/// answers the verdict marker directly.
fn model_server() -> (String, Arc<Mutex<Vec<Value>>>) {
    let script: Script = Arc::new(|req: &Value| {
        let last = last(req);
        let said = text(&last);
        if last["role"] == "tool" {
            return answer("done");
        }
        if said.contains("verify (fail)") {
            return call("write_file", json!({"path": "fixed.txt", "content": "ok"}));
        }
        for (marker, file) in [
            ("TASK:w1", "w1.txt"),
            ("TASK:w2", "w2.txt"),
            ("TASK:draft", "draft.txt"),
            ("TASK:impl", "impl.txt"),
        ] {
            if said.contains(marker) {
                return call("write_file", json!({"path": file, "content": "ok"}));
            }
        }
        if said.contains("TASK:verdict-pass") {
            return answer("everything checks out\nVERDICT: PASS");
        }
        if said.contains("TASK:verdict-mumble") {
            return answer("I'm not really sure about this");
        }
        answer("done")
    });
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
            let script = script.clone();
            std::thread::spawn(move || serve(stream, &log, &script));
        }
    });
    (url, seen)
}

fn serve(mut stream: TcpStream, log: &Mutex<Vec<Value>>, script: &Script) {
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
    let out = script(&req).to_string();
    log.lock().unwrap().push(req);
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
        out.len()
    );
}

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

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn path(&self) -> &Path {
        self.dir.path()
    }
}

fn home(url: &str) -> Home {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    for d in ["work", "data", "home", "tmp"] {
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
    Home { dir }
}

fn ferrule(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args(args)
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_KEY", "sk-test")
        .env("TMPDIR", home.join("tmp"))
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
    cmd.output().unwrap()
}

fn graph(home: &Path, name: &str, text: &str) -> std::path::PathBuf {
    let file = home.join(name);
    std::fs::write(&file, text).unwrap();
    file
}

#[test]
fn a_linear_run_implement_then_check() {
    let (url, _) = model_server();
    let home = home(&url);
    let file = graph(
        home.path(),
        "g.toml",
        r#"
goal = "ship impl"

[[nodes]]
id = "implement"
task = "TASK:impl"

[[nodes]]
id = "verify"
kind = "check"
command = "test -f impl.txt"

[[edges]]
from = "implement"
to = "verify"
"#,
    );
    let out = ferrule(home.path(), &["graph", "run", file.to_str().unwrap()]);
    let stdout = plain(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(stdout.contains("✓ implement"), "{stdout}");
    assert!(stdout.contains("✓ verify"), "{stdout}");
    assert!(stdout.contains("graph: goal met"), "{stdout}");
}

#[test]
fn a_failed_check_rolls_back_until_the_attempt_cap() {
    let (url, _) = model_server();
    let home = home(&url);
    let file = graph(
        home.path(),
        "g.toml",
        r#"
goal = "write a file the model never writes"

[[nodes]]
id = "implement"
task = "TASK:draft"

[[nodes]]
id = "verify"
kind = "check"
command = "test -f impossible.txt"

[[edges]]
from = "implement"
to = "verify"

[[edges]]
from = "verify"
to = "implement"
on = "fail"
"#,
    );
    let out = ferrule(
        home.path(),
        &["graph", "run", file.to_str().unwrap(), "--max-steps", "12"],
    );
    let stdout = plain(&out.stdout);
    // The model always writes draft.txt, so every attempt fails the check:
    // the loop stops at the node's attempt cap with a truthful not-met.
    assert_eq!(out.status.code(), Some(2), "stdout:\n{stdout}");
    assert!(stdout.contains("✗ verify"), "{stdout}");
    assert!(stdout.contains("graph: not met"), "{stdout}");
    assert!(stdout.contains("implement (agent, attempt 1)"), "{stdout}");
    assert!(stdout.contains("implement (agent, attempt 3)"), "{stdout}");
    assert!(
        !stdout.contains("attempt 4"),
        "the per-node attempt cap holds:\n{stdout}"
    );
}

#[test]
fn a_rollback_with_repair_guidance_succeeds() {
    let (url, _) = model_server();
    let home = home(&url);
    // No instructions in the task itself: the check's own failure, carried
    // back by {{prev}}, is what tells the model to write fixed.txt.
    let file = graph(
        home.path(),
        "g.toml",
        r#"
goal = "fixed on the second go"

[[nodes]]
id = "implement"
task = "What came back:\n{{prev}}"

[[nodes]]
id = "verify"
kind = "check"
command = "test -f fixed.txt"

[[edges]]
from = "implement"
to = "verify"

[[edges]]
from = "verify"
to = "implement"
on = "fail"
"#,
    );
    let out = ferrule(home.path(), &["graph", "run", file.to_str().unwrap()]);
    let stdout = plain(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(
        stdout.contains("verify (check, attempt 1) fail"),
        "{stdout}"
    );
    assert!(
        stdout.contains("implement (agent, attempt 2) pass"),
        "{stdout}"
    );
    assert!(
        stdout.contains("verify (check, attempt 2) pass"),
        "{stdout}"
    );
    assert!(stdout.contains("graph: goal met"), "{stdout}");
}

#[test]
fn a_verifier_agents_verdict_routes_the_run() {
    let (url, _) = model_server();
    let home = home(&url);
    let file = graph(
        home.path(),
        "g.toml",
        r#"
goal = "verify then ship"

[[nodes]]
id = "review"
role = "verifier"
task = "TASK:verdict-pass"

[[nodes]]
id = "ship"
kind = "check"
command = "true"

[[edges]]
from = "review"
to = "ship"
on = "pass"
"#,
    );
    let out = ferrule(home.path(), &["graph", "run", file.to_str().unwrap()]);
    let stdout = plain(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(stdout.contains("✓ review"), "{stdout}");
    assert!(stdout.contains("✓ ship"), "{stdout}");

    // A verifier that doesn't say the marker fails closed.
    let file = graph(
        home.path(),
        "g2.toml",
        r#"
goal = "mumbles don't pass"

[[nodes]]
id = "review"
role = "verifier"
task = "TASK:verdict-mumble"

[[nodes]]
id = "ship"
kind = "check"
command = "true"

[[edges]]
from = "review"
to = "ship"
on = "pass"
"#,
    );
    let out = ferrule(home.path(), &["graph", "run", file.to_str().unwrap()]);
    let stdout = plain(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "stdout:\n{stdout}");
    assert!(stdout.contains("✗ review"), "{stdout}");
    assert!(stdout.contains("ship never ran"), "{stdout}");
}

#[test]
fn fan_out_and_fan_in_the_join_waits_for_every_input() {
    let (url, _) = model_server();
    let home = home(&url);
    let file = graph(
        home.path(),
        "g.toml",
        r#"
goal = "two workers, one join"

[[nodes]]
id = "start"
kind = "check"
command = "true"

[[nodes]]
id = "w1"
task = "TASK:w1"

[[nodes]]
id = "w2"
task = "TASK:w2"

[[nodes]]
id = "join"
kind = "check"
command = "test -f w1.txt -a -f w2.txt"

[[edges]]
from = "start"
to = "w1"
on = "pass"

[[edges]]
from = "start"
to = "w2"
on = "pass"

[[edges]]
from = "w1"
to = "join"

[[edges]]
from = "w2"
to = "join"
"#,
    );
    let out = ferrule(home.path(), &["graph", "run", file.to_str().unwrap()]);
    let stdout = plain(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(
        stdout.contains("✓ w1") && stdout.contains("✓ w2"),
        "{stdout}"
    );
    assert!(stdout.contains("✓ join"), "{stdout}");
    assert!(stdout.contains("graph: goal met"), "{stdout}");
}

#[test]
fn an_approval_node_parks_for_the_owner() {
    let (url, _) = model_server();
    let home = home(&url);
    let file = graph(
        home.path(),
        "g.toml",
        r#"
goal = "ship with a human gate"

[[nodes]]
id = "implement"
task = "TASK:impl"

[[nodes]]
id = "gate"
kind = "approval"
message = "impl is written. Ship?"

[[nodes]]
id = "ship"
kind = "check"
command = "true"

[[edges]]
from = "implement"
to = "gate"

[[edges]]
from = "gate"
to = "ship"
on = "pass"
"#,
    );
    // --yes approves: the ship check runs.
    let out = ferrule(
        home.path(),
        &["graph", "run", file.to_str().unwrap(), "--yes"],
    );
    let stdout = plain(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(stdout.contains("✓ gate"), "{stdout}");
    assert!(stdout.contains("✓ ship"), "{stdout}");

    // No terminal and no --yes: the gate denies, ship never runs.
    let out = ferrule(home.path(), &["graph", "run", file.to_str().unwrap()]);
    let stdout = plain(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "stdout:\n{stdout}");
    assert!(stdout.contains("✗ gate"), "{stdout}");
    assert!(stdout.contains("ship never ran"), "{stdout}");
}

#[test]
fn a_graph_naming_a_missing_node_is_refused() {
    let (url, _) = model_server();
    let home = home(&url);
    let file = graph(
        home.path(),
        "g.toml",
        "[[nodes]]\nid = \"a\"\ntask = \"x\"\n[[edges]]\nfrom = \"a\"\nto = \"ghost\"\n",
    );
    let out = ferrule(home.path(), &["graph", "run", file.to_str().unwrap()]);
    let stderr = plain(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("doesn't exist"), "{stderr}");
}
