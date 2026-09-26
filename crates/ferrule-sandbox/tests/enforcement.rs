//! Real enforcement, against the running kernel. Each test skips (with a
//! note on stderr) where the backend isn't available, so they're safe in
//! any CI; where it is, they prove the kernel refuses what it should.

use ferrule_sandbox::{Mode, Policy, Sandbox};
use std::path::Path;
use std::process::{Output, Stdio};

fn sandbox(policy: Policy) -> Option<Sandbox> {
    if cfg!(windows) {
        eprintln!("skipping: these run /bin/sh; Windows has tests/windows.rs");
        return None;
    }
    let sb = Sandbox::new(Policy {
        require: false,
        ..policy
    })
    .unwrap();
    if sb.is_active() {
        Some(sb)
    } else {
        eprintln!(
            "skipping: no sandbox here ({})",
            sb.degraded().unwrap_or("?")
        );
        None
    }
}

fn sh(sb: &Sandbox, workspace: &Path, script: &str) -> Output {
    sb.command("/bin/sh", ["-c", script], workspace)
        .unwrap()
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn no_tmp(mode: Mode) -> Policy {
    // Without `tmp`, a second tempdir is a convenient "outside" to aim at.
    Policy {
        mode,
        tmp: false,
        ..Policy::default()
    }
}

#[test]
fn writes_land_in_the_workspace_and_nowhere_else() {
    let Some(sb) = sandbox(no_tmp(Mode::WorkspaceWrite)) else {
        return;
    };
    let ws = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("readable"), "hello").unwrap();

    let out = sh(
        &sb,
        ws.path(),
        "mkdir -p sub && echo ok > sub/f && cat sub/f && echo x > /dev/null",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(ws.path().join("sub/f")).unwrap(),
        "ok\n"
    );

    let target = outside.path().join("escaped");
    let out = sh(
        &sb,
        ws.path(),
        &format!("echo pwned > '{}'", target.display()),
    );
    assert!(!out.status.success());
    assert!(!target.exists(), "write outside the workspace went through");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(ferrule_sandbox::DENIED), "{stderr}");

    // Reads outside stay open — the agent needs the toolchain.
    let out = sh(
        &sb,
        ws.path(),
        &format!("cat '{}'", outside.path().join("readable").display()),
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");

    // A child of a child is still confined.
    let out = sh(
        &sb,
        ws.path(),
        &format!("sh -c \"sh -c 'touch {}'\"", target.display()),
    );
    assert!(!out.status.success() && !target.exists());
}

#[test]
fn read_only_mode_refuses_even_the_workspace() {
    let Some(sb) = sandbox(no_tmp(Mode::ReadOnly)) else {
        return;
    };
    let ws = tempfile::tempdir().unwrap();
    let out = sh(&sb, ws.path(), "touch f; ls; echo fine > /dev/null");
    assert!(!ws.path().join("f").exists());
    assert!(out.status.success(), "/dev/null stays writable");
}

