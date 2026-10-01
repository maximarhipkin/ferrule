//! M42 part 7 session tree, through the real `ferrule` binary: a run's
//! session is listed, a chat forks it with the fold-applied prefix, and
//! the branch names its parent. Unix only: shared shell helpers.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

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
    let script: Arc<dyn Fn(&Value) -> Value + Send + Sync> = Arc::new(|_: &Value| answer("done"));
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

fn serve(
    mut stream: TcpStream,
    log: &Mutex<Vec<Value>>,
    script: &Arc<dyn Fn(&Value) -> Value + Send + Sync>,
) {
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

fn home(url: &str) -> tempfile::TempDir {
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

#[test]
fn a_session_is_listed_then_forked_and_the_branch_names_its_parent() {
    let (url, _) = model_server();
    let dir = home(&url);
    let home = dir.path();

    let out = ferrule(home, &["run", "summarise this folder"]);
    assert!(out.status.success(), "{}", plain(&out.stderr));

    // The session is listed with its first line.
    let out = ferrule(home, &["sessions"]);
    let stdout = plain(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    let line = stdout
        .lines()
        .find(|l| l.contains("summarise this folder"))
        .unwrap_or_else(|| panic!("the run's session is listed:\n{stdout}"));
    let sid = line.split_whitespace().next().unwrap().to_string();

    // Fork it at two messages (goal + answer); stdin is closed, so the
    // chat exits after starting.
    let out = ferrule(home, &["chat", "--fork", &sid, "--at", "2"]);
    let stdout = plain(&out.stdout);
    assert!(
        out.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        plain(&out.stderr)
    );
    let forked = stdout
        .split("forked ")
        .nth(1)
        .and_then(|s| s.split(" into ").next())
        .unwrap_or_else(|| panic!("the fork line:\n{stdout}"))
        .to_string();
    assert_eq!(forked, sid);
    let branch = stdout
        .split(" into ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap()
        .to_string();
    assert!(stdout.contains("— 2 messages, at 2"), "{stdout}");

    // The branch is listed, naming its parent.
    let out = ferrule(home, &["sessions", "--all"]);
    let stdout = plain(&out.stdout);
    let branch_line = stdout
        .lines()
        .find(|l| l.starts_with(&branch))
        .unwrap_or_else(|| panic!("the branch is listed:\n{stdout}"));
    assert!(branch_line.contains(&format!("↳ {sid}")), "{branch_line}");

    // Forking a session that doesn't exist says so.
    let out = ferrule(home, &["chat", "--fork", "no-such-session"]);
    assert!(!out.status.success());
    assert!(
        plain(&out.stderr).contains("no session transcript"),
        "{}",
        plain(&out.stderr)
    );
}
