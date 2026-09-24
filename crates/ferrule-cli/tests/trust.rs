//! M19 through the real `ferrule` binary, against a scripted
//! OpenAI-compatible server: a gated command in a run with nobody to ask is
//! refused and audited, a sub-agent inherits that and charges its parent's
//! tree, the run and day caps stop a run before the model is called again,
//! `ferrule stop` halts everything until `--clear`, and a `[trust]` that
//! doesn't validate is an error rather than no caps.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

fn text(m: &Value) -> String {
    m["content"].as_str().unwrap_or_default().to_string()
}

/// What the scripted model says. The task's marker picks the script.
fn reply(req: &Value) -> Value {
    let messages = req["messages"].as_array().unwrap();
    let system = messages.first().map(text).unwrap_or_default();
    let task = messages
        .iter()
        .find(|m| m["role"] == "user")
        .map(text)
        .unwrap_or_default();
    let last = messages.last().unwrap();
    let after_tool = last["role"] == "tool";
    if system.contains("## You are agent") {
        return if after_tool {
            answer(&format!("CHILD_SAW {}", text(last)))
        } else {
            call("shell", json!({"command": "rm -rf victim"}))
        };
    }
    if task.contains("SPAWN") {
        if last["role"] == "user" && text(last).contains("agent_notice") {
            return answer("ROOT_DONE");
        }
        return if after_tool {
            answer("ROOT_WAITING")
        } else {
            call(
                "spawn_agent",
                json!({"task": "clean up", "worktree": false}),
            )
        };
    }
    if task.contains("DELETE_IT") {
        return if after_tool {
            answer(&format!("SAW {}", text(last)))
        } else {
            call("shell", json!({"command": "rm -rf victim"}))
        };
    }
    if task.contains("LOOP") {
        return call("shell", json!({"command": "echo again"}));
    }
    answer("PLAIN")
}

fn call(name: &str, args: Value) -> Value {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{}", uuid_ish()),
                    "type": "function",
                    "function": {"name": name, "arguments": args.to_string()},
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    })
}

fn uuid_ish() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
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

