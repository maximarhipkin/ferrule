//! M43 through the real `ferrule gateway` binary against a fake Telegram:
//! `/goal` starts a loop from a chat and reports its ending there, `/graph`
//! runs a graph on the gateway's shared supervisor, and a stranger gets
//! neither. Unix only.
#![cfg(unix)]

use super::channels::support;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use support::wait;

const LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Echoes the last user message.
fn model_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || serve_model(stream));
        }
    });
    url
}

fn serve_model(stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).unwrap_or(0) == 0 {
        return;
    }
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
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let req: Value = serde_json::from_slice(&body).unwrap();
    let last = req["messages"].as_array().and_then(|m| m.last()).cloned();
    let text = last
        .as_ref()
        .and_then(|m| m["content"].as_str().map(String::from))
        .unwrap_or_else(|| "done".into());
    let out = json!({
        "choices": [{
            "message": {"role": "assistant", "content": format!("echo: {text}")},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    });
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
        out.to_string().len(),
        out = out
    );
}

struct FakeTelegram {
    queue: Arc<Mutex<Vec<Value>>>,
    sent: Arc<Mutex<Vec<Value>>>,
    next: Mutex<i64>,
}

impl FakeTelegram {
    fn start() -> (String, Self) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let queue: Arc<Mutex<Vec<Value>>> = Arc::default();
        let sent: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (q, s) = (queue.clone(), sent.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (q, s) = (q.clone(), s.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request = String::new();
                    if reader.read_line(&mut request).unwrap_or(0) == 0 {
                        return;
                    }
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
                    if reader.read_exact(&mut body).is_err() {
                        return;
                    }
                    let out = if request.contains("getUpdates") {
                        let mut updates: Vec<Value> = q.lock().unwrap().drain(..).collect();
                        if updates.is_empty() {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                            updates = q.lock().unwrap().drain(..).collect();
                        }
                        json!({"ok": true, "result": updates})
                    } else {
                        let mut sent = s.lock().unwrap();
                        sent.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                        json!({"ok": true, "result": {"message_id": 1000 + sent.len()}})
                    };
                    let mut stream = stream;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
                        out.to_string().len()
                    );
                });
            }
        });
        (
            url,
            Self {
                queue,
                sent,
                next: Mutex::new(1),
            },
        )
    }

    fn say(&self, chat: i64, text: &str) {
        let mut next = self.next.lock().unwrap();
        *next += 1;
        self.queue.lock().unwrap().push(json!({
            "update_id": *next,
            "message": {"message_id": *next, "chat": {"id": chat}, "from": {"username": "max", "id": chat},
                        "text": text, "date": 1700000000},
        }));
    }

    fn texts_to(&self, chat: i64) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m["chat_id"] == chat.to_string().as_str() || m["chat_id"] == chat)
            .filter_map(|m| m["text"].as_str().map(String::from))
            .collect()
    }
}

fn home(url: &str, tg: &str, extra: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    for sub in ["work", "data", "home"] {
        std::fs::create_dir_all(home.join(sub)).unwrap();
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

[gateway]
telegram_token_env = "FERRULE_TEST_TG"
telegram_base_url = "{tg}"
telegram_allowed_chats = [42]

{extra}
"#,
        ),
    )
    .unwrap();
    dir
}

struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn gateway(home: &Path) -> Running {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args(["gateway"])
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_KEY", "sk-test")
        .env("FERRULE_TEST_TG", "TESTTOKEN")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for var in ["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_DATA_HOME"] {
        cmd.env(var, home.join("home"));
    }
    for var in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        cmd.env_remove(var);
    }
    Running(cmd.spawn().unwrap())
}

