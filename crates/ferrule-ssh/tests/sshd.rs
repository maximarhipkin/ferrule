//! The remote tools against a real OpenSSH `sshd` on 127.0.0.1, started
//! per test with a temp host key, user key and `authorized_keys`
//! (docs/m34-ssh-local.md §7).
//!
//! The sshd is `FERRULE_TEST_SSHD` (plus `FERRULE_TEST_SSHD_ARGS`), else
//! the system's. Without one the tests skip, unless
//! `FERRULE_REQUIRE_SSHD=1` (Linux CI), which makes a missing or broken
//! sshd a failure. The client is `FERRULE_SSH`, else `ssh`.
#![cfg(unix)]

use ferrule_core::tool::{Tool, ToolContext};
use ferrule_ssh::{
    trust, DenySpec, Forward, HostConfig, Link, LinkOptions, RemoteEditFile, RemoteListDir,
    RemoteReadFile, RemoteShellTool, RemoteWriteFile, Target,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Put in the user key's comment; it must never come back.
const KEY_COMMENT: &str = "ferrule-test-KEYCOMMENT-7c1f";

fn required() -> bool {
    std::env::var("FERRULE_REQUIRE_SSHD").as_deref() == Ok("1")
}

fn sshd_program() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FERRULE_TEST_SSHD") {
        return Some(PathBuf::from(p));
    }
    [
        "/usr/sbin/sshd",
        "/usr/local/sbin/sshd",
        "/opt/homebrew/sbin/sshd",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

/// `ssh-keygen` beside the client, else on `PATH`.
fn keygen() -> PathBuf {
    if let Ok(ssh) = std::env::var("FERRULE_SSH") {
        let p = Path::new(&ssh).with_file_name("ssh-keygen");
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("ssh-keygen")
}

fn skip(why: &str) -> Option<Sshd> {
    if required() {
        panic!("FERRULE_REQUIRE_SSHD=1 but {why}");
    }
    eprintln!("skipping: {why}");
    None
}

fn gen_key(path: &Path, comment: &str) {
    let st = Command::new(keygen())
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(path)
        .status()
        .expect("ssh-keygen");
    assert!(st.success());
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
        let Some(prog) = sshd_program() else {
            return skip("no sshd (set FERRULE_TEST_SSHD)");
        };
        let dir = tempfile::Builder::new()
            .prefix("fsshd")
            .tempdir_in("/tmp")
            .unwrap();
        let d = dir.path();
        gen_key(&d.join("host"), "host");
        gen_key(&d.join("user"), KEY_COMMENT);
        gen_key(&d.join("other"), "other");
        std::fs::copy(d.join("user.pub"), d.join("authorized_keys")).unwrap();
        std::fs::create_dir(d.join("ws")).unwrap();
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string();
        for _ in 0..5 {
            let port = free_port();
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
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
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
        let log = std::fs::read_to_string(dir.path().join("sshd.log")).unwrap_or_default();
        skip(&format!("sshd didn't start: {log}"))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn ws(&self) -> String {
        // The real path: /tmp may be a symlink (macOS).
        std::fs::canonicalize(self.path("ws"))
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn target(&self, key: &str) -> Target {
        Target::from_config(
            "t",
            &HostConfig {
                host: "127.0.0.1".into(),
                user: Some(self.user.clone()),
                port: Some(self.port),
                path: self.ws(),
                identity_file: Some(self.path(key)),
                ssh_config: Some(PathBuf::from("/dev/null")),
                ssh: None,
            },
        )
        .unwrap()
    }

    /// The host's key line, as ferrule's known_hosts would hold it.
    fn host_line(&self) -> String {
        let pubkey = std::fs::read_to_string(self.path("host.pub")).unwrap();
        let mut w = pubkey.split_whitespace();
        format!(
            "[127.0.0.1]:{} {} {}\n",
            self.port,
            w.next().unwrap(),
            w.next().unwrap()
        )
    }

    fn opts(&self, known_hosts: &str) -> LinkOptions {
        let kh = self.path("kh");
        std::fs::write(&kh, known_hosts).unwrap();
        LinkOptions {
            known_hosts: Some(kh),
            user_known_hosts: Vec::new(),
            deny: DenySpec {
                default: true,
                deny_read: vec!["secret".into()],
                allow_read: Vec::new(),
            },
            forward: None,
            extra_args: vec!["-o".into(), "GlobalKnownHostsFile=/dev/null".into()],
        }
    }

    fn link(&self) -> Arc<Link> {
        Link::new(self.target("user"), self.opts(&self.host_line()))
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        workspace: PathBuf::from("/nonexistent-local"),
        max_output_chars: 30_000,
    }
}

async fn call(tool: &dyn Tool, args: serde_json::Value) -> Result<String, String> {
    tool.call(args, &ctx())
        .await
        .map(|o| o.content)
        .map_err(|e| e.to_string())
}

/// No private key bytes and no key comment in anything shown.
fn assert_no_key(sshd: &Sshd, texts: &[&str]) {
    let private = std::fs::read_to_string(sshd.path("user")).unwrap();
    let body: Vec<&str> = private
        .lines()
        .filter(|l| !l.starts_with("-----") && l.len() > 20)
        .collect();
    assert!(!body.is_empty());
    for t in texts {
        assert!(!t.contains("PRIVATE KEY"), "{t}");
        assert!(!t.contains(KEY_COMMENT), "{t}");
        for b in &body {
            assert!(!t.contains(b), "key bytes leaked: {t}");
        }
    }
}

/// Running, not a zombie: an orphan's zombie may never be reaped in a
/// container whose init doesn't.
fn alive(pid: i32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

async fn wait_for_file(p: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(s) = std::fs::read_to_string(p) {
            if s.ends_with('\n') {
                return s;
            }
        }
        assert!(Instant::now() < deadline, "{} never appeared", p.display());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_dead(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(pid) {
        assert!(Instant::now() < deadline, "remote pid {pid} still alive");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn file_and_shell_tools_work_on_the_remote_workspace() {
    let Some(sshd) = Sshd::start() else { return };
    let link = sshd.link();
    let ws = sshd.ws();
    let remote = link.connect().await.unwrap();
    assert_eq!(remote.workspace, ws);
    assert!(remote.writable);

    let out = call(
        &RemoteWriteFile(link.clone()),
        json!({"path": "src/a b.txt", "content": "one\ntwo\n"}),
    )
    .await
    .unwrap();
    assert!(out.contains("wrote 8 bytes"), "{out}");
    assert_eq!(
        std::fs::read_to_string(sshd.path("ws/src/a b.txt")).unwrap(),
        "one\ntwo\n"
    );

    let read = call(
        &RemoteReadFile(link.clone()),
        json!({"path": "src/a b.txt"}),
    )
    .await
    .unwrap();
    assert!(read.contains("one") && read.contains("two"), "{read}");

    let edited = call(
        &RemoteEditFile(link.clone()),
        json!({"path": "src/a b.txt", "old_string": "two", "new_string": "deux"}),
    )
    .await
    .unwrap();
    assert!(!edited.is_empty());
    assert_eq!(
        std::fs::read_to_string(sshd.path("ws/src/a b.txt")).unwrap(),
        "one\ndeux\n"
    );

    let listed = call(&RemoteListDir(link.clone()), json!({"path": "."}))
        .await
        .unwrap();
    assert!(listed.contains("src"), "{listed}");

    let shell = RemoteShellTool::new(link.clone());
    let out = call(
        &shell,
        json!({"command": "pwd; cat 'src/a b.txt'; echo oops >&2; exit 3"}),
    )
    .await
    .unwrap();
    assert!(out.contains(&ws), "{out}");
    assert!(out.contains("deux"), "{out}");
    assert!(out.contains("[stderr]\noops"), "{out}");
    assert!(out.ends_with("[exit code: 3]"), "{out}");

    // The same deny list as the local shell.
    let err = call(&shell, json!({"command": "rm -rf /"}))
        .await
        .unwrap_err();
    assert!(err.contains("deny list"), "{err}");

    // Out of the workspace, and into a denied path, are refused remotely
    // just as locally.
    let err = call(&RemoteReadFile(link.clone()), json!({"path": "../user"}))
        .await
        .unwrap_err();
    assert!(err.contains("escapes workspace"), "{err}");
    let err = call(
        &RemoteReadFile(link.clone()),
        json!({"path": sshd.path("user").to_string_lossy()}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("escapes workspace"), "{err}");
    std::os::unix::fs::symlink(sshd.path("user"), sshd.path("ws/link")).unwrap();
    let err = call(&RemoteReadFile(link.clone()), json!({"path": "link"}))
        .await
        .unwrap_err();
    assert!(err.contains("escapes workspace"), "{err}");
    std::fs::write(sshd.path("ws/secret"), "s").unwrap();
    let err = call(&RemoteReadFile(link.clone()), json!({"path": "secret"}))
        .await
        .unwrap_err();
    assert!(!err.contains("escapes"), "{err}");
    let err = call(&RemoteReadFile(link.clone()), json!({"path": "nope.txt"}))
        .await
        .unwrap_err();
    assert!(err.contains("No such file"), "{err}");

    let status = link.status_line();
    assert!(status.contains("connected"), "{status}");
    assert_no_key(&sshd, &[&out, &read, &listed, &status, &err]);
}

#[tokio::test]
async fn an_unknown_host_is_refused_and_trust_adds_it_by_fingerprint() {
    let Some(sshd) = Sshd::start() else { return };
    let link = Link::new(sshd.target("user"), sshd.opts(""));
    let err = link.connect().await.unwrap_err();
    assert!(err.contains("ferrule ssh trust"), "{err}");
    assert!(!sshd.path("ws").join(".ferrule").exists());

    // Setup's path: scan, show, add only the fingerprint the owner saw.
    let target = sshd.target("user");
    let (resolved, keys) = trust::scan(&target).await.unwrap();
    assert_eq!(resolved.known_as, format!("[127.0.0.1]:{}", sshd.port));
    let fp_out = Command::new(keygen())
        .arg("-l")
        .arg("-f")
        .arg(sshd.path("host.pub"))
        .output()
        .unwrap();
    let expected = String::from_utf8_lossy(&fp_out.stdout)
        .split_whitespace()
        .find(|w| w.starts_with("SHA256:"))
        .unwrap()
        .to_string();
    assert!(trust::matching(&keys, "SHA256:wrong").is_empty());
    let good = trust::matching(&keys, &expected);
    assert_eq!(good.len(), 1);
    let kh = sshd.path("kh");
    assert!(!trust::is_known(&target, &resolved.known_as, std::slice::from_ref(&kh)).await);
    trust::add(&kh, &good).unwrap();
    assert!(trust::is_known(&target, &resolved.known_as, std::slice::from_ref(&kh)).await);

    // `opts` starts ferrule's known_hosts afresh: add again, as setup would.
    let opts = sshd.opts("");
    trust::add(&kh, &good).unwrap();
    let link = Link::new(target, opts);
    link.connect().await.unwrap();
    let (code, out) = ferrule_ssh::tools::run_remote(&link, "echo hi", Duration::from_secs(20))
        .await
        .unwrap();
    assert_eq!((code, out.trim()), (0, "hi"));
}

#[tokio::test]
async fn a_changed_host_key_is_a_hard_stop() {
    let Some(sshd) = Sshd::start() else { return };
    let other = std::fs::read_to_string(sshd.path("other.pub")).unwrap();
    let mut w = other.split_whitespace();
    let line = format!(
        "[127.0.0.1]:{} {} {}\n",
        sshd.port,
        w.next().unwrap(),
        w.next().unwrap()
    );
    let link = Link::new(sshd.target("user"), sshd.opts(&line));
    let err = link.connect().await.unwrap_err();
    assert!(err.to_lowercase().contains("changed"), "{err}");
    assert!(err.contains("ssh-keygen -R"), "{err}");
    // Poisoned: even with the file fixed, this link sends nothing more.
    std::fs::write(sshd.path("kh"), sshd.host_line()).unwrap();
    let again = link.connect().await.unwrap_err();
    assert!(again.to_lowercase().contains("changed"), "{again}");
    assert!(
        link.status_line().contains("STOPPED"),
        "{}",
        link.status_line()
    );
}

#[tokio::test]
async fn an_auth_failure_says_so() {
    let Some(sshd) = Sshd::start() else { return };
    let link = Link::new(sshd.target("other"), sshd.opts(&sshd.host_line()));
    let started = Instant::now();
    let err = link.connect().await.unwrap_err();
    assert!(err.to_lowercase().contains("auth"), "{err}");
    // Not retried like a network blip.
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert_no_key(&sshd, &[&err, &link.status_line()]);
}

#[tokio::test]
async fn a_dropped_link_is_interrupted_and_the_next_command_reconnects() {
    let Some(sshd) = Sshd::start() else { return };
    let link = sshd.link();
    link.connect().await.unwrap();
    let pidfile = sshd.path("ws/pid");
    let l2 = link.clone();
    let task = tokio::spawn(async move {
        ferrule_ssh::tools::run_remote(
            &l2,
            "echo started; echo $$ > pid; sleep 60; echo finished",
            Duration::from_secs(90),
        )
        .await
    });
    let pid: i32 = wait_for_file(&pidfile).await.trim().parse().unwrap();
    let master = link.master_pid().await.expect("a master");
    unsafe {
        libc::kill(-(master as i32), libc::SIGKILL);
    }
    let err = task.await.unwrap().unwrap_err();
    assert!(err.starts_with("interrupted"), "{err}");
    assert!(err.contains("did NOT finish"), "{err}");
    assert!(!err.contains("finished\n"), "{err}");
    // The remote command went with the connection.
    wait_dead(pid).await;
    // And the next one reconnects.
    let (code, out) = ferrule_ssh::tools::run_remote(&link, "echo back", Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!((code, out.trim()), (0, "back"));
    assert!(
        link.status_line().contains("reconnect"),
        "{}",
        link.status_line()
    );
}

#[tokio::test]
async fn stop_kills_the_remote_process_group() {
    let Some(sshd) = Sshd::start() else { return };
    let link = sshd.link();
    link.connect().await.unwrap();
    let shell = RemoteShellTool::new(link.clone());
    let task = tokio::spawn(async move {
        shell
            .call(
                json!({"command": "sleep 60 & echo $! > child; echo $$ > pid; wait"}),
                &ctx(),
            )
            .await
    });
    let pid: i32 = wait_for_file(&sshd.path("ws/pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    let child: i32 = wait_for_file(&sshd.path("ws/child"))
        .await
        .trim()
        .parse()
        .unwrap();
    assert!(alive(pid) && alive(child));
    // `/stop` drops the turn, and with it the tool call.
    task.abort();
    let _ = task.await;
    wait_dead(pid).await;
    wait_dead(child).await;
    // The link itself is fine.
    let (code, _) = ferrule_ssh::tools::run_remote(&link, "true", Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(code, 0);
}

#[tokio::test]
async fn timeouts_and_output_caps_match_the_local_shell() {
    let Some(sshd) = Sshd::start() else { return };
    let link = sshd.link();
    let local = ferrule_tools::shell::ShellTool::default();
    let mut shell = RemoteShellTool::new(link.clone());
    assert_eq!(shell.timeout, local.timeout);
    shell.timeout = Duration::from_secs(2);
    let err = call(&shell, json!({"command": "echo $$ > pid; sleep 60"}))
        .await
        .unwrap_err();
    assert!(err.contains("timeout after 2s"), "{err}");
    let pid: i32 = wait_for_file(&sshd.path("ws/pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    wait_dead(pid).await;

    shell.timeout = Duration::from_secs(60);
    let out = call(
        &shell,
        json!({"command": "head -c 200000 /dev/zero | tr '\\0' a"}),
    )
    .await
    .unwrap();
    assert!(
        out.contains("[truncated, 200015 chars total]"),
        "{}",
        &out[out.len() - 80..]
    );
    assert!(out.len() < 30_100);
}

/// A stand-in for ferrule's proxy: answers any request with a fixed body.
async fn fake_proxy() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nvia-proxy\n")
                    .await;
            });
        }
    });
    port
}

#[tokio::test]
async fn the_credential_proxy_reaches_remote_commands() {
    let Some(sshd) = Sshd::start() else { return };
    let port = fake_proxy().await;
    let ca = sshd.path("ca.pem");
    std::fs::write(&ca, "-----BEGIN CERTIFICATE-----\nfake\n").unwrap();
    let ca_path = ca.to_string_lossy().into_owned();
    let mut opts = sshd.opts(&sshd.host_line());
    opts.forward = Some(Forward {
        port,
        env: Arc::new(move || {
            vec![
                ("HTTPS_PROXY".into(), format!("http://127.0.0.1:{port}")),
                ("SSL_CERT_FILE".into(), ca_path.clone()),
            ]
        }),
    });
    let link = Link::new(sshd.target("user"), opts);
    let (code, out) = ferrule_ssh::tools::run_remote(
        &link,
        "echo \"$HTTPS_PROXY\"; echo \"$SSL_CERT_FILE\"; cat \"$SSL_CERT_FILE\"",
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("http://127.0.0.1:"), "{out}");
    assert_ne!(lines[0], format!("http://127.0.0.1:{port}"), "{out}");
    assert_ne!(lines[1], ca.to_string_lossy(), "{out}");
    assert!(lines[1].ends_with("/ca.pem"), "{out}");
    assert!(out.contains("fake"), "{out}");
    let status = link.status_line();
    assert!(status.contains("credential proxy forwarded"), "{status}");

    // Through the forward, to the local proxy (a real client, if there).
    let (code, out) = ferrule_ssh::tools::run_remote(
        &link,
        "command -v curl >/dev/null || { echo no-curl; exit 0; }; \
         curl -s --max-time 10 --noproxy '*' \"$HTTPS_PROXY/x\"",
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("via-proxy") || out.contains("no-curl"),
        "{out}"
    );
}