/// A home with the scripted provider and `trust` as its `[trust]` table.
fn home(url: &str, trust: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    for d in ["work", "data", "home"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    std::fs::create_dir_all(home.join("work/victim")).unwrap();
    std::fs::write(home.join("work/victim/keep.txt"), "keep").unwrap();
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

[trust]
{trust}
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
    // stdin is not a terminal: nobody can approve.
    cmd.output().unwrap()
}

fn jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        plain(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_gated_command_with_nobody_to_ask_is_refused_and_audited() {
    let (url, seen) = model_server();
    let dir = home(&url, "");
    let home = dir.path();
    let out = ferrule(home, &["run", "DELETE_IT please"]);
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = plain(&out.stdout);
    assert!(stdout.contains("final: SAW refused by ferrule"), "{stdout}");
    assert!(stdout.contains("needs the owner's approval"), "{stdout}");
    assert!(stdout.contains("no terminal to ask"), "{stdout}");
    assert!(home.join("work/victim/keep.txt").exists());
    assert_eq!(seen.lock().unwrap().len(), 2);

    // The rows carry the run's tree; the refusal is in the audit log.
    let rows = jsonl(&home.join("data/ledger.jsonl"));
    assert_eq!(rows.len(), 2);
    let tree = rows[0]["tree"].as_str().unwrap().to_string();
    assert_eq!(rows[0]["session_id"], tree.as_str());
    let audit = jsonl(&home.join("data/trust/audit.jsonl"));
    let refused: Vec<&Value> = audit
        .iter()
        .filter(|e| e["event"] == "approval_answered")
        .collect();
    assert_eq!(refused.len(), 1, "{audit:?}");
    assert_eq!(refused[0]["tree"], tree.as_str());
    assert_eq!(refused[0]["detail"]["answer"], "unattended");
    assert!(refused[0]["detail"]["subject"]
        .as_str()
        .unwrap()
        .contains("rm -rf victim"));

    let out = ferrule(home, &["trust", "audit"]);
    assert!(plain(&out.stdout).contains("approval_answered"));

    // With the gates off, the same command runs.
    let (url, _) = model_server();
    let dir = self::home(&url, "gates = false");
    let out = ferrule(dir.path(), &["run", "DELETE_IT please"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(!dir.path().join("work/victim").exists());
}

#[test]
fn a_sub_agent_inherits_the_refusal_and_charges_its_parents_tree() {
    let (url, _) = model_server();
    let dir = home(&url, "");
    let home = dir.path();
    let out = ferrule(home, &["run", "SPAWN a cleaner"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(home.join("work/victim/keep.txt").exists());

    let rows = jsonl(&home.join("data/ledger.jsonl"));
    let child: Vec<&Value> = rows.iter().filter(|r| r["task_shape"] == "agent").collect();
    assert!(!child.is_empty(), "{rows:?}");
    let root = rows.iter().find(|r| r["task_shape"] == "run").unwrap();
    let tree = root["tree"].as_str().unwrap();
    assert_eq!(root["session_id"], tree);
    for r in &child {
        assert_eq!(r["tree"], tree, "a child's row is charged to its root");
        assert_ne!(r["session_id"], tree);
    }
    let audit = jsonl(&home.join("data/trust/audit.jsonl"));
    let refused = audit
        .iter()
        .find(|e| e["event"] == "approval_answered")
        .expect("the child's rm -rf was refused");
    assert_eq!(refused["tree"], tree);
    assert_eq!(refused["detail"]["answer"], "unattended");
}

#[test]
fn the_run_cap_stops_a_run_before_the_next_model_call() {
    let (url, seen) = model_server();
    // 110 tokens a call: the third call would start at 220.
    let dir = home(&url, "max_tokens_per_run = 200");
    let home = dir.path();
    let out = ferrule(home, &["run", "LOOP forever"]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    let stdout = plain(&out.stdout);
    assert!(
        stdout.contains("this run reached its token cap (max_tokens_per_run = 200)"),
        "{stdout}"
    );
    assert_eq!(seen.lock().unwrap().len(), 2);
    let audit = jsonl(&home.join("data/trust/audit.jsonl"));
    assert!(audit
        .iter()
        .any(|e| e["event"] == "cap_stop" && e["detail"]["cap"] == "max_tokens_per_run"));
    // 110 is under warn_at (160) and 220 is over the cap: a stop, no warning.
    assert!(!audit.iter().any(|e| e["event"] == "cap_warning"));
}

#[test]
fn the_day_cap_counts_what_other_processes_spent_today() {
    let (url, seen) = model_server();
    let dir = home(&url, "max_tokens_per_day = 1000");
    let home = dir.path();
    let now = chrono::Utc::now().to_rfc3339();
    let row = json!({
        "timestamp": now, "session_id": "elsewhere", "task_shape": "run",
        "provider": "mock", "model": "scripted", "iteration": 0, "call_kind": "turn",
        "input_tokens": 1000, "cached_input_tokens": 0, "output_tokens": 0,
        "tool_calls": 0, "latency_ms": 1, "outcome": "ok",
    });
    std::fs::write(home.join("data/ledger.jsonl"), format!("{row}\n")).unwrap();
    let out = ferrule(home, &["run", "anything"]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(plain(&out.stdout).contains("today's spend reached its token cap"));
    assert!(seen.lock().unwrap().is_empty(), "no model call was made");

    let out = ferrule(home, &["trust", "status"]);
    let status = plain(&out.stdout);
    assert!(status.contains("today:    1,000 tokens"), "{status}");
    assert!(status.contains("per day:  1,000 tokens"), "{status}");
}

#[test]
fn ferrule_stop_halts_every_run_until_cleared() {
    let (url, seen) = model_server();
    let dir = home(&url, "");
    let home = dir.path();
    let out = ferrule(home, &["stop", "--reason", "too much spend"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(home.join("data/trust/stop").exists());
    let out = ferrule(home, &["stop", "--status"]);
    assert!(plain(&out.stdout).contains("too much spend"));

    let out = ferrule(home, &["run", "anything"]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(plain(&out.stdout).contains("Ferrule is stopped (by ferrule stop"));
    assert!(seen.lock().unwrap().is_empty());
    let out = ferrule(home, &["trust", "status"]);
    assert!(plain(&out.stdout).contains("kill switch: ON"));

    let out = ferrule(home, &["stop", "--clear"]);
    assert!(plain(&out.stdout).contains("cleared"));
    let out = ferrule(home, &["run", "anything"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(plain(&out.stdout).contains("final: PLAIN"));
    let out = ferrule(home, &["stop", "--clear"]);
    assert!(plain(&out.stdout).contains("wasn't stopped"));

    let events: Vec<String> = jsonl(&home.join("data/trust/audit.jsonl"))
        .iter()
        .map(|e| e["event"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(events, ["stop_engaged", "stop_cleared"]);
}

#[test]
fn a_trust_table_that_doesnt_validate_is_an_error() {
    let (url, seen) = model_server();
    let dir = home(&url, "timezone = \"Mars/Olympus\"");
    let out = ferrule(dir.path(), &["run", "anything"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("isn't an IANA zone"),
        "{}",
        describe(&out)
    );
    assert!(seen.lock().unwrap().is_empty());
}
