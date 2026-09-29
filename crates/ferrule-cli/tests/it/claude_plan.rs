//! M35's Claude plan through the real `ferrule` binary, with the stand-in
//! `claude` (`ferrule-fake-claude`, never shipped) and a temp config dir:
//! `ferrule login|logout claude` (a piped setup-token, claude's own login,
//! an exported token), `ferrule status` and doctor; a turn and its resume,
//! the ledger at $0 with the notional price, and the token nowhere on disk
//! but sealed; claude's permission prompts answered by M19, with nobody to
//! ask and by the owner in Telegram; a rejected usage limit falling back
//! and the owner told when it resets.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const TOKEN: &str = "sk-ant-oat01-TESTSECRETTOKEN";

/// The stand-in `claude`: next to `ferrule` when the workspace's tests
/// built it (ferrule-plans' own tests need it), else built now.
fn fake_claude() -> PathBuf {
    static FAKE: OnceLock<PathBuf> = OnceLock::new();
    FAKE.get_or_init(|| {
        let exe = Path::new(env!("CARGO_BIN_EXE_ferrule"));
        let fake = exe.with_file_name(format!(
            "ferrule-fake-claude{}",
            std::env::consts::EXE_SUFFIX
        ));
        if !fake.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let mut cmd = Command::new(cargo);
            cmd.args([
                "build",
                "-p",
                "ferrule-plans",
                "--bin",
                "ferrule-fake-claude",
            ]);
            if exe.parent().and_then(Path::file_name) == Some("release".as_ref()) {
                cmd.arg("--release");
            }
            let status = cmd.status().expect("running cargo build");
            assert!(status.success(), "building ferrule-fake-claude");
        }
        assert!(fake.exists(), "{} not found", fake.display());
        fake
    })
    .clone()
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

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        plain(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `[plans.claude_code]` on the fake, the sandbox off (the tests run
/// anywhere), and `extra` appended.
fn config(default: &str, extra: &str) -> String {
    format!(
        r#"default_provider = "{default}"

[providers.a]
base_url = "http://127.0.0.1:9/v1"
api_key_env = "FERRULE_TEST_KEY"
model = "a-one"

[skills]
enabled = false

[sandbox]
mode = "off"

[plans.claude_code]
binary = '{}'
{extra}
"#,
        fake_claude().display()
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
        .env("FERRULE_TEST_TG", "TESTTOKEN")
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
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CONFIG_DIR",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn ferrule(home: &Path, args: &[&str]) -> Output {
    command(home, args).output().unwrap()
}

/// `ferrule login claude --token`, the token piped in.
fn paste(home: &Path, token: &str) -> Output {
    let mut child = command(home, &["login", "claude", "--token"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{token}").unwrap();
    child.wait_with_output().unwrap()
}

fn jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Every file under `dir` that has `needle` in it.
fn files_with(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if std::fs::read(&p)
                .map(|b| String::from_utf8_lossy(&b).contains(needle))
                .unwrap_or(false)
            {
                found.push(p);
            }
        }
    }
    found
}

/// What the fake recorded of each turn's launch.
fn fake_calls(home: &Path) -> Vec<Value> {
    let dir = home.join("data/claude-code/fake");
    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("call-"))
        })
        .collect();
    names.sort();
    names
        .iter()
        .map(|p| serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap())
        .collect()
}

