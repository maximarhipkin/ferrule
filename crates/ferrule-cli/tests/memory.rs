//! M15 through the real `ferrule` binary, against a scripted
//! OpenAI-compatible server: a fact stored in one `ferrule run` and
//! corrected with `update_memory` in a second is what the third session
//! recalls (a user message after the goal since M27); a read-only
//! sub-agent gets `recall` and `search_history` but no tool that writes
//! memory.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Script = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

fn text(m: &Value) -> String {
    m["content"].as_str().unwrap_or_default().to_string()
}

fn system(req: &Value) -> String {
    text(&req["messages"][0])
}

/// The recalled memory block: since M27 a user message after the goal, so
/// the system prompt stays the same bytes for every session.
fn recalled(req: &Value) -> String {
    req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(text)
        .find(|t| t.starts_with("[Long-term memory]"))
        .unwrap_or_default()
}

/// The session's first user message: which run a request belongs to.
fn first_user(req: &Value) -> String {
    req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .map(text)
        .unwrap_or_default()
}

fn last(req: &Value) -> Value {
    req["messages"].as_array().unwrap().last().unwrap().clone()
}

fn tool_names(req: &Value) -> Vec<String> {
    req["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
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

/// Serves `script` on a thread per connection; returns the base URL and
/// every request body it saw.
fn model_server(script: Script) -> (String, Arc<Mutex<Vec<Value>>>) {
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

fn home_with(url: &str) -> tempfile::TempDir {
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
    dir
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

fn run_ok(home: &Path, args: &[&str]) -> String {
    let out = ferrule(home, args);
    let stdout = plain(&out.stdout);
    assert!(
        out.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// The `- #id fact` line for a fact in the session's memory block.
fn memory_id(system: &str, fact: &str) -> Option<i64> {
    let block = system.split("[Long-term memory]\n").nth(1)?;
    block.lines().find_map(|l| {
        let rest = l.strip_prefix("- #")?;
        let (id, content) = rest.split_once(' ')?;
        (content == fact).then(|| id.parse().ok())?
    })
}

#[test]
fn a_correction_made_through_the_tool_is_what_a_later_session_recalls() {
    let script: Script = Arc::new(|req: &Value| {
        let goal = first_user(req);
        let last = last(req);
        if last["role"] == "tool" {
            return answer(&format!("DONE {}", text(&last)));
        }
        if goal.starts_with("S1") {
            return call(
                "remember",
                json!({"content": "The deploy target is fly.io", "tags": ["infra"]}),
            );
        }
        if goal.starts_with("S2") {
            // The model corrects the fact by the id its prompt shows.
            return match memory_id(&recalled(req), "The deploy target is fly.io") {
                Some(id) => call(
                    "update_memory",
                    json!({"id": id, "content": "The deploy target is render"}),
                ),
                None => answer("NO_OLD_FACT_IN_PROMPT"),
            };
        }
        answer("S3_ANSWER")
    });
    let (url, seen) = model_server(script);
    let dir = home_with(&url);
    let home = dir.path();

    let out = run_ok(home, &["run", "S1: remember where we deploy (fly.io)"]);
    assert!(out.contains("DONE remembered (#1)"), "{out}");
    let out = run_ok(
        home,
        &[
            "run",
            "S2: the deploy target moved to render, fix your memory",
        ],
    );
    assert!(out.contains("DONE remembered (#2), replacing #1"), "{out}");
    run_ok(home, &["run", "S3: which deploy target do we use?"]);

    let seen = seen.lock().unwrap();
    let s3: Vec<&Value> = seen
        .iter()
        .filter(|r| first_user(r).starts_with("S3"))
        .collect();
    assert_eq!(s3.len(), 1);
    assert!(!system(s3[0]).contains("[Long-term memory]"));
    let prompt = recalled(s3[0]);
    assert!(
        prompt.contains("[Long-term memory]\n- #2 The deploy target is render"),
        "{prompt}"
    );
    assert!(!prompt.contains("fly.io"), "{prompt}");

    // The CLI sees the same store: the old version is history, not recalled.
    let out = run_ok(home, &["memory", "search", "deploy"]);
    assert!(out.contains("#2 The deploy target is render"), "{out}");
    assert!(!out.contains("fly.io"), "{out}");
    let out = run_ok(home, &["memory", "forget", "2"]);
    assert_eq!(out.trim(), "forgot #1, #2");
    let out = run_ok(home, &["memory", "recent"]);
    assert!(out.trim().is_empty(), "{out}");
    assert!(!ferrule(home, &["memory", "forget", "2"]).status.success());
}

#[test]
fn a_read_only_child_can_recall_and_search_history_but_not_write_memory() {
    let script: Script = Arc::new(|req: &Value| {
        let last = last(req);
        if system(req).contains("## You are agent") {
            // Slow enough that the root's first run has ended.
            std::thread::sleep(Duration::from_millis(1500));
            return answer("CHILD_REPORT checked");
        }
        if last["role"] == "user" && text(&last).contains("agent_notice") {
            return answer("ROOT_DONE");
        }
        if last["role"] == "tool" {
            return answer("ROOT_WAITING");
        }
        call(
            "spawn_agent",
            json!({"task": "verify the answer", "role": "verifier", "worktree": false}),
        )
    });
    let (url, seen) = model_server(script);
    let dir = home_with(&url);
    let home = dir.path();

    let out = run_ok(home, &["run", "have a verifier check it"]);
    assert!(out.contains("final: ROOT_DONE"), "{out}");

    let seen = seen.lock().unwrap();
    let (child, root): (Vec<&Value>, Vec<&Value>) = seen
        .iter()
        .partition(|r| system(r).contains("## You are agent"));
    assert_eq!(child.len(), 1);
    let child_tools = tool_names(child[0]);
    for has in ["recall", "search_history"] {
        assert!(child_tools.iter().any(|t| t == has), "{child_tools:?}");
    }
    for lacks in ["remember", "update_memory", "forget", "write_file"] {
        assert!(!child_tools.iter().any(|t| t == lacks), "{child_tools:?}");
    }
    let root_tools = tool_names(root[0]);
    for has in [
        "remember",
        "recall",
        "update_memory",
        "forget",
        "search_history",
    ] {
        assert!(root_tools.iter().any(|t| t == has), "{root_tools:?}");
    }
}