#[test]
fn a_goal_loop_from_a_chat_reports_back_when_the_judge_passes() {
    let url = model_server();
    let (tg_url, tg) = FakeTelegram::start();
    let dir = home(&url, &tg_url, "[agent]\nverify_command = \"true\"\n");
    let _gw = gateway(dir.path());

    tg.say(42, "/goal make the suite green");
    wait("the ack", LIMIT, || {
        tg.texts_to(42)
            .iter()
            .any(|t| t.contains("Goal loop started"))
    });
    wait("the completion report", LIMIT, || {
        tg.texts_to(42).iter().any(|t| t.contains("Goal met"))
    });
    let texts = tg.texts_to(42);
    let ack = texts
        .iter()
        .find(|t| t.contains("Goal loop started"))
        .unwrap();
    assert!(ack.contains("The judge is `true`"), "{ack}");
    assert!(ack.contains("ferrule run --resume"), "{ack}");

    // The loop's state file records the satisfied judge.
    let mut found = false;
    for entry in std::fs::read_dir(dir.path().join("data/sessions")).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if let Some(sid) = name.strip_suffix(".goal.json") {
            let state: Value = serde_json::from_str(
                &std::fs::read_to_string(dir.path().join("data/sessions").join(&name)).unwrap(),
            )
            .unwrap();
            assert_eq!(state["goal"], "make the suite green");
            assert_eq!(state["attempts"], 1, "the judge passed first time");
            assert!(sid.starts_with("goal__"), "{sid}");
            found = true;
        }
    }
    assert!(found, "a goal state file exists");
}

#[test]
fn a_goal_loop_without_a_verdict_reports_pending() {
    let url = model_server();
    let (tg_url, tg) = FakeTelegram::start();
    // The judge never passes (and the echo model never writes a file), so
    // the verify rounds run out and the loop reports pending.
    let dir = home(
        &url,
        &tg_url,
        "[agent]\nverify_command = \"test -f never.txt\"\n",
    );
    let _gw = gateway(dir.path());

    tg.say(42, "/goal write never.txt");
    wait("the pending report", LIMIT, || {
        tg.texts_to(42).iter().any(|t| t.contains("Goal pending"))
    });
    let texts = tg.texts_to(42);
    let report = texts.iter().find(|t| t.contains("Goal pending")).unwrap();
    assert!(report.contains("ferrule run --resume"), "{report}");
}

#[test]
fn a_chat_started_graph_runs_and_reports() {
    let url = model_server();
    let (tg_url, tg) = FakeTelegram::start();
    let dir = home(&url, &tg_url, "");
    // implement (echo agent, finishes) -> verify (check) — goal met.
    std::fs::write(
        dir.path().join("work/g.toml"),
        r#"
goal = "a graph from a chat"

[[nodes]]
id = "implement"
task = "do the thing"

[[nodes]]
id = "verify"
kind = "check"
command = "true"

[[edges]]
from = "implement"
to = "verify"
"#,
    )
    .unwrap();
    let _gw = gateway(dir.path());

    tg.say(42, "/graph g.toml --yes");
    wait("the ack", LIMIT, || {
        tg.texts_to(42)
            .iter()
            .any(|t| t.contains("Graph g.toml started"))
    });
    wait("the completion report", LIMIT, || {
        tg.texts_to(42)
            .iter()
            .any(|t| t.contains("graph: goal met"))
    });
}

#[test]
fn a_stranger_gets_neither_door() {
    let url = model_server();
    let (tg_url, tg) = FakeTelegram::start();
    let dir = home(&url, &tg_url, "[agent]\nverify_command = \"true\"\n");
    let _gw = gateway(dir.path());

    // Chat 99 isn't allowed at all: the channel ignores it (its one answer
    // is the pairing line, not a door).
    tg.say(99, "/goal anything");
    std::thread::sleep(std::time::Duration::from_secs(3));
    let texts = tg.texts_to(99);
    assert!(
        !texts.iter().any(|t| t.contains("Goal loop started")),
        "{texts:?}"
    );

    // /goal with no argument lists open loops (none) for the owner.
    tg.say(42, "/goal");
    wait("the empty list", LIMIT, || {
        tg.texts_to(42)
            .iter()
            .any(|t| t.contains("No open goal loops"))
    });
}