#[test]
fn login_status_doctor_and_logout_for_each_way_of_signing_in() {
    let dir = home(&config("a", ""));
    let home = dir.path();

    // Claude's own sign-in needs a terminal; a key isn't a setup-token.
    let out = ferrule(home, &["login", "claude"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("needs a terminal"),
        "{}",
        describe(&out)
    );
    let out = paste(home, "sk-ant-api03-NOTATOKEN");
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("API key"));
    let out = ferrule(home, &["login", "chatgpt", "--token"]);
    assert!(!out.status.success(), "{}", describe(&out));

    // A pasted setup-token: sealed, and said to be stored.
    let out = paste(home, TOKEN);
    assert!(out.status.success(), "{}", describe(&out));
    let said = plain(&out.stdout);
    assert!(said.contains("Saved the setup-token, sealed"), "{said}");
    assert!(said.contains("stores a Claude credential"), "{said}");
    let toml = std::fs::read_to_string(home.join("ferrule.toml")).unwrap();
    assert!(toml.contains("[providers.claude-code]"), "{toml}");
    assert!(toml.contains("plan = \"claude-code\""), "{toml}");
    let sealed = home.join("data/private/plans/claude-code.json");
    assert!(sealed.exists());
    assert!(files_with(home, "TESTSECRET").is_empty());

    let out = ferrule(home, &["status"]);
    let status = plain(&out.stdout);
    assert!(
        status.contains("claude-code: signed in"),
        "{}",
        describe(&out)
    );

    let out = ferrule(home, &["doctor", "--offline"]);
    let doctor = plain(&out.stdout) + &String::from_utf8_lossy(&out.stderr);
    assert!(doctor.contains("Claude Code 2.1.283"), "{doctor}");
    assert!(doctor.contains("a setup-token pasted"), "{doctor}");
    assert!(doctor.contains("days left"), "{doctor}");

    // An API key in the environment would bill a hand-run claude: doctor
    // says so, and the engine's claude never sees it.
    let out = command(home, &["doctor", "--offline"])
        .env("ANTHROPIC_API_KEY", "sk-ant-api03-OUTRANKS")
        .output()
        .unwrap();
    let doctor = plain(&out.stdout) + &String::from_utf8_lossy(&out.stderr);
    assert!(doctor.contains("ANTHROPIC_API_KEY"), "{doctor}");

    // A subscription token as some provider's API key is refused.
    let out = command(home, &["doctor", "--offline"])
        .env("FERRULE_TEST_KEY", TOKEN)
        .output()
        .unwrap();
    let doctor = plain(&out.stdout) + &String::from_utf8_lossy(&out.stderr);
    assert!(
        doctor.contains("is a Claude subscription token"),
        "{doctor}"
    );
    assert!(!doctor.contains("TESTSECRET"), "{doctor}");

    // Logout deletes the token and signs claude out of ferrule's dir.
    let out = ferrule(home, &["logout", "claude"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(!sealed.exists());
    let out = ferrule(home, &["status"]);
    assert!(
        plain(&out.stdout).contains("claude-code: not signed in"),
        "{}",
        describe(&out)
    );

    // Claude's own login (the fake keeps it as a file), asked of claude.
    std::fs::write(home.join("data/claude-code/fake/logged-in"), "").unwrap();
    let out = ferrule(home, &["status"]);
    let status = plain(&out.stdout);
    assert!(
        status.contains("signed in as owner@example.com (max)"),
        "{}",
        describe(&out)
    );
    let out = ferrule(home, &["logout", "claude"]);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(!home.join("data/claude-code/fake/logged-in").exists());

    // An exported token goes first, and ferrule doesn't store it.
    let out = command(home, &["doctor", "--offline"])
        .env("CLAUDE_CODE_OAUTH_TOKEN", TOKEN)
        .output()
        .unwrap();
    let doctor = plain(&out.stdout) + &String::from_utf8_lossy(&out.stderr);
    assert!(
        doctor.contains("CLAUDE_CODE_OAUTH_TOKEN from the environment"),
        "{doctor}"
    );
    assert!(!sealed.exists());
    assert!(files_with(home, "TESTSECRET").is_empty());
}

#[test]
fn a_run_goes_through_claude_is_ledgered_at_zero_and_a_gated_command_is_refused() {
    let dir = home(&config(
        "claude-code",
        "\n[providers.claude-code]\nplan = \"claude-code\"\nmodel = \"haiku\"\n",
    ));
    let home = dir.path();
    assert!(paste(home, TOKEN).status.success());

    let out = command(home, &["run", "hello there"])
        .env("ANTHROPIC_API_KEY", "sk-ant-api03-OUTRANKS")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        plain(&out.stdout).contains("you said hello there"),
        "{}",
        describe(&out)
    );

    let calls = fake_calls(home);
    assert_eq!(calls.len(), 1, "{calls:#?}");
    let call = &calls[0];
    let args: Vec<&str> = call["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert!(!args.contains(&"--bare"), "{args:?}");
    assert!(args.contains(&"--strict-mcp-config"), "{args:?}");
    assert_eq!(call["anthropic_api_key"], false, "stripped");
    assert_eq!(call["anthropic_base_url"], false, "stripped");
    assert!(call["token_sha"].is_string(), "the child got the token");
    assert!(
        call["prompt"].as_str().unwrap().contains("hello there"),
        "the prompt went on stdin"
    );

    let rows = jsonl(&home.join("data/ledger.jsonl"));
    let row = rows
        .iter()
        .find(|r| r["provider"] == "claude-code")
        .unwrap_or_else(|| panic!("{rows:#?}"));
    assert_eq!(row["plan"], "claude-code", "{row:#}");
    assert_eq!(row["cost_usd"], json!(0.0), "{row:#}");
    assert_eq!(row["notional_usd"], json!(0.0123), "{row:#}");

    // claude asks ferrule about each tool: `rm -rf` needs the owner, and
    // a run has nobody to ask.
    let out = ferrule(home, &["run", "[approve]"]);
    assert!(out.status.success(), "{}", describe(&out));
    let said = plain(&out.stdout);
    let rm = said.split("rm=").nth(1).unwrap_or_default();
    assert!(said.contains("ls={\"behavior\":\"allow\""), "{said}");
    assert!(rm.starts_with("{\"behavior\":\"deny\""), "{said}");
    assert!(said.contains("ours={\"behavior\":\"allow\""), "{said}");
    let audit = jsonl(&home.join("data/trust/audit.jsonl"));
    assert!(
        audit.iter().any(|e| e["event"] == "approval_answered"),
        "{audit:#?}"
    );

    // The token is sealed and nowhere else: not the ledger, the
    // transcripts, the audit or the fake's own records.
    assert!(
        files_with(home, "TESTSECRET").is_empty(),
        "{:?}",
        files_with(home, "TESTSECRET")
    );
}

// ── The gateway: resume, the owner approving, a limit falling back ─────

fn answer(text: &str) -> Value {
    json!({
        "choices": [{
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    })
}

/// An OpenAI-compatible server that says `B:<model>`.
fn model_server() -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let calls: Arc<Mutex<usize>> = Arc::default();
    let seen = calls.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = seen.clone();
            std::thread::spawn(move || {
                let Some(body) = read_request(&stream).1 else {
                    return;
                };
                *seen.lock().unwrap() += 1;
                let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let model = req["model"].as_str().unwrap_or_default();
                respond(stream, &answer(&format!("B:{model}")).to_string());
            });
        }
    });
    (url, calls)
}

