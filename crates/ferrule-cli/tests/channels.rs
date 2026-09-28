//! M31 through the real `ferrule` binary: one gateway runs Telegram,
//! Discord and Slack at once, against a fake Telegram and the gateway
//! crate's Discord and Slack mocks. Each answers its own allowed user,
//! strangers never reach the model, and a dead Discord socket shows up in
//! `/status` and `ferrule status` while the other two keep answering.

#[path = "../../ferrule-gateway/tests/support/mod.rs"]
mod support;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{discord, email, matrix, signal, slack, wait, whatsapp};

const LIMIT: Duration = Duration::from_secs(30);

/// Echoes the last user message, so each channel's answer is its own.
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
            std::thread::spawn(move || serve_model(stream, &log));
        }
    });
    (url, seen)
}

fn read_request(stream: &TcpStream) -> Option<(String, Vec<u8>)> {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).unwrap_or(0) == 0 {
        return None;
    }
    let mut length = 0;
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
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some((request, body))
}

fn respond(mut stream: TcpStream, out: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
        out.len()
    );
}

fn serve_model(stream: TcpStream, log: &Mutex<Vec<Value>>) {
    let Some((_, body)) = read_request(&stream) else {
        return;
    };
    let req: Value = serde_json::from_slice(&body).unwrap();
    let last = req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string();
    log.lock().unwrap().push(req);
    let out = json!({
        "choices": [{
            "message": {"role": "assistant", "content": format!("ECHO {last}")},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    });
    respond(stream, &out.to_string());
}

fn asked(log: &Mutex<Vec<Value>>, needle: &str) -> bool {
    log.lock()
        .unwrap()
        .iter()
        .any(|r| r["messages"].to_string().contains(needle))
}

struct FakeTelegram {
    url: String,
    queue: Arc<Mutex<Vec<Value>>>,
    sent: Arc<Mutex<Vec<Value>>>,
    next: Mutex<i64>,
}

impl FakeTelegram {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let queue: Arc<Mutex<Vec<Value>>> = Arc::default();
        let sent: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (q, s) = (queue.clone(), sent.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (q, s) = (q.clone(), s.clone());
                std::thread::spawn(move || {
                    let Some((request, body)) = read_request(&stream) else {
                        return;
                    };
                    let out = if request.contains("getUpdates") {
                        let mut updates: Vec<Value> = q.lock().unwrap().drain(..).collect();
                        if updates.is_empty() {
                            std::thread::sleep(Duration::from_millis(100));
                            updates = q.lock().unwrap().drain(..).collect();
                        }
                        json!({"ok": true, "result": updates})
                    } else {
                        let mut sent = s.lock().unwrap();
                        sent.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                        json!({"ok": true, "result": {"message_id": 1000 + sent.len()}})
                    };
                    respond(stream, &out.to_string());
                });
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
        self.queue.lock().unwrap().push(json!({
            "update_id": *next,
            "message": {"message_id": *next, "chat": {"id": chat}, "from": {"username": "max"},
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

fn discord_texts(d: &discord::Discord, channel: &str) -> Vec<String> {
    d.state()
        .messages()
        .into_iter()
        .filter(|(c, _)| c == channel)
        .filter_map(|(_, m)| m["content"].as_str().map(String::from))
        .collect()
}

fn slack_texts(s: &slack::Slack, channel: &str) -> Vec<String> {
    s.state()
        .posts()
        .into_iter()
        .filter(|p| p["channel"] == channel)
        .filter_map(|p| p["text"].as_str().map(String::from))
        .collect()
}

/// A home with the echoing provider and all three channels. Telegram
/// allows chat 42, Discord user 111, Slack member U111.
fn home(url: &str, tg: &FakeTelegram, d: &discord::Discord, s: &slack::Slack) -> tempfile::TempDir {
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
discord_token_env = "FERRULE_TEST_DISCORD"
discord_api_url = "{d}"
discord_allowed_users = ["111"]
discord_stream = false
slack_bot_token_env = "FERRULE_TEST_SLACK_BOT"
slack_app_token_env = "FERRULE_TEST_SLACK_APP"
slack_api_url = "{s}"
slack_allowed_users = ["U111"]
slack_stream = false
"#,
            tg = tg.url,
            d = d.api,
            s = s.api,
        ),
    )
    .unwrap();
    dir
}

/// Kills the gateway when the test ends, passed or not.
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(home: &Path, args: &[&str], discord_token: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args(args)
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_KEY", "sk-test")
        .env("FERRULE_TEST_TG", "TESTTOKEN")
        .env("FERRULE_TEST_DISCORD", discord_token)
        .env("FERRULE_TEST_SLACK_BOT", slack::BOT_TOKEN)
        .env("FERRULE_TEST_SLACK_APP", slack::APP_TOKEN)
        .stdin(std::process::Stdio::null());
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
    cmd
}

fn gateway(home: &Path, discord_token: &str) -> Running {
    let mut cmd = command(home, &["gateway"], discord_token);
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    Running(cmd.spawn().unwrap())
}

fn any_has(texts: &[String], needle: &str) -> bool {
    texts.iter().any(|t| t.contains(needle))
}

#[test]
fn one_gateway_answers_telegram_discord_and_slack_and_only_their_allowed_users() {
    let (url, log) = model_server();
    let tg = FakeTelegram::start();
    let d = discord::Discord::start();
    let s = slack::Slack::start();
    let dir = home(&url, &tg, &d, &s);
    let _gw = gateway(dir.path(), discord::TOKEN);

    wait("the Discord handshake", LIMIT, || d.state().readies >= 1);
    wait("the Slack socket", LIMIT, || s.state().connections >= 1);

    tg.say(42, "hello from telegram");
    d.dm("m1", "111", "hello from discord");
    s.event(
        "e1",
        slack::dm("U111", "1700000000.000100", "hello from slack"),
    );

    wait("the Telegram answer", LIMIT, || {
        any_has(&tg.texts_to(42), "ECHO hello from telegram")
    });
    wait("the Discord answer", LIMIT, || {
        any_has(&discord_texts(&d, "7111"), "ECHO hello from discord")
    });
    wait("the Slack answer", LIMIT, || {
        any_has(&slack_texts(&s, "DU111"), "ECHO hello from slack")
    });
    // Each answer went to its own channel only.
    assert!(!any_has(&discord_texts(&d, "7111"), "telegram"));
    assert!(!any_has(&slack_texts(&s, "DU111"), "discord"));
    assert!(!any_has(&tg.texts_to(42), "slack"));

    // Strangers: dropped before the model, on both new channels.
    d.dm("m2", "222", "STRANGER-DISCORD");
    s.event(
        "e2",
        slack::dm("U222", "1700000000.000200", "STRANGER-SLACK"),
    );
    // A later allowed message is the barrier: the strangers came first.
    d.dm("m3", "111", "after the stranger");
    s.event(
        "e3",
        slack::dm("U111", "1700000000.000300", "after the stranger"),
    );
    wait("the later Discord answer", LIMIT, || {
        any_has(&discord_texts(&d, "7111"), "ECHO after the stranger")
    });
    wait("the later Slack answer", LIMIT, || {
        any_has(&slack_texts(&s, "DU111"), "ECHO after the stranger")
    });
    assert!(!asked(&log, "STRANGER-DISCORD"));
    assert!(!asked(&log, "STRANGER-SLACK"));
    assert!(discord_texts(&d, "7222").is_empty());
    assert!(slack_texts(&s, "DU222").is_empty());

    // /status lists all three, from any of them.
    d.dm("m4", "111", "/status");
    wait("the Discord /status", LIMIT, || {
        any_has(&discord_texts(&d, "7111"), "channels:")
    });
    let report = discord_texts(&d, "7111")
        .into_iter()
        .find(|t| t.contains("channels:"))
        .unwrap();
    for part in [
        "telegram: last ok poll",
        "discord: last ok poll",
        "slack: last ok poll",
    ] {
        assert!(report.contains(part), "{part} missing from {report}");
    }
    for secret in [
        discord::TOKEN,
        slack::BOT_TOKEN,
        slack::APP_TOKEN,
        "TESTTOKEN",
    ] {
        assert!(!report.contains(secret), "{report}");
    }
}

#[test]
fn a_dead_discord_socket_shows_in_status_while_telegram_and_slack_keep_answering() {
    let (url, _) = model_server();
    let tg = FakeTelegram::start();
    let d = discord::Discord::start();
    let s = slack::Slack::start();
    let dir = home(&url, &tg, &d, &s);
    // A token the mock rejects: 401 on the gateway lookup, a fatal stop.
    let bad = "MTExMTExMTExMTEx.GbAdTk.not-the-mock-token-000000000000";
    let _gw = gateway(dir.path(), bad);

    wait("the Slack socket", LIMIT, || s.state().connections >= 1);
    tg.say(42, "still here?");
    s.event("e1", slack::dm("U111", "1700000000.000100", "still here?"));
    wait("the Telegram answer", LIMIT, || {
        any_has(&tg.texts_to(42), "ECHO still here?")
    });
    wait("the Slack answer", LIMIT, || {
        any_has(&slack_texts(&s, "DU111"), "ECHO still here?")
    });

    tg.say(42, "/status");
    wait(
        "the Telegram /status naming Discord's problem",
        LIMIT,
        || any_has(&tg.texts_to(42), "rejected the bot token"),
    );
    let report = tg
        .texts_to(42)
        .into_iter()
        .find(|t| t.contains("channels:"))
        .unwrap();
    assert!(report.contains("discord: no ok poll yet"), "{report}");
    assert!(!report.contains(bad), "{report}");

    // The same on the machine.
    let mut status = String::new();
    wait("status.txt with the problem", LIMIT, || {
        let out = command(dir.path(), &["status"], bad).output().unwrap();
        status = String::from_utf8_lossy(&out.stdout).into_owned();
        status.contains("rejected the bot token")
    });
    assert!(!status.contains(bad), "{status}");
}

/// M39: WhatsApp through the real binary: webhooks from the relay's
/// mailbox, answers through the Graph API mock, a stranger never reaching
/// the model, and the doctor's line.
#[test]
fn whatsapp_through_the_relay_mailbox_answers_only_its_allowed_number() {
    let (url, log) = model_server();
    let meta = whatsapp::start();
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

[connections]
relay_url = "{relay}"

[gateway.whatsapp]
phone_number_id = "{phone}"
api_url = "{relay}"
allowed_users = ["{max}"]
"#,
            relay = meta.url,
            phone = whatsapp::PHONE_ID,
            max = whatsapp::MAX,
        ),
    )
    .unwrap();
    let with_keys = |mut cmd: Command| {
        cmd.env("WHATSAPP_TOKEN", whatsapp::TOKEN)
            .env("WHATSAPP_APP_SECRET", whatsapp::APP_SECRET)
            .env("WHATSAPP_VERIFY_TOKEN", whatsapp::VERIFY)
            .env("FERRULE_RELAY_KEY", whatsapp::RELAY_KEY);
        cmd
    };
    let mut cmd = with_keys(command(home, &["gateway"], discord::TOKEN));
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _gw = Running(cmd.spawn().unwrap());

    wait("the mailbox set up", LIMIT, || {
        meta.state().config.is_some()
    });
    assert_eq!(
        meta.state().config,
        Some((whatsapp::VERIFY.into(), whatsapp::APP_SECRET.into()))
    );
    meta.deliver(
        &whatsapp::text("wamid.s1", "15550000999", "STRANGER-WA"),
        whatsapp::APP_SECRET,
    );
    meta.deliver(
        &whatsapp::text("wamid.m1", whatsapp::MAX, "hello from whatsapp"),
        whatsapp::APP_SECRET,
    );
    wait("the WhatsApp answer", LIMIT, || {
        any_has(&meta.texts(), "ECHO hello from whatsapp")
    });
    assert!(!asked(&log, "STRANGER-WA"));
    // Every send went to the allowed number; nothing to the stranger.
    for b in meta.state().sent.iter() {
        if let Some(to) = b["to"].as_str() {
            assert_eq!(to, whatsapp::MAX, "{b}");
        }
    }

    let o = with_keys(command(
        home,
        &["doctor", "--offline", "--json"],
        discord::TOKEN,
    ))
    .output()
    .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let report: Value = serde_json::from_str(out.lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {out}"));
    let wa: Vec<&Value> = report["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["what"] == "whatsapp")
        .collect();
    assert!(
        wa.iter().any(
            |l| l["level"] == "ok" && l["text"].as_str().unwrap().contains("through the relay")
        ),
        "{wa:?}"
    );
    assert!(!out.contains(whatsapp::TOKEN), "{out}");
}

#[test]
fn matrix_answers_its_allowed_user_in_a_dm_and_the_doctor_reads_it() {
    let (url, log) = model_server();
    let hs = matrix::start();
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

[gateway.matrix]
homeserver = "{hs}"
access_token_env = "MATRIX_ACCESS_TOKEN"
user = "{bot}"
allowed_users = ["{max}"]
stream = false
"#,
            hs = hs.url,
            bot = matrix::BOT,
            max = matrix::MAX,
        ),
    )
    .unwrap();
    let with_token = |mut cmd: Command| {
        cmd.env("MATRIX_ACCESS_TOKEN", matrix::TOKEN);
        cmd
    };
    let mut cmd = with_token(command(home, &["gateway"], discord::TOKEN));
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _gw = Running(cmd.spawn().unwrap());

    wait("the first long-poll", LIMIT, || {
        hs.state().sinces.iter().any(Option::is_some)
    });
    hs.timeline(
        matrix::DM,
        vec![
            matrix::text("$s1", matrix::STRANGER, "STRANGER-MX"),
            matrix::text("$m1", matrix::MAX, "hello from matrix"),
        ],
    );
    wait("the Matrix answer", LIMIT, || {
        any_has(&hs.bodies(matrix::DM), "ECHO hello from matrix")
    });
    assert!(!asked(&log, "STRANGER-MX"));
    // The sync position is the instance's own.
    assert!(home.join("data/gateway/matrix/state.json").exists());

    let o = with_token(command(
        home,
        &["doctor", "--offline", "--json"],
        discord::TOKEN,
    ))
    .output()
    .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let report: Value = serde_json::from_str(out.lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {out}"));
    let mx: Vec<&Value> = report["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["what"] == "matrix")
        .collect();
    assert!(
        mx.iter()
            .any(|l| l["level"] == "ok" && l["text"].as_str().unwrap().contains("1 user(s)")),
        "{mx:?}"
    );
    assert!(!out.contains(matrix::TOKEN), "{out}");
}

#[test]
fn email_answers_its_allowed_sender_in_the_thread_and_the_doctor_reads_it() {
    let (url, log) = model_server();
    let p = email::Provider::start(true);
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

[gateway.email]
address = "{bot}"
imap_host = "127.0.0.1"
imap_port = {imap}
smtp_host = "127.0.0.1"
smtp_port = {smtp}
password_env = "EMAIL_PASSWORD"
allowed_senders = ["{max}"]
"#,
            bot = email::ADDRESS,
            max = email::MAX,
            imap = p.imap_port,
            smtp = p.smtp_port,
        ),
    )
    .unwrap();
    let with_password = |mut cmd: Command| {
        cmd.env("EMAIL_PASSWORD", email::PASSWORD);
        cmd
    };
    let mut cmd = with_password(command(home, &["gateway"], discord::TOKEN));
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _gw = Running(cmd.spawn().unwrap());

    wait("the first IDLE", LIMIT, || p.state().idles > 0);
    let stranger = p.deliver(email::mail("someone@else.test", "hi", "STRANGER-MAIL", &[]));
    let max = p.deliver(email::mail(
        email::MAX,
        "a question",
        "hello by mail",
        &[("Message-ID", "<q1@example.com>")],
    ));
    wait("the email answer", LIMIT, || p.sent() > 0);
    let data = p.sent_data(0);
    let reply = ferrule_connections::native::mime::parse(data.as_bytes());
    assert!(reply.text.contains("ECHO"), "{data}");
    assert!(reply.text.contains("hello by mail"), "{data}");
    assert!(data.contains("In-Reply-To: <q1@example.com>"), "{data}");
    assert!(data.contains("Subject: Re: a question"), "{data}");
    assert!(!asked(&log, "STRANGER-MAIL"));
    assert!(p.seen(max));
    assert!(!p.seen(stranger), "a stranger's mail stays unread");
    // The last UID seen is the instance's own.
    assert!(home.join("data/gateway/email/state.json").exists());

    let o = with_password(command(
        home,
        &["doctor", "--offline", "--json"],
        discord::TOKEN,
    ))
    .output()
    .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let report: Value = serde_json::from_str(out.lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {out}"));
    let lines: Vec<&Value> = report["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["what"] == "email")
        .collect();
    assert!(
        lines
            .iter()
            .any(|l| l["level"] == "ok"
                && l["text"].as_str().unwrap().contains("1 sender(s) allowed")),
        "{lines:?}"
    );
    assert!(!out.contains(email::PASSWORD), "{out}");
}

#[test]
fn signal_answers_its_allowed_number_through_the_daemon_and_the_doctor_reads_it() {
    let (url, log) = model_server();
    let d = signal::start();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    for sub in ["work", "data", "home"] {
        std::fs::create_dir_all(home.join(sub)).unwrap();
    }
    let base = format!(
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
    );
    std::fs::write(
        home.join("ferrule.toml"),
        format!(
            r#"{base}
[gateway.signal]
account = "{acct}"
url = "{daemon}"
allowed_users = ["{max}"]
"#,
            acct = signal::ACCOUNT,
            daemon = d.url,
            max = signal::MAX,
        ),
    )
    .unwrap();
    let mut cmd = command(home, &["gateway"], discord::TOKEN);
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _gw = Running(cmd.spawn().unwrap());

    wait("the event stream", LIMIT, || d.state().open > 0);
    d.push(signal::dm(
        signal::STRANGER,
        "STRANGER-SIG",
        1_760_000_000_001,
    ));
    d.push(signal::dm(
        signal::MAX,
        "hello from signal",
        1_760_000_000_002,
    ));
    wait("the Signal answer", LIMIT, || {
        d.calls("send").iter().any(|p| {
            p["message"]
                .as_str()
                .is_some_and(|m| m.contains("ECHO hello from signal"))
        })
    });
    assert!(!asked(&log, "STRANGER-SIG"));
    let sent = d.calls("send");
    assert!(
        sent.iter()
            .all(|p| p["recipient"][0].as_str() != Some(signal::STRANGER)),
        "{sent:?}"
    );

    let doctor = |home: &Path, offline: bool| -> Vec<Value> {
        let args: &[&str] = if offline {
            &["doctor", "--offline", "--json"]
        } else {
            &["doctor", "--json"]
        };
        let o = command(home, args, discord::TOKEN).output().unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        let report: Value = serde_json::from_str(out.lines().last().unwrap_or_default())
            .unwrap_or_else(|e| panic!("{e}: {out}"));
        report["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["what"] == "signal")
            .cloned()
            .collect()
    };
    let lines = doctor(home, true);
    assert!(
        lines.iter().any(|l| l["level"] == "ok"
            && l["text"]
                .as_str()
                .unwrap()
                .contains("1 user(s), 0 group(s)")),
        "{lines:?}"
    );

    // Without a URL ferrule starts signal-cli itself: doctor says when
    // there's none to start.
    let other = tempfile::tempdir().unwrap();
    let home2 = other.path();
    for sub in ["work", "data", "home"] {
        std::fs::create_dir_all(home2.join(sub)).unwrap();
    }
    let missing = home2.join("no-signal-cli");
    std::fs::write(
        home2.join("ferrule.toml"),
        format!(
            "{base}\n[gateway.signal]\naccount = \"{}\"\nsignal_cli = {:?}\nport = {}\n",
            signal::ACCOUNT,
            missing.display().to_string(),
            support::bind().1,
        ),
    )
    .unwrap();
    let lines = doctor(home2, true);
    assert!(
        lines.iter().any(
            |l| l["level"] == "fail" && l["text"].as_str().unwrap().contains("isn't installed")
        ),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l["level"] == "warn"),
        "no one allowed: {lines:?}"
    );
}
