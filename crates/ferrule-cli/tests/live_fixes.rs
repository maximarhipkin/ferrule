//! M19c through the real `ferrule` binary: every reason a live bot doesn't
//! answer reaches the owner or `/status` — a lasting 409, a webhook, a
//! message that isn't text, an ignored chat, OpenRouter's 404 and 429 — and
//! `ferrule doctor` names the webhook, the `:free` model and a second
//! gateway. The model server answers with OpenRouter's own error bodies.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TOKEN: &str = "123456:SECRET-bot-token";
const NO_TOOLS: &str = r#"{"error":{"message":"No endpoints found that support tool use. To learn more about provider routing, visit: https://openrouter.ai/docs/provider-routing","code":404}}"#;
const FREE_429: &str = r#"{"error":{"message":"Rate limit exceeded: free-models-per-min. ","code":429,"metadata":{"headers":{"X-RateLimit-Limit":"20","X-RateLimit-Remaining":"0"}}}}"#;
const WEBHOOK: &str = "https://hooks.example.com/secret-path/abc";

/// One HTTP request: its first line and its body.
fn read_request(stream: &TcpStream) -> Option<(String, Vec<u8>)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut first = String::new();
    if reader.read_line(&mut first).ok()? == 0 {
        return None;
    }
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok()?;
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some((first, body))
}

fn respond(mut stream: TcpStream, status: &str, headers: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// An OpenRouter stand-in: the last user message picks the answer —
/// NOTOOLS gets the 404, LIMIT the free pool's 429 (Retry-After 2), the
/// rest "PLAIN". Every chat request is kept.
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
            std::thread::spawn(move || {
                let Some((first, body)) = read_request(&stream) else {
                    return;
                };
                if first.starts_with("GET") {
                    let models = json!({"data": [{"id": "qwen/qwen3.8-27b:free"}]}).to_string();
                    return respond(stream, "200 OK", "", &models);
                }
                let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let last = req["messages"]
                    .as_array()
                    .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or_default()
                    .to_string();
                log.lock().unwrap().push(req);
                if last.contains("NOTOOLS") {
                    respond(stream, "404 Not Found", "", NO_TOOLS);
                } else if last.contains("LIMIT") {
                    respond(
                        stream,
                        "429 Too Many Requests",
                        "retry-after: 2\r\n",
                        FREE_429,
                    );
                } else {
                    let answer = json!({
                        "choices": [{"message": {"role": "assistant", "content": "PLAIN"}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
                    });
                    respond(stream, "200 OK", "", &answer.to_string());
                }
            });
        }
    });
    (url, seen)
}

#[derive(Default)]
struct TgState {
    queue: VecDeque<Value>,
    sent: Vec<Value>,
    methods: Vec<String>,
    webhook: String,
    /// Answer `getUpdates` with the other-poller 409 until then.
    conflict_until: Option<Instant>,
    next: i64,
}

/// A Bot API stand-in with a webhook and a 409 to switch on.
struct FakeTelegram {
    url: String,
    state: Arc<Mutex<TgState>>,
}