#[test]
fn temp_dirs_are_writable_when_enabled() {
    let Some(sb) = sandbox(Policy {
        mode: Mode::WorkspaceWrite,
        ..Policy::default()
    }) else {
        return;
    };
    // Outside /tmp, so a successful mktemp proves the temp grant.
    let ws = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let out = sh(
        &sb,
        ws.path(),
        "f=$(mktemp) && echo x > \"$f\" && rm \"$f\"",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn hidden_paths_stay_shut_even_inside_the_workspace() {
    // The hard case: the workspace is an ancestor of the hidden directory,
    // so the carve has to cut it out of a writable root, not just `/`.
    let ws = tempfile::tempdir().unwrap();
    let data = ws.path().join("data");
    let private = data.join("private");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::create_dir_all(data.join("sub")).unwrap();
    std::fs::write(private.join("secrets.env"), "API_KEY=sk-live\n").unwrap();
    std::fs::write(data.join("memory.db"), "notes\n").unwrap();
    let Some(sb) = sandbox(Policy {
        hidden: vec![private.clone()],
        ..no_tmp(Mode::WorkspaceWrite)
    }) else {
        return;
    };

    let out = sh(
        &sb,
        ws.path(),
        "cat data/memory.db && echo more >> data/memory.db && ls data && \
         echo x > data/sub/f && ln -s ../private data/sub/p",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("notes"), "siblings stay readable");
    assert!(stdout.contains("private"), "the parent still lists");

    let secret = private.join("secrets.env");
    let via_root = format!("/proc/self/root{}", secret.display());
    for script in [
        "cat data/private/secrets.env",
        "cat data/sub/p/secrets.env",
        via_root.as_str(),
        "echo x > data/private/new",
        "rm data/private/secrets.env",
        "mv data/private/secrets.env data/sub/",
    ] {
        let script = if script.starts_with('/') {
            format!("cat '{script}'")
        } else {
            script.to_string()
        };
        let out = sh(&sb, ws.path(), &script);
        assert!(!out.status.success(), "`{script}` went through");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("sk-live"),
            "`{script}` leaked the secret"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&secret).unwrap(),
        "API_KEY=sk-live\n"
    );
    // READ_DIR on the ancestors is inherited, so on Linux the names inside
    // stay listable — only contents are shut. Seatbelt hides both.
    assert!(!private.join("new").exists());
}

#[test]
fn hidden_paths_outside_the_workspace_and_the_host_environ() {
    let ws = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let private = elsewhere.path().join("private");
    std::fs::create_dir(&private).unwrap();
    std::fs::write(private.join("key.pem"), "PRIVATE").unwrap();
    std::fs::write(elsewhere.path().join("bundle.pem"), "PUBLIC").unwrap();
    let Some(sb) = sandbox(Policy {
        hidden: vec![private.clone()],
        ..no_tmp(Mode::WorkspaceWrite)
    }) else {
        return;
    };
    let dir = elsewhere.path().display();
    let out = sh(&sb, ws.path(), &format!("cat '{dir}/bundle.pem'"));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "PUBLIC");
    let out = sh(&sb, ws.path(), &format!("cat '{dir}/private/key.pem'"));
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("PRIVATE"));

    // Secrets loaded from the secrets file live in the host's environment;
    // Landlock's ptrace scoping keeps a sandboxed child out of it.
    let out = sh(
        &sb,
        ws.path(),
        &format!("cat /proc/{}/environ", std::process::id()),
    );
    assert!(!out.status.success(), "the host environ was readable");
    if cfg!(target_os = "linux") {
        let open = sb.unconfined("sandbox = false");
        let out = sh(
            &open,
            ws.path(),
            &format!("cat /proc/{}/environ", std::process::id()),
        );
        assert!(
            !out.status.success(),
            "an unconfined helper read the environ"
        );
    }
}

#[test]
fn denied_reads_fail_and_allowed_reads_work() {
    let ws = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join(".env"), "TOKEN=sk-live\n").unwrap();
    std::fs::write(ws.path().join("notes.txt"), "fine\n").unwrap();
    let creds = elsewhere.path().join("creds");
    std::fs::create_dir(&creds).unwrap();
    std::fs::write(creds.join("key"), "sk-live\n").unwrap();
    std::fs::write(elsewhere.path().join("open.txt"), "fine\n").unwrap();
    let Some(sb) = sandbox(Policy {
        deny_read: vec![".env".into(), creds.clone()],
        ..no_tmp(Mode::WorkspaceWrite)
    }) else {
        return;
    };
    let dir = elsewhere.path().display();
    for script in [
        "cat .env".to_string(),
        format!("cat '{dir}/creds/key'"),
        format!("cp '{dir}/creds/key' stolen"),
    ] {
        let out = sh(&sb, ws.path(), &script);
        assert!(!out.status.success(), "`{script}` went through");
        assert!(!String::from_utf8_lossy(&out.stdout).contains("sk-live"));
    }
    assert!(!ws.path().join("stolen").exists());
    let out = sh(&sb, ws.path(), &format!("cat notes.txt '{dir}/open.txt'"));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "fine\nfine\n");

    // An MCP server's sandbox keeps the same denies on top of its state dir.
    let state = tempfile::tempdir().unwrap();
    let helper = sb.for_helper(state.path(), &[]);
    let out = sh(&helper, ws.path(), &format!("cat '{dir}/creds/key'"));
    assert!(!out.status.success());
    let out = sh(&helper, ws.path(), "cat notes.txt");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "fine\n");

    // One with `sandbox = false` writes where it likes, but the denies hold.
    // (On Linux not directly beside a denied path: the carve grants that
    // dir's entries, not new ones in it.)
    std::fs::create_dir(elsewhere.path().join("out")).unwrap();
    let open = sb.unconfined("sandbox = false");
    assert!(open.is_hide_only());
    let out = sh(&open, ws.path(), &format!("cat '{dir}/creds/key'"));
    assert!(
        !out.status.success(),
        "an unconfined helper read a denied path"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("sk-live"));
    let out = sh(
        &open,
        ws.path(),
        &format!("echo w > '{dir}/out/written' && cat '{dir}/open.txt' .env"),
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("sk-live"));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("fine\n"));
    assert!(
        elsewhere.path().join("out/written").exists(),
        "writes are open"
    );
}

