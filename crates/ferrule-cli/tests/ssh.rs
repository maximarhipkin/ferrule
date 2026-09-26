//! M34 through the real `ferrule` binary: `ferrule ssh trust|test`, then a
//! run whose workspace is a directory behind a real `sshd` on 127.0.0.1,
//! driven by a scripted model. Afterwards nothing ferrule kept or said,
//! and nothing the model was sent, holds the private key's bytes or its
//! comment (docs/m34-ssh-local.md §8).
//!
//! The sshd is found the way crates/ferrule-ssh/tests/sshd.rs finds it;
//! without one the test skips unless `FERRULE_REQUIRE_SSHD=1`.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const KEY_COMMENT: &str = "ferrule-e2e-KEYCOMMENT-41ab";

// ── sshd ───────────────────────────────────────────────────────────────

fn keygen() -> PathBuf {
    if let Ok(ssh) = std::env::var("FERRULE_SSH") {
        let p = Path::new(&ssh).with_file_name("ssh-keygen");
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("ssh-keygen")
}

fn gen_key(path: &Path, comment: &str) {
    let st = Command::new(keygen())
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(path)
        .status()
        .expect("ssh-keygen");
    assert!(st.success());
}

struct Sshd {
    dir: tempfile::TempDir,
    port: u16,
    child: Child,
    user: String,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Sshd {
    fn start() -> Option<Sshd> {
        let required = std::env::var("FERRULE_REQUIRE_SSHD").as_deref() == Ok("1");
        let skip = |why: String| -> Option<Sshd> {
            assert!(!required, "FERRULE_REQUIRE_SSHD=1 but {why}");
            eprintln!("skipping: {why}");
            None
        };
        let prog = match std::env::var("FERRULE_TEST_SSHD") {
            Ok(p) => PathBuf::from(p),
            Err(_) => match ["/usr/sbin/sshd", "/usr/local/sbin/sshd"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
            {
                Some(p) => p,
                None => return skip("no sshd (set FERRULE_TEST_SSHD)".into()),
            },
        };
        let dir = tempfile::Builder::new()
            .prefix("fsshd")
            .tempdir_in("/tmp")
            .unwrap();
        let d = dir.path();
        gen_key(&d.join("host"), "host");
        gen_key(&d.join("user"), KEY_COMMENT);
        std::fs::copy(d.join("user.pub"), d.join("authorized_keys")).unwrap();
        std::fs::create_dir(d.join("ws")).unwrap();
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string();
        for _ in 0..5 {
            let port = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            std::fs::write(
                d.join("sshd_config"),
                format!(
                    "Port {port}\nListenAddress 127.0.0.1\nHostKey {h}\nAuthorizedKeysFile {a}\n\
                     PidFile none\nStrictModes no\nUsePAM no\nPasswordAuthentication no\n\
                     KbdInteractiveAuthentication no\nAllowTcpForwarding yes\nLogLevel ERROR\n",
                    h = d.join("host").display(),
                    a = d.join("authorized_keys").display(),
                ),
            )
            .unwrap();
            // OpenSSH 9.8+ refuses a source that keeps disconnecting before
            // login, which every keyscan does; older sshd rejects the option.
            let config = d.join("sshd_config");
            let base = std::fs::read_to_string(&config).unwrap();
            std::fs::write(&config, format!("{base}PerSourcePenalties no\n")).unwrap();
            let checked = Command::new(&prog)
                .arg("-t")
                .arg("-f")
                .arg(&config)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if !checked.is_ok_and(|s| s.success()) {
                std::fs::write(&config, base).unwrap();
            }
            let mut c = Command::new(&prog);
            if let Ok(extra) = std::env::var("FERRULE_TEST_SSHD_ARGS") {
                c.args(extra.split_whitespace());
            }
            let mut child = c
                .arg("-D")
                .arg("-f")
                .arg(d.join("sshd_config"))
                .arg("-E")
                .arg(d.join("sshd.log"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn sshd");
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Some(Sshd {
                        dir,
                        port,
                        child,
                        user,
                    });
                }
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let log = std::fs::read_to_string(d.join("sshd.log")).unwrap_or_default();
        skip(format!("sshd didn't start: {log}"))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn ws(&self) -> PathBuf {
        std::fs::canonicalize(self.path("ws")).unwrap()
    }

    fn host_fingerprint(&self) -> String {
        let out = Command::new(keygen())
            .arg("-lf")
            .arg(self.path("host.pub"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .find(|w| w.starts_with("SHA256:"))
            .unwrap()
            .to_string()
    }
}

// ── The scripted model ─────────────────────────────────────────────────

/// Write a file, run a command beside it, then answer with what it saw.
fn reply(req: &Value) -> Value {
    let messages = req["messages"].as_array().unwrap();
    let tools_done = messages.iter().filter(|m| m["role"] == "tool").count();
    match tools_done {
        0 => call(
            "write_file",
            json!({"path": "made-remotely.txt", "content": "over ssh\n"}),
        ),
        1 => call("shell", json!({"command": "cat made-remotely.txt && pwd"})),
        _ => {
            let last = messages.last().unwrap()["content"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            answer(&format!("DONE: {last}"))
        }
    }
}

fn call(name: &str, args: Value) -> Value {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{name}"),
                    "type": "function",
                    "function": {"name": name, "arguments": args.to_string()},
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10},
    })
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

fn model_server() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let log = log.clone();
            std::thread::spawn(move || serve(stream, &log));
        }
    });
    (url, seen)
}

fn serve(mut stream: TcpStream, log: &Mutex<Vec<String>>) {
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
    let text = String::from_utf8_lossy(&body).into_owned();
    let req: Value = serde_json::from_str(&text).unwrap();
    let out = reply(&req).to_string();
    log.lock().unwrap().push(text);
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
        out.len()
    );
}

// ── The binary ─────────────────────────────────────────────────────────

fn home(url: &str, s: &Sshd) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    for d in ["work", "data", "home"] {
        std::fs::create_dir_all(home.join(d)).unwrap();
    }
    std::fs::write(
        home.join("ferrule.toml"),
        format!(
            r#"default_provider = "mock"
workspace = "ssh:box"

[providers.mock]
base_url = "{url}"
api_key_env = "FERRULE_TEST_KEY"
model = "scripted"

[skills]
enabled = false

[sandbox]
mode = "off"

[ssh.box]
host = "127.0.0.1"
user = "{user}"
port = {port}
path = "{ws}"
identity_file = "{key}"
ssh_config = "/dev/null"
"#,
            user = s.user,
            port = s.port,
            ws = s.ws().display(),
            key = s.path("user").display(),
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
        .stdin(Stdio::null());
    for var in ["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME"] {
        cmd.env(var, home.join("home"));
    }
    for var in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "SSH_AUTH_SOCK",
    ] {
        cmd.env_remove(var);
    }
    cmd.output().unwrap()
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Every file under `dir`, as bytes.
fn files(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            files(&p, out);
        } else if ft.is_file() {
            out.push((p.clone(), std::fs::read(&p).unwrap_or_default()));
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn a_run_over_ssh_works_and_the_key_never_leaves_ssh() {
    let Some(s) = Sshd::start() else { return };
    let (url, seen) = model_server();
    let dir = home(&url, &s);
    let h = dir.path();
    let known = h.join("data").join("ssh").join("known_hosts");
    let printed = std::cell::RefCell::new(Vec::<u8>::new());
    let ferrule = |h: &Path, args: &[&str]| {
        let out = ferrule(h, args);
        printed.borrow_mut().extend(&out.stdout);
        printed.borrow_mut().extend(&out.stderr);
        out
    };

    // Unknown host: the run refuses and says how to trust it.
    let out = ferrule(h, &["run", "write a file"]);
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("ferrule ssh trust"),
        "{}",
        describe(&out)
    );
    assert!(seen.lock().unwrap().is_empty(), "the model was called");

    // No terminal and no --fingerprint: nothing is trusted.
    let out = ferrule(h, &["ssh", "trust", "box"]);
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(!known.exists());

    // A wrong fingerprint: nothing is trusted.
    let out = ferrule(
        h,
        &[
            "ssh",
            "trust",
            "box",
            "--fingerprint",
            "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ],
    );
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(!known.exists());

    let fp = s.host_fingerprint();
    let out = ferrule(h, &["ssh", "trust", "ssh:box", "--fingerprint", &fp]);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(std::fs::read_to_string(&known)
        .unwrap()
        .contains("[127.0.0.1]:"));
    let out = ferrule(h, &["ssh", "trust", "box"]);
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("already known"),
        "{}",
        describe(&out)
    );

    let out = ferrule(h, &["ssh", "test", "box"]);
    assert!(out.status.success(), "{}", describe(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("login ok") && text.contains("writable"),
        "{text}"
    );
    assert!(
        text.contains("the remote ACCOUNT is the boundary"),
        "{text}"
    );

    let out = ferrule(h, &["ssh", "list"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("ssh:box (default workspace)"),
        "{}",
        describe(&out)
    );

    let out = ferrule(h, &["run", "write a file"]);
    assert!(out.status.success(), "{}", describe(&out));
    let ws = s.ws();
    assert_eq!(
        std::fs::read_to_string(ws.join("made-remotely.txt")).unwrap(),
        "over ssh\n"
    );
    // Nothing landed in the local anchor or the cwd.
    assert!(!h.join("work").join("made-remotely.txt").exists());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("DONE") && stdout.contains(&*ws.to_string_lossy()),
        "{}",
        describe(&out)
    );
    let requests = seen.lock().unwrap().clone();
    assert!(
        requests[0].contains("on the remote host 127.0.0.1"),
        "the prompt doesn't name the remote: {}",
        requests[0]
    );
    let first: Value = serde_json::from_str(&requests[0]).unwrap();
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(!tools.contains(&"code_search"), "{tools:?}");

    // A changed host key is a hard stop, and the old line stays.
    let wrong = s.path("wrong");
    gen_key(&wrong, "wrong");
    let wrong_pub = std::fs::read_to_string(wrong.with_extension("pub")).unwrap();
    let mut w = wrong_pub.split_whitespace();
    let line = format!(
        "[127.0.0.1]:{} {} {}\n",
        s.port,
        w.next().unwrap(),
        w.next().unwrap()
    );
    std::fs::write(&known, &line).unwrap();
    let out = ferrule(h, &["run", "write a file"]);
    assert!(!out.status.success(), "{}", describe(&out));
    let out = ferrule(h, &["ssh", "trust", "box", "--fingerprint", &fp]);
    assert!(
        !out.status.success() && String::from_utf8_lossy(&out.stderr).contains("STOP"),
        "{}",
        describe(&out)
    );
    assert_eq!(std::fs::read_to_string(&known).unwrap(), line);

    // The key: its private body, its comment. Checked everywhere ferrule
    // wrote, everything it printed, everything the model was sent.
    let private = std::fs::read_to_string(s.path("user")).unwrap();
    let body: Vec<&str> = private
        .lines()
        .filter(|l| !l.starts_with("-----") && l.len() > 20)
        .collect();
    assert!(!body.is_empty());
    let mut kept = Vec::new();
    files(&h.join("data"), &mut kept);
    files(&h.join("home"), &mut kept);
    files(&h.join("work"), &mut kept);
    assert!(
        kept.iter()
            .any(|(p, _)| p.extension().is_some_and(|e| e == "jsonl")),
        "no transcript or ledger was written: {:?}",
        kept.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );
    for r in &requests {
        kept.push((PathBuf::from("<model request>"), r.clone().into_bytes()));
    }
    kept.push((PathBuf::from("<what ferrule printed>"), printed.take()));
    for (path, bytes) in &kept {
        assert!(
            !contains(bytes, KEY_COMMENT.as_bytes()),
            "{} holds the key's comment",
            path.display()
        );
        for l in &body {
            assert!(
                !contains(bytes, l.as_bytes()),
                "{} holds private key bytes",
                path.display()
            );
        }
    }
}
