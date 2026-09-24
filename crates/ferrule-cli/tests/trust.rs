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

/// A Bot API stand-in: `getUpdates` hands out what the test queued (or
/// nothing after a short wait), `sendMessage` bodies are kept.
struct FakeTelegram {
    url: String,
    queue: Arc<Mutex<std::collections::VecDeque<Value>>>,
    sent: Arc<Mutex<Vec<Value>>>,
    next: Mutex<i64>,
}

impl FakeTelegram {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let queue: Arc<Mutex<std::collections::VecDeque<Value>>> = Arc::default();
        let sent: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (q, s) = (queue.clone(), sent.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (q, s) = (q.clone(), s.clone());
                std::thread::spawn(move || telegram_serve(stream, &q, &s));
            }
        });
        Self {
            url,
            queue,
            sent,
            next: Mutex::new(1),
        }
    }

    fn say(&self, chat: i64, text: &str) {
        let mut next = self.next.lock().unwrap();
        *next += 1;
        self.queue.lock().unwrap().push_back(json!({
            "update_id": *next,
            "message": {"message_id": *next, "chat": {"id": chat}, "from": {"username": "max"},
                        "text": text, "date": 1700000000},
        }));
    }

    /// Waits for a message to `chat` containing `needle`, after the first
    /// `from` messages.
    fn wait_for(&self, chat: i64, needle: &str, from: usize) -> (usize, String) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            {
                let sent = self.sent.lock().unwrap();
                for (i, m) in sent.iter().enumerate().skip(from) {
                    let text = m["text"].as_str().unwrap_or_default();
                    if m["chat_id"] == chat.to_string().as_str() && text.contains(needle) {
                        return (i + 1, text.to_string());
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "no message to {chat} with {needle:?}; sent: {sent:#?}"
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

fn telegram_serve(
    mut stream: TcpStream,
    queue: &Mutex<std::collections::VecDeque<Value>>,
    sent: &Mutex<Vec<Value>>,
) {
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
    reader.read_exact(&mut body).unwrap();
    let out = if request.contains("getUpdates") {
        let mut updates: Vec<Value> = queue.lock().unwrap().drain(..).collect();
        if updates.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(100));
            updates = queue.lock().unwrap().drain(..).collect();
        }
        json!({"ok": true, "result": updates})
    } else {
        let mut sent = sent.lock().unwrap();
        sent.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
        json!({"ok": true, "result": {"message_id": 1000 + sent.len()}})
    }
    .to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
        out.len()
    );
}

/// Kills the gateway when the test ends, passed or not.
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
    Running(cmd.spawn().unwrap())
}

#[test]
fn the_owner_approves_stops_and_resumes_from_telegram() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    let dir = home(
        &url,
        &format!(
            "\n[gateway]\ntelegram_token_env = \"FERRULE_TEST_TG\"\ntelegram_base_url = \"{}\"\ntelegram_allowed_chats = [-100, 42]\n",
            tg.url
        ),
    );
    let home = dir.path();
    let _gw = gateway(home);

    // A gated command in the owner's chat is asked about there; yes runs it.
    tg.say(42, "DELETE_IT now");
    let (n, question) = tg.wait_for(42, "Reply `yes` to allow it", 0);
    assert!(question.contains("rm -rf victim"), "{question}");
    assert!(home.join("work/victim/keep.txt").exists());
    tg.say(42, "yes");
    let (n, _) = tg.wait_for(42, "SAW", n);
    assert!(!home.join("work/victim").exists());

    // Only the owner chat resumes; any allowed chat can stop.
    tg.say(-100, "/stop too much");
    let (n, _) = tg.wait_for(-100, "Stopped:", n);
    assert!(home.join("data/trust/stop").exists());
    tg.say(42, "hello");
    let (n, _) = tg.wait_for(42, "Ferrule is stopped (by telegram chat -100", n);
    tg.say(-100, "/resume");
    let (n, _) = tg.wait_for(-100, "Only the owner chat", n);
    tg.say(42, "/resume");
    let (n, _) = tg.wait_for(42, "Resumed", n);
    // (chat 42's session would replay DELETE_IT: the script keys off its
    // first message.)
    tg.say(-100, "hello again");
    tg.wait_for(-100, "PLAIN", n);
    assert!(!home.join("data/trust/stop").exists());

    let audit = jsonl(&home.join("data/trust/audit.jsonl"));
    let events: Vec<&str> = audit.iter().map(|e| e["event"].as_str().unwrap()).collect();
    assert_eq!(
        events,
        [
            "approval_asked",
            "approval_answered",
            "stop_engaged",
            "stop_cleared"
        ],
        "{audit:#?}"
    );
    assert_eq!(audit[1]["detail"]["answer"], "yes");
    assert_eq!(audit[1]["tree"], "telegram__42");
}
