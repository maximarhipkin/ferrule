//! M42 part 5 goal loops, through the real `ferrule` binary against a
//! scripted OpenAI-compatible server: the judge runs until it passes, a
//! budget cut ends goal-pending and `--resume` finishes it, the judge runs
//! even when nothing changed, and a goal without a judge is refused.
//! Unix only: the checks are `sh` commands.
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

fn home(url: &str, extra: &str) -> tempfile::TempDir {
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

{extra}
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

fn texts(out: &Output) -> (String, String) {
    (plain(&out.stdout), plain(&out.stderr))
}

/// The session id from the "goal loop session <id>" stderr line.
fn session_id(stderr: &str) -> String {
    stderr
        .split("goal loop session ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or_else(|| panic!("no session line in stderr:\n{stderr}"))
        .to_string()
}

fn goal_state(home: &Path, session: &str) -> Value {
    let file = home
        .join("data/sessions")
        .join(format!("{session}.goal.json"));
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    serde_json::from_str(&text).unwrap()
}

const GOAL_CHECK: &str = "test -f goal.txt || { echo 'goal.txt is missing'; exit 1; }";

#[test]
fn a_goal_loop_runs_the_judge_until_it_passes_and_reports_goal_met() {
    let script: Script = Arc::new(|req: &Value| {
        let last = last(req);
        let said = text(&last);
        if last["role"] == "user" && said.contains("goal.txt is missing") {
            return call("write_file", json!({"path": "goal.txt", "content": "met"}));
        }
        if last["role"] == "tool" {
            return answer("DONE");
        }
        call("write_file", json!({"path": "draft.txt", "content": "wip"}))
    });
    let (url, seen) = model_server(script);
    let dir = home(&url, "");
    let out = ferrule(
        dir.path(),
        &["run", "--goal", "--verify", GOAL_CHECK, "reach the goal"],
    );
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("final: DONE"), "{stdout}");
    assert!(stdout.contains("goal met"), "{stdout}");

    let sid = session_id(&stderr);
    let state = goal_state(dir.path(), &sid);
    assert_eq!(state["goal"], "reach the goal");
    assert_eq!(state["attempts"], 2, "{state}");
    assert!(state["last_failure"].is_null(), "{state}");
    let _ = seen;
}

#[test]
fn a_budget_cut_ends_goal_pending_and_resume_finishes_the_loop() {
    let script: Script = Arc::new(|req: &Value| {
        let last = last(req);
        let resumed = req["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| text(m).contains("goal loop, resumed"));
        if resumed {
            // The resumed round: fix the problem and finish.
            if last["role"] == "tool" {
                return answer("DONE");
            }
            return call("write_file", json!({"path": "goal.txt", "content": "met"}));
        }
        // First round: keep working, never finish.
        call(
            "write_file",
            json!({"path": "wip.txt", "content": "still working"}),
        )
    });
    let (url, _) = model_server(script);
    let dir = home(&url, "");

    let out = ferrule(
        dir.path(),
        &[
            "run",
            "--goal",
            "--verify",
            GOAL_CHECK,
            "--max-iterations",
            "2",
            "reach the goal",
        ],
    );
    let (stdout, stderr) = texts(&out);
    assert_eq!(out.status.code(), Some(2), "stdout:\n{stdout}");
    assert!(stdout.contains("incomplete"), "{stdout}");
    assert!(stdout.contains("goal pending"), "{stdout}");
    assert!(stdout.contains("--resume"), "{stdout}");

    let sid = session_id(&stderr);
    let state = goal_state(dir.path(), &sid);
    assert_eq!(state["attempts"], 0, "the judge never ran: {state}");

    // Resume with no prompt: the loop's own memory carries the goal.
    let out = ferrule(dir.path(), &["run", "--resume", &sid]);
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("final: DONE"), "{stdout}");
    assert!(stdout.contains("goal met"), "{stdout}");

    let state = goal_state(dir.path(), &sid);
    assert_eq!(state["attempts"], 1, "{state}");
    assert!(state["last_failure"].is_null(), "{state}");
}

#[test]
fn the_judge_runs_even_when_the_run_changed_nothing() {
    let script: Script = Arc::new(|req: &Value| {
        let last = last(req);
        let said = text(&last);
        if last["role"] == "user" && said.contains("goal.txt is missing") {
            return call("write_file", json!({"path": "goal.txt", "content": "met"}));
        }
        if last["role"] == "tool" {
            return answer("DONE");
        }
        // No tool call at all: the run changes nothing.
        answer("nothing to do")
    });
    let (url, seen) = model_server(script);
    let dir = home(&url, "");
    let out = ferrule(
        dir.path(),
        &["run", "--goal", "--verify", GOAL_CHECK, "reach the goal"],
    );
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("goal met"), "{stdout}");
    // The judge failed once with no files changed, and the failure went back.
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|r| text(&last(r)).contains("goal.txt is missing")),
        "the judge's word reached the model"
    );
    let sid = session_id(&stderr);
    assert_eq!(goal_state(dir.path(), &sid)["attempts"], 2);
}

#[test]
fn a_goal_without_a_judge_is_refused() {
    let script: Script = Arc::new(|_: &Value| answer("unused"));
    let (url, _) = model_server(script);
    let dir = home(&url, "");
    let out = ferrule(dir.path(), &["run", "--goal", "reach the goal"]);
    let (_, stderr) = texts(&out);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("needs a judge"), "{stderr}");
}

#[test]
fn a_goal_loop_can_use_the_configured_judge() {
    let script: Script = Arc::new(|req: &Value| {
        let last = last(req);
        let said = text(&last);
        if last["role"] == "user" && said.contains("goal.txt is missing") {
            return call("write_file", json!({"path": "goal.txt", "content": "met"}));
        }
        if last["role"] == "tool" {
            return answer("DONE");
        }
        call("write_file", json!({"path": "draft.txt", "content": "wip"}))
    });
    let (url, _) = model_server(script);
    let dir = home(
        &url,
        &format!("[agent]\nverify_command = \"{GOAL_CHECK}\"\n"),
    );
    let out = ferrule(dir.path(), &["run", "--goal", "reach the goal"]);
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("goal met"), "{stdout}");
}