impl FakeTelegram {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let state: Arc<Mutex<TgState>> = Arc::default();
        let st = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let st = st.clone();
                std::thread::spawn(move || telegram_serve(stream, &st));
            }
        });
        Self { url, state }
    }

    fn push(&self, chat: i64, fields: Value) {
        let mut st = self.state.lock().unwrap();
        st.next += 1;
        let mut message = json!({"message_id": st.next, "chat": {"id": chat, "type": "private"},
                                 "from": {"username": "max"}, "date": 1700000000});
        for (k, v) in fields.as_object().unwrap() {
            message[k] = v.clone();
        }
        let update = json!({"update_id": st.next, "message": message});
        st.queue.push_back(update);
    }

    fn say(&self, chat: i64, text: &str) {
        self.push(chat, json!({ "text": text }));
    }

    fn sent(&self) -> Vec<Value> {
        self.state.lock().unwrap().sent.clone()
    }

    /// Waits for a message to `chat` containing `needle`, after the first
    /// `from` sent; returns the count so far and the text.
    fn wait_for(&self, chat: i64, needle: &str, from: usize) -> (usize, String) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let sent = self.sent();
            for (i, m) in sent.iter().enumerate().skip(from) {
                let text = m["text"].as_str().unwrap_or_default();
                if m["chat_id"] == chat.to_string().as_str() && text.contains(needle) {
                    return (i + 1, text.to_string());
                }
            }
            assert!(
                Instant::now() < deadline,
                "no message to {chat} with {needle:?}; sent: {sent:#?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `/status` from `chat` until its report contains `needle`.
    fn status_until(&self, chat: i64, needle: &str) -> String {
        let (mut n, mut report) = (0, String::new());
        for _ in 0..60 {
            self.say(chat, "/status");
            (n, report) = self.wait_for(chat, "ferrule ", n);
            if report.contains(needle) {
                return report;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!("/status never had {needle:?}: {report}");
    }

    fn texts_to(&self, chat: i64) -> Vec<String> {
        self.sent()
            .iter()
            .filter(|m| m["chat_id"] == chat.to_string().as_str())
            .map(|m| m["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

fn telegram_serve(stream: TcpStream, state: &Mutex<TgState>) {
    let Some((first, body)) = read_request(&stream) else {
        return;
    };
    let method = first
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.split('?').next())
        .and_then(|p| p.rsplit('/').next())
        .unwrap_or_default()
        .to_string();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let conflict = |description: &str| {
        json!({"ok": false, "error_code": 409, "description": description}).to_string()
    };
    let mut st = state.lock().unwrap();
    st.methods.push(method.clone());
    match method.as_str() {
        "getUpdates" => {
            if !st.webhook.is_empty() {
                drop(st);
                return respond(stream, "409 Conflict", "", &conflict("Conflict: can't use getUpdates method while webhook is active; use deleteWebhook to delete the webhook first"));
            }
            if st.conflict_until.is_some_and(|t| Instant::now() < t) {
                drop(st);
                return respond(stream, "409 Conflict", "", &conflict("Conflict: terminated by other getUpdates request; make sure that only one bot instance is running"));
            }
            let mut updates: Vec<Value> = st.queue.drain(..).collect();
            if updates.is_empty() {
                drop(st);
                std::thread::sleep(Duration::from_millis(100));
                updates = state.lock().unwrap().queue.drain(..).collect();
            } else {
                drop(st);
            }
            respond(
                stream,
                "200 OK",
                "",
                &json!({"ok": true, "result": updates}).to_string(),
            );
        }
        "getMe" => {
            drop(st);
            let me =
                json!({"ok": true, "result": {"id": 1, "is_bot": true, "username": "test_bot"}});
            respond(stream, "200 OK", "", &me.to_string());
        }
        "getWebhookInfo" => {
            let info =
                json!({"ok": true, "result": {"url": st.webhook, "pending_update_count": 3}});
            drop(st);
            respond(stream, "200 OK", "", &info.to_string());
        }
        "deleteWebhook" => {
            st.webhook.clear();
            st.sent.push(json!({"deleteWebhook": body}));
            drop(st);
            respond(stream, "200 OK", "", r#"{"ok":true,"result":true}"#);
        }
        _ => {
            st.sent.push(body);
            let n = st.sent.len();
            drop(st);
            respond(
                stream,
                "200 OK",
                "",
                &json!({"ok": true, "result": {"message_id": 1000 + n}}).to_string(),
            );
        }
    }
}

/// A home on the stand-ins: chats -100 and 42 (the owner) allowed.
fn home(url: &str, tg: &str, extra: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    for d in ["work", "data", "home"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    std::fs::write(
        home.join("ferrule.toml"),
        format!(
            r#"default_provider = "openrouter"

[providers.openrouter]
base_url = "{url}"
api_key_env = "FERRULE_TEST_KEY"
model = "qwen/qwen3.8-27b:free"

[skills]
enabled = false

[sandbox]
mode = "off"

[gateway]
telegram_token_env = "FERRULE_TEST_TG"
telegram_base_url = "{tg}"
telegram_allowed_chats = [-100, 42]
{extra}
"#
        ),
    )
    .unwrap();
    dir
}

fn command(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args(args)
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_KEY", "sk-test")
        .env("FERRULE_TEST_TG", TOKEN)
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
        "NOTIFY_SOCKET",
        "WATCHDOG_USEC",
        "WATCHDOG_PID",
        "RUST_LOG",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

/// Kills the gateway when the test ends, passed or not.
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A gateway with `RUST_LOG` unset, its stderr in `<home>/stderr.log`.
fn gateway(home: &Path) -> Running {
    let log = std::fs::File::create(home.join("stderr.log")).unwrap();
    let child = command(home, &["gateway"])
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    Running(child)
}

fn ferrule(home: &Path, args: &[&str]) -> Output {
    command(home, args).output().unwrap()
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

#[test]
fn a_lasting_409_is_told_once_shown_by_status_and_its_end_too() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    tg.state.lock().unwrap().conflict_until = Some(Instant::now() + Duration::from_secs(4));
    let dir = home(&url, &tg.url, "\n[health]\ntelegram_conflict_secs = 1\n");
    let _gw = gateway(dir.path());

    let (n, told) = tg.wait_for(42, "I'm not getting this bot's messages", 0);
    assert!(
        told.contains("(409 Conflict: \"Conflict: terminated by other getUpdates request; make sure that only one bot instance is running\")"),
        "{told}"
    );
    assert!(
        told.contains(
            "The cause: another program is fetching this bot's messages with the same token"
        ),
        "{told}"
    );
    assert!(told.contains("Less likely: a webhook"), "{told}");
    let (_, back) = tg.wait_for(42, "I'm getting this bot's messages again", n);
    assert!(back.contains("the 409 Conflict cleared"), "{back}");
    let told = tg
        .texts_to(42)
        .iter()
        .filter(|t| t.contains("I'm not getting"))
        .count();
    assert_eq!(told, 1, "told once per episode");

    // The journal has the warning, with RUST_LOG unset.
    let log = std::fs::read_to_string(dir.path().join("stderr.log")).unwrap();
    assert!(log.contains("409 Conflict"), "{log}");
    assert!(!log.contains("SECRET"), "{log}");
}

#[test]
fn status_shows_an_open_conflict() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    let dir = home(&url, &tg.url, "\n[health]\ntelegram_conflict_secs = 1\n");
    let _gw = gateway(dir.path());
    // Up and answering first, so the report can get out.
    tg.say(42, "hello");
    tg.wait_for(42, "PLAIN", 0);
    tg.state.lock().unwrap().conflict_until = Some(Instant::now() + Duration::from_secs(60));
    // The status file is what `ferrule status` prints.
    let file = dir.path().join("data/gateway/status.txt");
    let deadline = Instant::now() + Duration::from_secs(30);
    let report = loop {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        if text.contains("409 Conflict since") {
            break text;
        }
        assert!(Instant::now() < deadline, "{text}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        report.contains("(another program polling with this token, or a webhook)"),
        "{report}"
    );
    let out = ferrule(dir.path(), &["status"]);
    assert!(
        plain(&out.stdout).contains("409 Conflict since"),
        "{}",
        plain(&out.stdout)
    );
}

#[test]
fn a_webhook_is_removed_at_start_and_the_owner_told() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    tg.state.lock().unwrap().webhook = WEBHOOK.into();
    let dir = home(&url, &tg.url, "");

    // Doctor first: it names the webhook's host and the fix, and the
    // `:free` model.
    let out = ferrule(dir.path(), &["doctor"]);
    let doctor = plain(&out.stdout);
    assert!(
        doctor.contains("! telegram  the bot has a webhook set (to hooks.example.com)"),
        "{doctor}"
    );
    assert!(
        doctor.contains("the gateway removes it when it starts"),
        "{doctor}"
    );
    assert!(
        doctor.contains("a `:free` model draws on OpenRouter's shared free pool"),
        "{doctor}"
    );
    assert!(
        doctor.contains("use the paid id `qwen/qwen3.8-27b`"),
        "{doctor}"
    );
    assert!(!doctor.contains("secret-path"), "{doctor}");

    let _gw = gateway(dir.path());
    let (_, told) = tg.wait_for(42, "webhook", 0);
    assert_eq!(
        told,
        "Your bot had a webhook set (to hooks.example.com), so Telegram was sending its messages there instead of to me. I removed it so I can receive them; messages already waiting at Telegram were kept. If another service needs that webhook, it and ferrule can't share this bot token: give one of them its own bot from @BotFather."
    );
    let removed = tg
        .sent()
        .into_iter()
        .find_map(|m| m.get("deleteWebhook").cloned())
        .unwrap();
    assert_eq!(removed["drop_pending_updates"], false, "{removed}");
    tg.say(42, "hello");
    tg.wait_for(42, "PLAIN", 0);
}

#[test]
fn messages_that_arent_text_get_plain_words() {
    let (url, seen) = model_server();
    let tg = FakeTelegram::start();
    let dir = home(&url, &tg.url, "");
    let _gw = gateway(dir.path());

    tg.push(42, json!({"voice": {"file_id": "v1", "duration": 3}}));
    let (n, voice) = tg.wait_for(42, "I got your", 0);
    assert_eq!(
        voice,
        "I got your voice message, but I can only read text for now, so I don't know what's in it. Please type your message instead."
    );
    // An album is one reply.
    for _ in 0..3 {
        tg.push(
            42,
            json!({"photo": [{"file_id": "p"}], "media_group_id": "g1"}),
        );
    }
    tg.say(42, "after the album");
    let (n, _) = tg.wait_for(42, "PLAIN", n);
    let photos = tg
        .texts_to(42)
        .iter()
        .filter(|t| t.starts_with("I got your photo"))
        .count();
    assert_eq!(photos, 1);

    // A caption is the text; the model hears the photo wasn't read.
    tg.push(
        42,
        json!({"photo": [{"file_id": "p2"}], "caption": "what car is this?"}),
    );
    tg.wait_for(42, "PLAIN", n);
    let heard = seen
        .lock()
        .unwrap()
        .iter()
        .flat_map(|r| r["messages"].as_array().cloned().unwrap_or_default())
        .filter_map(|m| m["content"].as_str().map(str::to_string))
        .find(|c| c.contains("what car is this?"))
        .unwrap();
    assert!(
        heard.contains(
            "[The photo attached to this message wasn't read: ferrule reads only text for now.]"
        ),
        "{heard}"
    );
    let log = std::fs::read_to_string(dir.path().join("stderr.log")).unwrap();
    assert!(log.contains("voice"), "logged at info: {log}");
}

#[test]
fn an_ignored_chat_is_a_warning_in_status() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    let dir = home(&url, &tg.url, "");
    let _gw = gateway(dir.path());
    tg.say(777, "hi from a stranger");
    tg.say(777, "again");
    let report = tg.status_until(42, "chat 777");
    assert!(
        report.contains("ignored a message from chat 777: it isn't in telegram_allowed_chats"),
        "{report}"
    );
    assert_eq!(
        report.matches("chat 777").count(),
        1,
        "once an hour: {report}"
    );
    assert!(tg.texts_to(777).is_empty(), "a stranger gets silence");
}

#[test]
fn openrouters_404_and_429_reach_the_chat_in_plain_words() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    let dir = home(&url, &tg.url, "");
    let _gw = gateway(dir.path());

    tg.say(-100, "NOTOOLS please");
    let (_, no_tools) = tg.wait_for(-100, "I couldn't reply", 0);
    assert!(
        no_tools.starts_with("I couldn't reply: this model has no endpoint on OpenRouter that supports tools, and ferrule needs tools. Pick another model (a `:free` one is often the cause)"),
        "{no_tools}"
    );
    assert!(
        no_tools.contains("\n\nThe error: provider error: HTTP 404"),
        "{no_tools}"
    );

    // Retry-After 2, four tries: /status counts the wait down meanwhile.
    tg.say(-100, "LIMIT please");
    let report = tg.status_until(42, "waiting out the model's rate limit");
    assert!(
        report.contains("waiting out the model's rate limit, retry "),
        "{report}"
    );
    let (_, limited) = tg.wait_for(-100, "rate-limiting", 0);
    assert!(
        limited.starts_with("I couldn't reply: the model provider is rate-limiting us (HTTP 429). This is OpenRouter's shared pool for free models"),
        "{limited}"
    );
    assert!(
        limited.contains("I tried 4 times before giving up."),
        "{limited}"
    );
    assert!(limited.contains("free-models-per-min"), "{limited}");
}

#[test]
fn no_log_line_carries_the_token() {
    let (url, _) = model_server();
    // Nothing listens there: every poll fails with an HTTP error.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let dir = home(&url, &format!("http://127.0.0.1:{port}"), "");
    let file = dir.path().join("data/gateway/status.txt");
    {
        let _gw = gateway(dir.path());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let log = std::fs::read_to_string(dir.path().join("stderr.log")).unwrap_or_default();
            if log.contains("telegram: poll failed") && file.exists() {
                break;
            }
            assert!(Instant::now() < deadline, "{log}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let log = std::fs::read_to_string(dir.path().join("stderr.log")).unwrap();
    assert!(log.contains(" WARN "), "warnings reach the journal: {log}");
    assert!(!log.contains('\x1b'), "no color codes in a journal: {log}");
    assert!(!log.contains("SECRET"), "{log}");
    let status = std::fs::read_to_string(&file).unwrap_or_default();
    assert!(!status.contains("SECRET"), "{status}");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn doctor_warns_about_two_gateways() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    let dir = home(&url, &tg.url, "");
    // The second starts once the first is up, as a forgotten terminal would.
    let mut a = gateway(dir.path());
    let marker = dir.path().join("data/gateway/running.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "{:?}", a.0.try_wait());
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut b = gateway(dir.path());
    // A process just spawned may not have exec'd yet, and doctor is slow on
    // a loaded machine: ask until both show.
    let pids = [a.0.id(), b.0.id()];
    let deadline = Instant::now() + Duration::from_secs(90);
    let (line, doctor) = loop {
        let out = ferrule(dir.path(), &["doctor"]);
        let doctor = plain(&out.stdout);
        let line = doctor
            .lines()
            .find(|l| l.contains(" gateway "))
            .unwrap_or_default()
            .to_string();
        if pids.iter().all(|p| line.contains(&p.to_string())) || Instant::now() > deadline {
            break (line, doctor);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    // Other tests' gateways may run too; these two are among them.
    assert!(
        line.contains("running on this machine (pids "),
        "{doctor}\nexited: {:?} {:?}",
        a.0.try_wait(),
        b.0.try_wait()
    );
    for pid in pids {
        assert!(line.contains(&pid.to_string()), "{pid}: {doctor}");
    }
    assert!(line.contains("409 Conflict"), "{doctor}");
}