#[test]
fn secret_env_vars_do_not_reach_the_command() {
    std::env::set_var("FERRULE_TEST_API_KEY", "sk-should-not-leak");
    std::env::set_var("FERRULE_TEST_PLAIN", "visible");
    let sb = Sandbox::new(Policy {
        mode: Mode::Off,
        ..Policy::default()
    })
    .unwrap();
    let out = if cfg!(windows) {
        sb.command("cmd", ["/d", "/c", "set"], &std::env::temp_dir())
            .unwrap()
            .stdin(Stdio::null())
            .output()
            .unwrap()
    } else {
        sh(&sb, Path::new("/"), "env")
    };
    let env = String::from_utf8_lossy(&out.stdout);
    assert!(!env.contains("sk-should-not-leak"));
    assert!(env.contains("FERRULE_TEST_PLAIN=visible"));
}

/// Runs this test binary's `net_probe_helper` under the sandbox; the helper
/// tries to open a UDP and a TCP socket on loopback and exits 0 only if both
/// work. A shell `curl` would need the network to be up to mean anything.
fn net_probe(sb: &Sandbox) -> Output {
    let me = std::env::current_exe().unwrap();
    sb.command(
        &me,
        [
            "--exact",
            "net_probe_helper",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ],
        Path::new("/"),
    )
    .unwrap()
    .env("FERRULE_NET_PROBE", "1")
    .stdin(Stdio::null())
    .output()
    .unwrap()
}

#[test]
#[ignore = "helper, run by the network tests in a sandboxed child"]
fn net_probe_helper() {
    if std::env::var_os("FERRULE_NET_PROBE").is_none() {
        return;
    }
    let udp = std::net::UdpSocket::bind("127.0.0.1:0");
    let tcp = std::net::TcpListener::bind("127.0.0.1:0");
    eprintln!(
        "udp: {:?}\ntcp: {:?}",
        udp.as_ref().err(),
        tcp.as_ref().err()
    );
    assert!(udp.is_ok() && tcp.is_ok());
}

