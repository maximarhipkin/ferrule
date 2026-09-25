//! M21 through the real `ferrule` binary: two or three scripted
//! OpenAI-compatible servers at once (one of them failing), a fake
//! Telegram and a temp config dir. The default answers and `/model default`
//! moves it for the next turn and past a restart; a pinned chat stays on its
//! model; a task and a sub-agent run on theirs; a 503 falls over and tells
//! the owner once, a 401 doesn't; an old config is untouched; the eval never
//! follows the owner's default, pins or fallback.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn text(m: &Value) -> String {
    m["content"].as_str().unwrap_or_default().to_string()
}

/// What a scripted server says: `<label>:<model>` for a plain turn, and
/// the root's side of the sub-agent test when asked to spawn.
fn reply(label: &str, req: &Value) -> Value {
    let messages = req["messages"].as_array().unwrap();
    let model = req["model"].as_str().unwrap_or_default();
    let system = messages.first().map(text).unwrap_or_default();
    if system.contains("## You are agent") {
        return answer(&format!("CHILD {label}:{model}"));
    }
    let task = messages
        .iter()
        .find(|m| m["role"] == "user")
        .map(text)
        .unwrap_or_default();
    if !task.contains("SPAWN") {
        return answer(&format!("{label}:{model}"));
    }
    // One spawn refused (not connected), one on `fast`, and once the child
    // has reported and spent the tree's budget, one more that's refused.
    let tools = messages.iter().filter(|m| m["role"] == "tool").count();
    let woken = messages
        .iter()
        .any(|m| m["role"] == "user" && text(m).contains("agent_notice"));
    match (tools, woken) {
        (0, _) => spawn("nowhere/at-all"),
        (1, _) => spawn("fast"),
        (2, false) => answer("ROOT_WAITING"),
        (2, true) => spawn("fast"),
        _ => answer("ROOT_DONE"),
    }
}

fn spawn(model: &str) -> Value {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{model}_{}", nanos()),
                    "type": "function",
                    "function": {
                        "name": "spawn_agent",
                        "arguments": json!({"task": "find the answer", "worktree": false, "model": model}).to_string(),
                    },
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    })
}