fn read_request(stream: &TcpStream) -> (String, Option<Vec<u8>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).unwrap_or(0) == 0 {
        return (request, None);
    }
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return (request, None);
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
    (request, Some(body))
}

fn respond(mut stream: TcpStream, out: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
        out.len()
    );
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
                std::thread::spawn(move || {
                    let (request, Some(body)) = read_request(&stream) else {
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
        self.queue.lock().unwrap().push_back(json!({
            "update_id": *next,
            "message": {"message_id": *next, "chat": {"id": chat},
                        "from": {"id": chat, "username": format!("u{chat}")},
                        "text": text, "date": 1700000000},
        }));
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
}

/// Kills the gateway when dropped, passed or not.
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn in_the_gateway_a_chat_resumes_the_owner_approves_and_a_spent_limit_falls_back() {
    let (url, b_calls) = model_server();
    let tg = FakeTelegram::start();
    let extra = format!(
        r#"
[providers.claude-code]
plan = "claude-code"
model = "haiku"

[providers.b]
base_url = "{url}"
api_key_env = "FERRULE_TEST_KEY"
model = "b-large"

[models]
fallback = ["b"]

[gateway]
telegram_token_env = "FERRULE_TEST_TG"
telegram_base_url = "{}"
telegram_allowed_chats = [42]
"#,
        tg.url
    );
    let dir = home(&config("claude-code", &extra));
    let home = dir.path();
    assert!(paste(home, TOKEN).status.success());
    let mut cmd = command(home, &["gateway"]);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let _gw = Running(cmd.spawn().unwrap());

    // The second turn resumes the first's claude session.
    tg.say(42, "hello");
    let (n, _) = tg.wait_for(42, "turn 1: you said hello", 0);
    tg.say(42, "again");
    let (n, _) = tg.wait_for(42, "turn 2: you said again", n);
    let calls = fake_calls(home);
    let second: Vec<&str> = calls[1]["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert!(second.contains(&"--resume"), "{second:?}");

    // claude's permission prompt reaches the owner through M19; yes lets
    // the command run.
    tg.say(42, "[approve]");
    let (n, question) = tg.wait_for(42, "Reply `yes` to allow it", n);
    assert!(question.contains("rm -rf /x"), "{question}");
    tg.say(42, "yes");
    let (n, said) = tg.wait_for(42, "ours=", n);
    assert!(said.contains("rm={\"behavior\":\"allow\""), "{said}");

    // A spent limit: b answers, and the owner hears when it resets.
    assert_eq!(*b_calls.lock().unwrap(), 0);
    tg.say(42, "[rejected] one more");
    let (_, told) = tg.wait_for(42, "answered instead", n);
    assert!(told.contains("usage limit"), "{told}");
    assert!(told.contains("resets"), "{told}");
    tg.wait_for(42, "B:b-large", n);
    assert!(*b_calls.lock().unwrap() >= 1);

    assert!(
        files_with(home, "TESTSECRET").is_empty(),
        "{:?}",
        files_with(home, "TESTSECRET")
    );
}

// ── Live: the real claude binary, the sandbox on ─────────────────────────

/// Two chat turns through the real `claude` named by
/// `FERRULE_LIVE_CLAUDE`, signed in by whatever `CLAUDE_CODE_OAUTH_TOKEN`
/// the caller exported, with ferrule's sandbox in its default mode and
/// the caller's proxy kept. Spends two short Haiku turns on the plan.
#[test]
#[ignore = "live: spends two turns on a Claude plan; set FERRULE_LIVE_CLAUDE"]
fn live_two_chat_turns_through_the_real_claude_with_the_sandbox_on() {
    let Ok(binary) = std::env::var("FERRULE_LIVE_CLAUDE") else {
        eprintln!("FERRULE_LIVE_CLAUDE not set; skipping");
        return;
    };
    let dir = home(&format!(
        r#"default_provider = "claude-code"

[providers.claude-code]
plan = "claude-code"
model = "haiku"

[skills]
enabled = false

[plans.claude_code]
binary = '{binary}'
"#
    ));
    let home = dir.path();
    let mut cmd = command(home, &["chat"]);
    // The caller's proxy and sign-in, which `command` drops for the
    // hermetic tests.
    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ] {
        if let Some(v) = std::env::var_os(var) {
            cmd.env(var, v);
        }
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(
            stdin,
            "Remember the word ferrule-otter. Reply with just: noted."
        )
        .unwrap();
        writeln!(stdin, "Which word did I ask you to remember? One word.").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let said = plain(&out.stdout);
    let transcript = format!("── stdout ──\n{said}\n── stderr ──\n{}", plain(&out.stderr));
    println!("{transcript}");
    // Kept for a report: the transcript and the ledger, and the home.
    let keep = std::env::var("FERRULE_LIVE_TRANSCRIPT").ok();
    if let Some(path) = &keep {
        std::fs::write(path, &transcript).unwrap();
        let _ = std::fs::write(
            format!("{path}.ledger.jsonl"),
            std::fs::read(home.join("data/ledger.jsonl")).unwrap_or_default(),
        );
        let _ = std::fs::write(format!("{path}.home"), home.display().to_string());
    }
    assert!(out.status.success(), "{}", describe(&out));
    assert!(said.to_lowercase().contains("otter"), "resumed: {said}");
    let rows = jsonl(&home.join("data/ledger.jsonl"));
    let rows: Vec<_> = rows
        .iter()
        .filter(|r| r["provider"] == "claude-code")
        .collect();
    // One row per model call: a turn in which claude runs one of
    // ferrule's own tools over the bridge pauses and resumes, so more.
    assert!(rows.len() >= 2, "{rows:#?}");
    for row in rows {
        assert_eq!(row["plan"], "claude-code", "{row:#}");
        assert_eq!(row["cost_usd"], json!(0.0), "{row:#}");
    }
    if keep.is_some() {
        std::mem::forget(dir);
    }
}