#[test]
fn network_off_blocks_inet_sockets_and_on_allows_them() {
    let Some(off) = sandbox(Policy {
        network: false,
        ..Policy::default()
    }) else {
        return;
    };
    let out = net_probe(&off);
    assert!(
        !out.status.success(),
        "network = false still opened an inet socket"
    );
    // Refused by the filter (EPERM), not failing for some unrelated reason.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("udp: Some(Os { code: 1"), "{stderr}");
    let on = sandbox(Policy::default()).unwrap();
    let out = net_probe(&on);
    assert!(
        out.status.success(),
        "network = true should allow sockets: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Unix sockets are local IPC, not network: they keep working.
    let ws = tempfile::tempdir().unwrap();
    let out = sh(&off, ws.path(), "command -v python3 >/dev/null || exit 0; python3 -c 'import socket; socket.socket(socket.AF_UNIX)'");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Runs `unix_probe_helper` under the sandbox with `targets` (`label=path`,
/// `label=@abstract`, or `own=path` to listen and then connect itself) and
/// returns its stdout: one `label: ok` or `label: errno N` line per target.
#[cfg(unix)]
fn unix_probe(sb: &Sandbox, workspace: &Path, targets: &[String]) -> String {
    let me = std::env::current_exe().unwrap();
    let out = sb
        .command(
            &me,
            [
                "--exact",
                "unix_probe_helper",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ],
            workspace,
        )
        .unwrap()
        .env("FERRULE_UNIX_PROBE", targets.join("\n"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[ignore = "helper, run by the Unix-socket tests in a sandboxed child"]
#[cfg(unix)]
fn unix_probe_helper() {
    use std::os::unix::net::{UnixListener, UnixStream};
    let Some(targets) = std::env::var_os("FERRULE_UNIX_PROBE") else {
        return;
    };
    let mut keep = Vec::new();
    for line in targets.to_string_lossy().lines() {
        let (label, target) = line.split_once('=').unwrap();
        let result = if let Some(name) = target.strip_prefix('@') {
            abstract_connect(name)
        } else {
            if label == "own" {
                keep.push(UnixListener::bind(target).unwrap());
            }
            UnixStream::connect(target).map(drop)
        };
        match result {
            Ok(()) => println!("{label}: ok"),
            Err(e) => println!("{label}: errno {}", e.raw_os_error().unwrap_or(-1)),
        }
    }
}

#[cfg(target_os = "linux")]
fn abstract_connect(name: &str) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
    std::os::unix::net::UnixStream::connect_addr(&addr).map(drop)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn abstract_connect(_: &str) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[test]
#[cfg(unix)]
fn unix_sockets_outside_the_allowlist_are_refused() {
    use std::os::unix::net::UnixListener;
    let outside = tempfile::tempdir().unwrap();
    let out = dunce::canonicalize(outside.path()).unwrap();
    let ok = out.join("ok.sock");
    let no = out.join("no.sock");
    let _l1 = UnixListener::bind(&ok).unwrap();
    let _l2 = UnixListener::bind(&no).unwrap();
    // An allowed directory holding a symlink to the refused socket.
    let lure = tempfile::tempdir().unwrap();
    let lure = dunce::canonicalize(lure.path()).unwrap();
    std::os::unix::fs::symlink(&no, lure.join("docker.sock")).unwrap();

    let tag = std::process::id();
    let Some(sb) = sandbox(Policy {
        unix_sockets: vec![
            ok.display().to_string(),
            format!("{}/", lure.display()),
            format!("@ferrule-test-ok-{tag}"),
        ],
        ..no_tmp(Mode::WorkspaceWrite)
    }) else {
        return;
    };
    if let Err(why) = sb.unix_enforcement() {
        // GitHub's Linux runners are VMs with a 6.x kernel: the supervisor
        // must work there, so a skip would hide a regression. (Inside
        // Docker's default seccomp profile, `pidfd_getfd` is refused.)
        assert!(
            !(cfg!(target_os = "linux") && std::env::var_os("GITHUB_ACTIONS").is_some()),
            "the Unix-socket supervisor should work on CI: {why}"
        );
        eprintln!("skipping: Unix sockets aren't enforced here ({why})");
        return;
    }
    let ws = tempfile::tempdir().unwrap();
    let wsp = dunce::canonicalize(ws.path()).unwrap();
    // ferrule's own listener in the workspace: the command didn't make it.
    let foreign = wsp.join("foreign.sock");
    let _l3 = UnixListener::bind(&foreign).unwrap();

    let mut targets = vec![
        format!("allowed={}", ok.display()),
        format!("denied={}", no.display()),
        format!("symlink={}", lure.join("docker.sock").display()),
        format!("own={}", wsp.join("mine.sock").display()),
        format!("foreign={}", foreign.display()),
    ];
    #[cfg(target_os = "linux")]
    let (_a1, _a2) = {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::SocketAddr;
        let bind =
            |n: &str| UnixListener::bind_addr(&SocketAddr::from_abstract_name(n).unwrap()).unwrap();
        targets.push(format!("abstract_ok=@ferrule-test-ok-{tag}"));
        targets.push(format!("abstract_no=@ferrule-test-no-{tag}"));
        (
            bind(&format!("ferrule-test-ok-{tag}")),
            bind(&format!("ferrule-test-no-{tag}")),
        )
    };

    let got = unix_probe(&sb, &wsp, &targets);
    eprintln!("{got}");
    // libtest prints the first result on its own `test … ... ` line.
    let line = |label: &str| {
        got.lines()
            .map(|l| l.rsplit(" ... ").next().unwrap_or(l))
            .find_map(|l| l.strip_prefix(&format!("{label}: ")))
            .unwrap_or_else(|| panic!("no {label} in {got}"))
            .to_string()
    };
    assert_eq!(line("allowed"), "ok");
    assert_eq!(line("own"), "ok", "a socket the command made itself");
    assert_ne!(line("denied"), "ok");
    assert_ne!(line("symlink"), "ok", "a symlink doesn't launder a socket");
    if cfg!(target_os = "linux") {
        // EACCES from the supervisor, not a failure for another reason.
        assert_eq!(line("denied"), "errno 13");
        assert_eq!(line("symlink"), "errno 13");
        assert_eq!(line("foreign"), "errno 13", "the workspace isn't a pass");
        assert_eq!(line("abstract_ok"), "ok");
        assert_eq!(line("abstract_no"), "errno 13");
    }
}