fn nanos() -> u128 {
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

/// A scripted server: its URL, every request it answered or refused, and
/// the status it answers with (200 until changed).
struct Server {
    url: String,
    seen: Arc<Mutex<Vec<Value>>>,
    status: Arc<AtomicU16>,
}

impl Server {
    fn start(label: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/v1",
            listener.local_addr().unwrap().port()
        );
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let status = Arc::new(AtomicU16::new(200));
        let (log, st) = (seen.clone(), status.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (log, st) = (log.clone(), st.clone());
                std::thread::spawn(move || serve(label, stream, &log, &st));
            }
        });
        Self { url, seen, status }
    }

    fn fail_with(&self, status: u16) -> &Self {
        self.status.store(status, Ordering::SeqCst);
        self
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

fn serve(label: &str, mut stream: TcpStream, log: &Mutex<Vec<Value>>, status: &AtomicU16) {
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
    let code = status.load(Ordering::SeqCst);
    let (head, out) = if code == 200 {
        ("200 OK", reply(label, &req).to_string())
    } else {
        (
            if code == 401 {
                "401 Unauthorized"
            } else if code == 400 {
                "400 Bad Request"
            } else {
                "503 Service Unavailable"
            },
            json!({"error": {"message": format!("scripted {code}")}}).to_string(),
        )
    };
    log.lock().unwrap().push(req);
    // Retry-After: 0, so the retries before a fall-over don't wait.
    let _ = write!(
        stream,
        "HTTP/1.1 {head}\r\ncontent-type: application/json\r\nretry-after: 0\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
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

/// Provider `a` (a-one, a-two) and `b` (b-large, b-small) on two servers,
/// `fast` = b/b-small, default_provider `a`, and `extra` appended.
fn two(a: &Server, b: &Server, extra: &str) -> String {
    format!(
        r#"default_provider = "a"

[providers.a]
base_url = "{}"
api_key_env = "FERRULE_TEST_KEY"
model = "a-one"

[providers.a.models."a-two"]

[providers.b]
base_url = "{}"
api_key_env = "FERRULE_TEST_KEY"
model = "b-large"

[providers.b.models."b-small"]

[models.aliases]
fast = "b/b-small"

[skills]
enabled = false

[sandbox]
mode = "off"
{extra}
"#,
        a.url, b.url
    )
}

fn home(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for d in ["work", "data", "home"] {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    std::fs::write(dir.path().join("ferrule.toml"), config).unwrap();
    dir
}

fn command(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args(args)
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_KEY", "sk-test")
        .env("FERRULE_TEST_TG", "TESTTOKEN");
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
        "NOTIFY_SOCKET",
        "WATCHDOG_USEC",
        "WATCHDOG_PID",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn ferrule(home: &Path, args: &[&str]) -> Output {
    command(home, args).output().unwrap()
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        plain(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn ledger(home: &Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("data/ledger.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

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

    /// `from` writes `text` in `chat`.
    fn say_from(&self, chat: i64, from: i64, text: &str) {
        let mut next = self.next.lock().unwrap();
        *next += 1;
        self.queue.lock().unwrap().push_back(json!({
            "update_id": *next,
            "message": {"message_id": *next, "chat": {"id": chat},
                        "from": {"id": from, "username": format!("u{from}")},
                        "text": text, "date": 1700000000},
        }));
    }

    fn say(&self, chat: i64, text: &str) {
        self.say_from(chat, chat, text)
    }

    /// Waits for a message to `chat` containing `needle`, after the first
    /// `from` messages sent anywhere; returns where it was and its text.
    fn wait_for(&self, chat: i64, needle: &str, from: usize) -> (usize, String) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
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
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn count(&self, chat: i64, needle: &str) -> usize {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|m| {
                m["chat_id"] == chat.to_string().as_str()
                    && m["text"].as_str().unwrap_or_default().contains(needle)
            })
            .count()
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
            std::thread::sleep(Duration::from_millis(100));
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

/// Kills the gateway when dropped, passed or not.
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn gateway(home: &Path) -> Running {
    let mut cmd = command(home, &["gateway"]);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    Running(cmd.spawn().unwrap())
}

/// `[gateway]` for the fake Telegram: the group -100 and 42, the owner
/// (the first private chat it allows).
fn telegram(tg: &FakeTelegram) -> String {
    format!(
        "\n[gateway]\ntelegram_token_env = \"FERRULE_TEST_TG\"\ntelegram_base_url = \"{}\"\ntelegram_allowed_chats = [-100, 42]\n",
        tg.url
    )
}

#[test]
fn the_default_answers_and_model_default_moves_it_for_the_next_turn_and_past_a_restart() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&two(&a, &b, &telegram(&tg)));
    let home = dir.path();
    let gw = gateway(home);

    tg.say(42, "hello");
    let (n, _) = tg.wait_for(42, "A:a-one", 0);
    tg.say(42, "/model");
    let (n, list) = tg.wait_for(42, "Default: a/a-one", n);
    for m in ["a/a-two", "b/b-large", "b/b-small"] {
        assert!(list.contains(m), "{m} missing from {list}");
    }
    tg.say(42, "/model default fast");
    let (n, said) = tg.wait_for(42, "b/b-small", n);
    assert!(!said.starts_with("Nothing changed"), "{said}");
    tg.say(42, "and now?");
    let (n, _) = tg.wait_for(42, "B:b-small", n);
    // A stranger's /model changes nothing.
    tg.say_from(-100, 7, "/model default a");
    tg.wait_for(-100, "Only the owner can change models.", n);

    // In the config, audited, and still the default after a restart.
    let config = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(config.contains(r#"default = "fast""#), "{config}");
    let audit = std::fs::read_to_string(home.join("data/trust/audit.jsonl")).unwrap();
    assert!(
        audit.contains("model.default") && audit.contains("telegram chat 42"),
        "{audit}"
    );
    drop(gw);
    let tg2 = FakeTelegram::start();
    let config = config.replace(&tg.url, &tg2.url);
    std::fs::write(home.join("ferrule.toml"), config).unwrap();
    let _gw = gateway(home);
    tg2.say(42, "after the restart");
    tg2.wait_for(42, "B:b-small", 0);

    // Every call is in the ledger with the model that ran it.
    let rows = ledger(home);
    let models: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| {
            (
                r["provider"].as_str().unwrap(),
                r["model"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        models,
        [("a", "a-one"), ("b", "b-small"), ("b", "b-small")],
        "{rows:#?}"
    );
}

#[test]
fn a_chat_pinned_to_b_answers_on_b_while_another_chat_stays_on_the_default() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&two(&a, &b, &telegram(&tg)));
    let home = dir.path();
    let _gw = gateway(home);

    // The owner pins the group.
    tg.say_from(-100, 42, "/model use b");
    let (n, said) = tg.wait_for(-100, "b/b-large", 0);
    assert!(!said.starts_with("Nothing changed"), "{said}");
    tg.say(-100, "group question");
    let (n, _) = tg.wait_for(-100, "B:b-large", n);
    tg.say(42, "private question");
    let (n, _) = tg.wait_for(42, "A:a-one", n);
    tg.say(-100, "/status");
    let (n, status) = tg.wait_for(-100, "ferrule ", n);
    assert!(
        status.contains("pinned: telegram:-100 → b/b-large"),
        "{status}"
    );
    assert!(status.contains("default: a/a-one"), "{status}");

    // Unpinned, it's back on the default; an unconnected pin is refused.
    tg.say_from(-100, 42, "/model use nowhere");
    let (n, said) = tg.wait_for(-100, "Nothing changed", n);
    assert!(said.contains("nowhere"), "{said}");
    tg.say_from(-100, 42, "/model use default");
    let (n, _) = tg.wait_for(-100, "default", n);
    tg.say(-100, "again");
    tg.wait_for(-100, "A:a-one", n);
    let pins = std::fs::read_to_string(home.join("data/models/pins.json")).unwrap();
    assert!(!pins.contains("-100"), "{pins}");
}

#[test]
fn a_task_runs_on_its_own_model_and_the_ledger_shows_it_per_call() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let dir = home(&two(&a, &b, ""));
    let home = dir.path();

    let out = ferrule(
        home,
        &[
            "tasks",
            "add",
            "digest",
            "--kind",
            "once",
            "--schedule",
            "2099-01-01T09:00:00Z",
            "--channel",
            "local",
            "--chat-id",
            "me",
            "--prompt",
            "write the digest",
            "--model",
            "fast",
        ],
    );
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(stdout.contains("model: fast"), "{stdout}");
    let id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("added task "))
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .to_string();
    let out = ferrule(home, &["tasks", "list"]);
    assert!(
        plain(&out.stdout).contains("model=fast"),
        "{}",
        describe(&out)
    );

    // Refused: a model that isn't connected.
    let out = ferrule(home, &["tasks", "model", &id, "nowhere"]);
    assert!(!out.status.success(), "{}", describe(&out));

    let out = ferrule(home, &["tasks", "run-now", &id]);
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(a.calls(), 0);
    assert_eq!(b.calls(), 1);
    assert_eq!(b.seen.lock().unwrap()[0]["model"], "b-small");

    // Back on the default.
    let out = ferrule(home, &["tasks", "model", &id, "default"]);
    assert!(out.status.success(), "{}", describe(&out));
    let out = ferrule(home, &["tasks", "run-now", &id]);
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(a.calls(), 1);

    let rows = ledger(home);
    let served: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{}/{}",
                r["provider"].as_str().unwrap(),
                r["model"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(served, ["b/b-small", "a/a-one"], "{rows:#?}");
    let audit = std::fs::read_to_string(home.join("data/trust/audit.jsonl")).unwrap();
    assert!(audit.contains("model.task"), "{audit}");
}

#[test]
fn a_sub_agent_runs_on_a_named_connected_model_and_still_counts_toward_its_trees_budget() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    // A child's call is 110 tokens: one exhausts this budget.
    let dir = home(&two(&a, &b, "\n[agents]\nmax_tokens = 100\n"));
    let home = dir.path();

    let out = ferrule(home, &["run", "SPAWN a child"]);
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(stdout.contains("final: ROOT_DONE"), "{stdout}");

    // The root ran on the default, the child on `fast`.
    let children_on = |s: &Server| {
        s.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| text(&r["messages"][0]).contains("## You are agent"))
            .map(|r| r["model"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(children_on(&a), Vec::<String>::new());
    assert_eq!(children_on(&b), ["b-small"]);

    // What the root was told: the unconnected model refused with the
    // reason, the one after the budget refused by the budget.
    let seen = a.seen.lock().unwrap();
    let last = &seen.last().unwrap()["messages"];
    let results: Vec<String> = last
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(text)
        .collect();
    assert_eq!(results.len(), 3, "{results:#?}");
    assert!(
        results[0].contains("can't run an agent on `nowhere/at-all`"),
        "{}",
        results[0]
    );
    assert!(
        results[2].contains("have used 110 tokens"),
        "{}",
        results[2]
    );

    let rows = ledger(home);
    let child: Vec<&Value> = rows.iter().filter(|r| r["task_shape"] == "agent").collect();
    assert_eq!(child.len(), 1, "{rows:#?}");
    assert_eq!(child[0]["provider"], "b");
    assert_eq!(child[0]["model"], "b-small");

    let out = ferrule(home, &["agents", "list", "--all"]);
    assert!(
        plain(&out.stdout).contains("on b/b-small"),
        "{}",
        describe(&out)
    );
}

#[test]
fn a_503_falls_over_and_tells_the_owner_once_and_a_401_does_not() {
    let (a, b, c) = (Server::start("A"), Server::start("B"), Server::start("C"));
    a.fail_with(503);
    c.fail_with(401);
    let tg = FakeTelegram::start();
    let extra = format!(
        r#"
[providers.c]
base_url = "{}"
api_key_env = "FERRULE_TEST_KEY"
model = "c-one"
{}"#,
        c.url,
        telegram(&tg)
    );
    let config = two(&a, &b, &extra).replace(
        "[models.aliases]",
        "[models]\nfallback = [\"b\"]\n\n[models.aliases]",
    );
    let dir = home(&config);
    let home = dir.path();
    let _gw = gateway(home);

    tg.say(42, "first");
    let (n, _) = tg.wait_for(42, "B:b-large", 0);
    tg.wait_for(42, "isn't answering", 0);
    assert!(a.calls() >= 1);
    tg.say(42, "second");
    tg.wait_for(42, "B:b-large", n);
    assert_eq!(tg.count(42, "isn't answering"), 1, "told once");

    // A key refused: no fall-over, and the owner hears why.
    let before = b.calls();
    tg.say_from(-100, 42, "/model use c");
    let (n, _) = tg.wait_for(-100, "c/c-one", 0);
    tg.say(-100, "third");
    let (_, said) = tg.wait_for(-100, "401", n);
    assert!(!said.contains("B:"), "{said}");
    assert_eq!(b.calls(), before, "a 401 doesn't fall over");
    assert!(c.calls() >= 1);

    let rows = ledger(home);
    assert!(
        rows.iter()
            .any(|r| r["provider"] == "a" && r["outcome"] != "ok"),
        "the failed attempts are in the ledger: {rows:#?}"
    );
    let audit = std::fs::read_to_string(home.join("data/trust/audit.jsonl")).unwrap();
    assert!(audit.contains("model.down"), "{audit}");
}

#[test]
fn an_old_config_behaves_exactly_as_before() {
    let a = Server::start("A");
    let config = format!(
        r#"default_provider = "mock"

[providers.mock]
base_url = "{}"
api_key_env = "FERRULE_TEST_KEY"
model = "scripted"

[skills]
enabled = false

[sandbox]
mode = "off"
"#,
        a.url
    );
    let dir = home(&config);
    let home = dir.path();
    let out = ferrule(home, &["run", "hello"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        plain(&out.stdout).contains("A:scripted"),
        "{}",
        describe(&out)
    );
    let out = ferrule(home, &["model", "list"]);
    let list = plain(&out.stdout);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(list.contains("mock/scripted"), "{list}");
    assert!(
        list.contains("fallback: off") || list.contains("Fallback: off"),
        "{list}"
    );

    let rows = ledger(home);
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0]["provider"], "mock");
    assert_eq!(rows[0]["model"], "scripted");
    // Nothing rewrote the config or pinned anything.
    assert_eq!(
        std::fs::read_to_string(home.join("ferrule.toml")).unwrap(),
        config
    );
    assert!(!home.join("data/models/pins.json").exists());
}

#[test]
fn the_eval_ignores_the_owners_default_pins_and_fallback_unless_a_model_is_picked() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let config = two(&a, &b, "").replace(
        "[models.aliases]",
        "[models]\ndefault = \"fast\"\nfallback = [\"b\"]\n\n[models.aliases]",
    );
    let dir = home(&config);
    let home = dir.path();
    let out = ferrule(home, &["model", "pin", "telegram:42", "b"]);
    assert!(out.status.success(), "{}", describe(&out));

    let suite = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/starter");
    let dry = |extra: &[&str]| {
        let mut args = vec![
            "eval",
            "run",
            suite.to_str().unwrap(),
            "--tag",
            "smoke",
            "--dry-run",
        ];
        args.extend_from_slice(extra);
        let out = ferrule(home, &args);
        assert!(out.status.success(), "{}", describe(&out));
        plain(&out.stdout)
    };
    let plan = dry(&[]);
    assert!(plan.contains("a-one via a,"), "{plan}");
    let plan = dry(&["--model", "fast"]);
    assert!(plan.contains("b-small via b,"), "{plan}");
    let plan = dry(&["--provider", "b"]);
    assert!(plan.contains("b-large via b,"), "{plan}");
    assert_eq!(a.calls() + b.calls(), 0);
}

/// M25: `[models]` without the network, and `b/b-mid` to route over.
fn routable(a: &Server, b: &Server, extra: &str) -> String {
    two(a, b, &format!("\n[providers.b.models.\"b-mid\"]\n{extra}")).replace(
        "[models.aliases]",
        "[models]\ncatalog_url = \"\"\n\n[models.aliases]",
    )
}

fn audit_events(home: &Path, event: &str) -> Vec<Value> {
    std::fs::read_to_string(home.join("data/trust/audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["event"] == event)
        .collect()
}

#[test]
fn model_route_sets_checks_shows_and_turns_off_routing_and_audits_it() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let dir = home(&routable(&a, &b, ""));
    let home = dir.path();

    // Refused, and nothing written.
    for (args, why) in [
        (
            vec!["model", "route", "set", "a/a-two", "nowhere/x"],
            "nowhere",
        ),
        (vec!["model", "route", "set", "a/a-two", "a/a-two"], "twice"),
        (
            vec!["model", "route", "set", "a/a-two", "tier:strong"],
            "tier",
        ),
        (
            vec!["model", "route", "set", "a/a-two", "b/b-mid", "--cap", "0"],
            "cap",
        ),
    ] {
        let out = ferrule(home, &args);
        assert!(!out.status.success(), "{args:?}: {}", describe(&out));
        assert!(
            plain(&out.stderr).contains(why),
            "{args:?}: {}",
            describe(&out)
        );
    }
    let out = ferrule(home, &["model", "route", "set", "a/a-two"]);
    assert!(!out.status.success(), "one tier: {}", describe(&out));
    let config = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(!config.contains("[routing]"), "{config}");
    assert!(audit_events(home, "routing.set").is_empty());

    let out = ferrule(
        home,
        &["model", "route", "set", "a/a-two", "b/b-mid", "--cap", "2"],
    );
    assert!(out.status.success(), "{}", describe(&out));
    let said = plain(&out.stdout);
    assert!(
        said.contains("Routing is on") && said.contains("$2.00"),
        "{said}"
    );
    let config = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(
        config.contains("[routing]") && config.contains("strong_daily_usd = 2"),
        "{config}"
    );
    let set = audit_events(home, "routing.set");
    assert_eq!(set.len(), 1, "{set:?}");
    assert_eq!(set[0]["detail"]["by"], "cli");
    assert_eq!(set[0]["detail"]["from"]["enabled"], false);
    assert_eq!(
        set[0]["detail"]["to"]["tiers"],
        json!(["a/a-two", "b/b-mid"])
    );
    assert_eq!(set[0]["detail"]["to"]["strong_daily_usd"], 2.0);

    let out = ferrule(home, &["model", "route", "--json"]);
    assert!(out.status.success(), "{}", describe(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["routing"]["on"], true, "{v:#}");
    assert_eq!(v["routing"]["tiers"][1]["reference"], "b/b-mid");
    assert_eq!(v["last_7_days"]["escalations"], 0);
    let out = ferrule(home, &["model", "route"]);
    let text = plain(&out.stdout);
    assert!(
        text.contains("Routing: on") && text.contains("b/b-mid"),
        "{text}"
    );
    let out = ferrule(home, &["model", "list"]);
    assert!(
        plain(&out.stdout).contains("Routing: on"),
        "{}",
        describe(&out)
    );

    // A tier's model can't be removed from under it.
    let out = ferrule(home, &["model", "remove", "b/b-mid"]);
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(plain(&out.stderr).contains("tier"), "{}", describe(&out));

    // `--no-cap` and `--sticky`.
    let out = ferrule(
        home,
        &[
            "model", "route", "set", "a/a-two", "b/b-mid", "--no-cap", "--sticky",
        ],
    );
    assert!(out.status.success(), "{}", describe(&out));
    let config = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(
        !config.contains("strong_daily_usd") && config.contains("de_escalate = false"),
        "{config}"
    );

    let out = ferrule(home, &["model", "route", "off"]);
    assert!(
        plain(&out.stdout).contains("Routing is off"),
        "{}",
        describe(&out)
    );
    let out = ferrule(home, &["model", "route", "off"]);
    assert!(
        plain(&out.stdout).contains("off already"),
        "{}",
        describe(&out)
    );
    assert_eq!(audit_events(home, "routing.unset").len(), 1);
    // The tiers stay written; a `tier:` ref still names its model.
    let config = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(
        config.contains("enabled = false") && config.contains("b/b-mid"),
        "{config}"
    );
}

#[test]
fn a_bad_request_on_the_cheap_tier_moves_the_turn_up_and_the_next_turn_starts_cheap() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    // A request the cheap model refuses; retrying it there won't help.
    a.fail_with(400);
    let tg = FakeTelegram::start();
    let dir = home(&routable(
        &a,
        &b,
        &format!(
            "\n[routing]\nenabled = true\ntiers = [\"a/a-two\", \"b/b-mid\"]\n{}",
            telegram(&tg)
        ),
    ));
    let home = dir.path();
    let _gw = gateway(home);

    tg.say(42, "hello");
    let (n, _) = tg.wait_for(42, "B:b-mid", 0);
    let rows = ledger(home);
    let up = rows
        .iter()
        .find(|r| r["route"]["tier"] == "b/b-mid")
        .unwrap_or_else(|| panic!("no row on the strong tier: {rows:#?}"));
    assert!(
        up["route"]["escalated"]
            .as_str()
            .unwrap()
            .starts_with("call_failed"),
        "{up:#}"
    );
    assert!(
        rows.iter()
            .any(|r| r["route"]["tier"] == "a/a-two" && r["outcome"] != "ok"),
        "{rows:#?}"
    );
    let esc = audit_events(home, "routing.escalate");
    assert_eq!(esc[0]["detail"]["to"], "b/b-mid", "{esc:?}");

    // Back on its feet, the cheap tier answers the next turn.
    a.fail_with(200);
    tg.say(42, "again");
    let (n, _) = tg.wait_for(42, "A:a-two", n);

    // A chat pinned to a tier starts there.
    tg.say_from(-100, 42, "/model tier:strong");
    let (n, said) = tg.wait_for(-100, "b/b-mid", n);
    assert!(!said.starts_with("Nothing changed"), "{said}");
    tg.say(-100, "group question");
    let (n, _) = tg.wait_for(-100, "B:b-mid", n);
    tg.say(42, "private question");
    tg.wait_for(42, "A:a-two", n);
}
