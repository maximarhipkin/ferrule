//! Real enforcement, against the running kernel. Each test skips (with a
//! note on stderr) where the backend isn't available, so they're safe in
//! any CI; where it is, they prove the kernel refuses what it should.

use ferrule_sandbox::{Mode, Policy, Sandbox};
use std::path::Path;
use std::process::{Output, Stdio};

fn sandbox(policy: Policy) -> Option<Sandbox> {
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
    assert!(String::from_utf8_lossy(&out.stderr).contains("Permission denied"));

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
fn secret_env_vars_do_not_reach_the_command() {
    std::env::set_var("FERRULE_TEST_API_KEY", "sk-should-not-leak");
    std::env::set_var("FERRULE_TEST_PLAIN", "visible");
    let sb = Sandbox::new(Policy {
        mode: Mode::Off,
        ..Policy::default()
    })
    .unwrap();
    let out = sh(&sb, Path::new("/"), "env");
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
