//! M19b through the real `ferrule` binary, against a scripted
//! OpenAI-compatible server and a fake Telegram: `/status` answers while a
//! chat's turn hangs, `ferrule status` reads what the daemon writes, and
//! says so when no gateway runs.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

fn text(m: &Value) -> String {
    m["content"].as_str().unwrap_or_default().to_string()
}

/// What the scripted model says, keyed off the chat's first message.
fn reply(req: &Value) -> Value {
    let messages = req["messages"].as_array().unwrap();
    let task = messages
        .iter()
        .find(|m| m["role"] == "user")
        .map(text)
        .unwrap_or_default();
    let after_tool = messages.last().unwrap()["role"] == "tool";
    if task.contains("HANG") && !after_tool {
        return call("shell", json!({"command": "sleep 30"}));
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

/// A home with the scripted provider, and `extra` appended.
fn home(url: &str, extra: &str) -> tempfile::TempDir {
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

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        plain(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
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
    gateway_env(home, &[])
}

/// A gateway with `env` set, and systemd's variables only if they're in it.
fn gateway_env(home: &Path, env: &[(&str, &str)]) -> Running {
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
        "NOTIFY_SOCKET",
        "WATCHDOG_USEC",
        "WATCHDOG_PID",
    ] {
        cmd.env_remove(var);
    }
    cmd.envs(env.iter().copied());
    Running(cmd.spawn().unwrap())
}

/// `[gateway]` for the fake Telegram: chats -100 and 42 (the owner).
fn telegram(tg: &FakeTelegram) -> String {
    format!(
        "\n[gateway]\ntelegram_token_env = \"FERRULE_TEST_TG\"\ntelegram_base_url = \"{}\"\ntelegram_allowed_chats = [-100, 42]\n",
        tg.url
    )
}

#[test]
fn status_answers_from_any_chat_while_a_turn_hangs() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    let dir = home(&url, &telegram(&tg));
    let home = dir.path();
    let _gw = gateway(home);

    tg.say(42, "HANG please");
    // The chat's lane goes into `sleep 30`; /status comes from another
    // chat, and doesn't wait for it.
    let (mut n, mut report) = (0, String::new());
    for _ in 0..40 {
        tg.say(-100, "/status");
        (n, report) = tg.wait_for(-100, "ferrule ", n);
        if report.contains("tool `shell`") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    assert!(
        report.contains("telegram chat 42: tool `shell` (sleep 30) for "),
        "{report}"
    );
    for part in [
        "spend and caps:",
        "kill switch: off",
        "schedule:",
        "telegram: last ok poll",
        "recent warnings and errors:",
    ] {
        assert!(report.contains(part), "{part} missing from {report}");
    }
    assert!(
        !report.contains("TESTTOKEN") && !report.contains("sk-test"),
        "{report}"
    );
    tg.say(42, "/status");
    let (_, again) = tg.wait_for(42, "ferrule ", n);
    assert!(again.contains("telegram chat 42: tool `shell`"), "{again}");

    // The same report on the machine.
    let out = ferrule(home, &["status"]);
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        stdout.contains("telegram chat 42: tool `shell` (sleep 30)"),
        "{stdout}"
    );
}

#[test]
fn ferrule_status_says_when_no_gateway_is_running() {
    let dir = home("http://127.0.0.1:9/v1", "");
    let out = ferrule(dir.path(), &["status"]);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        plain(&out.stdout).contains("no ferrule gateway is running"),
        "{}",
        describe(&out)
    );
}

#[test]
fn the_watchdog_tells_the_owner_about_another_chats_stuck_turn() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    let extra = format!("{}\n[health]\nwatchdog_after_secs = 1\n", telegram(&tg));
    let dir = home(&url, &extra);
    let _gw = gateway(dir.path());

    tg.say(-100, "HANG please");
    // Chat 42 is the owner: the first private chat the gateway allows. The
    // stall to wait for is the tool's: on a slow machine the model call
    // before it can be quiet for a second too, and that's a stall of its
    // own, told first (Windows CI, M32).
    let (n, notice) = tg.wait_for(42, "Stuck on tool `shell`", 0);
    let (head, tail) = notice.split_once(" s in ").unwrap_or_default();
    let secs = head.strip_prefix("Stuck on tool `shell` (sleep 30) for ");
    assert!(
        secs.is_some_and(|s| s.parse::<u64>().is_ok_and(|s| s >= 1))
            && tail.starts_with("telegram chat -100, handling: 'HANG please'"),
        "{notice}"
    );
    // Once per stall, not once a tick (a tick is 250 ms): nothing more
    // over twelve of them.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let again = tg
        .sent
        .lock()
        .unwrap()
        .iter()
        .skip(n)
        .filter(|m| m["text"].as_str().unwrap_or_default().contains("Stuck on"))
        .count();
    assert_eq!(again, 0);
}

/// Waits until `path` exists and contains `needle`.
fn wait_for_file(path: &Path, needle: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{} never had {needle:?}: {text}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn texts_to(tg: &FakeTelegram, chat: i64) -> Vec<String> {
    tg.sent
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m["chat_id"] == chat.to_string().as_str())
        .map(|m| m["text"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn a_killed_gateway_says_what_it_interrupted_and_never_reruns_it() {
    let (url, seen) = model_server();
    let tg = FakeTelegram::start();
    let dir = home(&url, &telegram(&tg));
    let marker = dir.path().join("data/gateway/running.json");
    {
        let mut gw = gateway(dir.path());
        tg.say(-100, "HANG please");
        wait_for_file(&marker, "HANG please");
        // SIGKILL: no chance to clean up, like a crash or the OOM killer.
        gw.0.kill().unwrap();
        gw.0.wait().unwrap();
    }
    assert!(marker.exists());
    let calls = seen.lock().unwrap().len();

    let _gw = gateway(dir.path());
    let (_, notice) = tg.wait_for(42, "I restarted at ", 0);
    assert!(
        notice.ends_with("; the turn for telegram chat -100 was interrupted while handling: 'HANG please'. It won't be re-run — send it again if it's still needed."),
        "{notice}"
    );
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert_eq!(seen.lock().unwrap().len(), calls, "the turn was re-run");
    assert_eq!(
        texts_to(&tg, 42)
            .iter()
            .filter(|t| t.contains("I restarted"))
            .count(),
        1
    );
    // The new process owns the marker now: no turn in it.
    let now = wait_for_file(&marker, "\"turns\": []");
    assert!(!now.contains("HANG"), "{now}");
}

#[cfg(unix)]
#[test]
fn a_clean_stop_leaves_no_marker_and_the_next_start_is_only_back_up() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    let extra = format!("{}\n[health]\nnotify_on_start = true\n", telegram(&tg));
    let dir = home(&url, &extra);
    let gw_dir = dir.path().join("data/gateway");
    {
        let mut gw = gateway(dir.path());
        tg.wait_for(42, "Back up: ferrule ", 0);
        wait_for_file(&gw_dir.join("running.json"), "\"pid\"");
        unsafe { libc::kill(gw.0.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = loop {
            if let Some(status) = gw.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "SIGTERM didn't stop it"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(status.success(), "{status:?}");
    }
    assert!(!gw_dir.join("running.json").exists());
    assert!(!gw_dir.join("status.txt").exists());

    let _gw = gateway(dir.path());
    let (n, _) = tg.wait_for(42, "Back up: ferrule ", 0);
    tg.wait_for(42, "Back up: ferrule ", n);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let texts = texts_to(&tg, 42);
    assert!(
        texts.iter().all(|t| !t.contains("I restarted")),
        "{texts:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn under_systemd_the_gateway_pings_its_watchdog() {
    use std::os::unix::net::UnixDatagram;
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    let dir = home(&url, &telegram(&tg));
    let socket = dir.path().join("notify");
    let rx = UnixDatagram::bind(&socket).unwrap();
    rx.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    // WatchdogSec=0.3: a ping every 100 ms.
    let _gw = gateway_env(
        dir.path(),
        &[
            ("NOTIFY_SOCKET", socket.to_str().unwrap()),
            ("WATCHDOG_USEC", "300000"),
        ],
    );
    let mut buf = [0u8; 64];
    for _ in 0..3 {
        let n = rx.recv(&mut buf).expect("no watchdog ping");
        assert_eq!(&buf[..n], b"WATCHDOG=1");
    }
}

/// A heartbeat receiver: every POSTed body, parsed.
fn heartbeat_server() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/ping/abc123",
        listener.local_addr().unwrap().port()
    );
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
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
            if reader.read_exact(&mut body).is_ok() {
                log.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
            }
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nOK"
            );
        }
    });
    (url, seen)
}

#[test]
fn the_heartbeat_names_a_stuck_turn_but_not_its_message() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    let (beat_url, beats) = heartbeat_server();
    let extra = format!(
        "{}\n[health]\nheartbeat_url = \"{beat_url}\"\nheartbeat_secs = 1\nwatchdog_after_secs = 1\n",
        telegram(&tg)
    );
    let dir = home(&url, &extra);
    let _gw = gateway(dir.path());

    let wait = |pred: &dyn Fn(&Value) -> bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(v) = beats.lock().unwrap().iter().find(|v| pred(v)) {
                return v.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no such heartbeat in {:#?}",
                beats.lock().unwrap()
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    let ok = wait(&|v| v["status"] == "ok");
    assert_eq!(ok["reason"], "");
    assert_eq!(ok["version"], env!("CARGO_PKG_VERSION"));
    assert!(ok["uptime_secs"].is_u64(), "{ok}");

    tg.say(42, "HANG with my password hunter2-private and sk-test");
    let bad = wait(&|v| v["status"] == "degraded");
    let reason = bad["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("a turn in telegram chat 42 has made no progress for "),
        "{reason}"
    );
    let all = serde_json::to_string(&*beats.lock().unwrap()).unwrap();
    for leak in [
        "hunter2",
        "HANG",
        "password",
        "sk-test",
        "TESTTOKEN",
        "sleep",
        "shell",
    ] {
        assert!(!all.contains(leak), "{leak} in {all}");
    }
}

#[test]
fn a_heartbeat_that_fails_is_a_warning_and_the_gateway_keeps_answering() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    // Nothing listens there.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let extra = format!(
        "{}\n[health]\nheartbeat_url = \"http://127.0.0.1:{port}/ping/secret-uuid\"\nheartbeat_secs = 1\n",
        telegram(&tg)
    );
    let dir = home(&url, &extra);
    let _gw = gateway(dir.path());
    let (mut n, mut report) = (0, String::new());
    for _ in 0..40 {
        tg.say(42, "/status");
        (n, report) = tg.wait_for(42, "ferrule ", n);
        if report.contains("the heartbeat failed") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    assert!(report.contains("the heartbeat failed"), "{report}");
    assert!(!report.contains("secret-uuid"), "{report}");
    tg.say(42, "hello");
    tg.wait_for(42, "PLAIN", n);
}
