//! The Windows tier-1 backend against real processes, for both shells a
//! command can run in: Git Bash and Windows PowerShell. Needs no admin;
//! runs on the windows-latest CI runner. A shell that isn't installed is
//! skipped with a note.

#![cfg(windows)]

use ferrule_sandbox::{Backend, Mode, Policy, Sandbox, Shell, ShellKind};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

fn shells() -> Vec<Shell> {
    let mut out = Vec::new();
    let bash = ["ProgramFiles", "ProgramW6432"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|d| PathBuf::from(d).join("Git").join("bin").join("bash.exe"))
        .find(|p| p.is_file());
    match bash {
        Some(program) => out.push(Shell {
            program,
            kind: ShellKind::Posix,
            name: "Git Bash",
        }),
        None => eprintln!("skipping Git Bash: not installed"),
    }
    out.push(Shell {
        program: "powershell.exe".into(),
        kind: ShellKind::PowerShell,
        name: "Windows PowerShell",
    });
    out
}

/// Capability SIDs kept with the build, not in the runner's profile.
fn policy(mode: Mode, tmp: bool) -> Policy {
    Policy {
        mode,
        tmp,
        require: true,
        state_dir: Some(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sandbox")),
        ..Policy::default()
    }
}

fn sandbox(policy: Policy) -> Sandbox {
    // The launcher this build made, not whatever sits next to the test exe.
    std::env::set_var(
        ferrule_sandbox::launch::LAUNCHER_VAR,
        env!("CARGO_BIN_EXE_ferrule-sandbox-launch"),
    );
    let sb = Sandbox::new(policy).expect("the Windows backend should start");
    assert_eq!(sb.backend(), Backend::Windows);
    sb
}

fn run(sb: &Sandbox, shell: &Shell, ws: &Path, script: &str) -> Output {
    let script = match shell.kind {
        ShellKind::Posix => script.to_string(),
        ShellKind::PowerShell => format!("$ErrorActionPreference = 'Stop'\n{script}"),
    };
    sb.command(&shell.program, shell.args(&script), ws)
        .unwrap()
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn show(out: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `path` as the shell wants it written: forward slashes for bash.
fn p(shell: &Shell, path: &Path) -> String {
    let s = path.display().to_string();
    match shell.kind {
        ShellKind::Posix => s.replace('\\', "/"),
        ShellKind::PowerShell => s,
    }
}

fn write(shell: &Shell, path: &str, text: &str) -> String {
    match shell.kind {
        ShellKind::Posix => format!("echo {text} > '{path}'"),
        ShellKind::PowerShell => format!("Set-Content -Path '{path}' -Value {text}"),
    }
}

fn read(shell: &Shell, path: &str) -> String {
    match shell.kind {
        ShellKind::Posix => format!("cat '{path}'"),
        ShellKind::PowerShell => format!("Get-Content -Path '{path}'"),
    }
}

#[test]
fn writes_land_in_the_workspace_and_nowhere_else() {
    let sb = sandbox(policy(Mode::WorkspaceWrite, false));
    for shell in shells() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("readable.txt"), "open\n").unwrap();
        let ok = run(&sb, &shell, ws.path(), &write(&shell, "ok.txt", "in"));
        assert!(ok.status.success(), "{}: {}", shell.name, show(&ok));
        assert!(ws.path().join("ok.txt").exists(), "{}", shell.name);

        let target = outside.path().join("x.txt");
        let out = run(
            &sb,
            &shell,
            ws.path(),
            &write(&shell, &p(&shell, &target), "out"),
        );
        assert!(!out.status.success(), "{}: {}", shell.name, show(&out));
        assert!(
            !target.exists(),
            "{}: wrote outside the workspace",
            shell.name
        );

        // Reads outside stay open.
        let out = run(
            &sb,
            &shell,
            ws.path(),
            &read(&shell, &p(&shell, &outside.path().join("readable.txt"))),
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("open"),
            "{}: {}",
            shell.name,
            show(&out)
        );
    }
}

#[test]
fn read_only_mode_refuses_even_the_workspace() {
    let sb = sandbox(policy(Mode::ReadOnly, false));
    for shell in shells() {
        let ws = tempfile::tempdir().unwrap();
        let out = run(&sb, &shell, ws.path(), &write(&shell, "x.txt", "no"));
        assert!(!out.status.success(), "{}: {}", shell.name, show(&out));
        assert!(!ws.path().join("x.txt").exists(), "{}", shell.name);
    }
}

#[test]
fn temp_dirs_are_writable_when_enabled() {
    let sb = sandbox(policy(Mode::WorkspaceWrite, true));
    for shell in shells() {
        let ws = tempfile::tempdir().unwrap();
        let name = format!("ferrule-sb-{}-{}.txt", std::process::id(), shell.kind as u8);
        let target = std::env::temp_dir().join(&name);
        let out = run(
            &sb,
            &shell,
            ws.path(),
            &write(&shell, &p(&shell, &target), "t"),
        );
        assert!(out.status.success(), "{}: {}", shell.name, show(&out));
        assert!(target.exists(), "{}", shell.name);
        let _ = std::fs::remove_file(target);
    }
}

#[test]
fn saved_keys_and_denied_paths_are_unreadable() {
    let data = tempfile::tempdir().unwrap();
    let private = data.path().join("private");
    std::fs::create_dir(&private).unwrap();
    std::fs::write(private.join("secrets.env"), "API_KEY=sk-live\n").unwrap();
    std::fs::write(data.path().join("config.toml"), "open\n").unwrap();
    let mine = tempfile::tempdir().unwrap();
    std::fs::write(mine.path().join("token"), "sk-mine\n").unwrap();
    let sb = sandbox(Policy {
        hidden: vec![private.clone()],
        deny_read: vec![mine.path().to_path_buf()],
        ..policy(Mode::WorkspaceWrite, false)
    });
    let state = tempfile::tempdir().unwrap();
    // An MCP server's sandbox keeps the same denies.
    let helper = sb.for_helper(state.path(), &[]);
    for shell in shells() {
        let ws = tempfile::tempdir().unwrap();
        for sb in [&sb, &helper] {
            for secret in [private.join("secrets.env"), mine.path().join("token")] {
                let out = run(sb, &shell, ws.path(), &read(&shell, &p(&shell, &secret)));
                assert!(!out.status.success(), "{}: {}", shell.name, show(&out));
                assert!(
                    !String::from_utf8_lossy(&out.stdout).contains("sk-"),
                    "{}: {}",
                    shell.name,
                    show(&out)
                );
            }
            let out = run(
                sb,
                &shell,
                ws.path(),
                &read(&shell, &p(&shell, &data.path().join("config.toml"))),
            );
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("open"),
                "{}: {}",
                shell.name,
                show(&out)
            );
        }
    }
    // Ferrule itself, and anything else of the user's, still reads it.
    assert_eq!(
        std::fs::read_to_string(private.join("secrets.env")).unwrap(),
        "API_KEY=sk-live\n"
    );
    std::fs::write(private.join("new"), "x").unwrap();
}

#[test]
fn the_job_takes_the_whole_tree_down() {
    let sb = sandbox(policy(Mode::WorkspaceWrite, false));
    for shell in shells() {
        let ws = tempfile::tempdir().unwrap();
        // A grandchild that writes a file after a few seconds, unless it's
        // killed first; `started` says it's running.
        let script = match shell.kind {
            ShellKind::Posix => "(sleep 5; echo late > late.txt) & echo up > started.txt; sleep 60",
            ShellKind::PowerShell => {
                "Start-Process -NoNewWindow -FilePath powershell.exe -ArgumentList \
                 '-NoProfile','-Command','Start-Sleep 5; Set-Content -Path late.txt -Value late'; \
                 Start-Sleep 2; Set-Content -Path started.txt -Value up; Start-Sleep 60"
            }
        };
        let mut child = sb
            .command(&shell.program, shell.args(script), ws.path())
            .unwrap()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !ws.path().join("started.txt").exists() {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "{}: never started",
                shell.name
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        std::thread::sleep(Duration::from_secs(8));
        assert!(
            !ws.path().join("late.txt").exists(),
            "{}: the grandchild outlived the launcher",
            shell.name
        );
    }
}

#[test]
fn the_process_limit_holds() {
    let sb = sandbox(Policy {
        process_limit: 1,
        ..policy(Mode::WorkspaceWrite, false)
    });
    let shell = Shell {
        program: "cmd.exe".into(),
        kind: ShellKind::Posix,
        name: "cmd",
    };
    let ws = tempfile::tempdir().unwrap();
    // cmd itself is the one process allowed; the child it starts isn't.
    let out = sb
        .command(
            &shell.program,
            ["/d", "/c", "cmd /d /c echo inner"],
            ws.path(),
        )
        .unwrap()
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("inner"),
        "{}",
        show(&out)
    );
}

#[test]
fn the_exit_code_and_output_come_back() {
    let sb = sandbox(policy(Mode::WorkspaceWrite, false));
    for shell in shells() {
        let ws = tempfile::tempdir().unwrap();
        let script = match shell.kind {
            ShellKind::Posix => "echo 'a \"quoted\" arg'; exit 7",
            ShellKind::PowerShell => "Write-Output 'a \"quoted\" arg'; exit 7",
        };
        let out = run(&sb, &shell, ws.path(), script);
        assert_eq!(out.status.code(), Some(7), "{}: {}", shell.name, show(&out));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("a \"quoted\" arg"),
            "{}: {}",
            shell.name,
            show(&out)
        );
    }
}

/// Runs this binary's `open_process_helper` against our own pid, either
/// sandboxed or not.
fn open_self(sb: &Sandbox) -> Output {
    let me = std::env::current_exe().unwrap();
    let ws = tempfile::tempdir().unwrap();
    sb.command(
        &me,
        [
            "--exact",
            "open_process_helper",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ],
        ws.path(),
    )
    .unwrap()
    .env("FERRULE_OPEN_PID", std::process::id().to_string())
    .stdin(Stdio::null())
    .output()
    .unwrap()
}

#[test]
#[ignore = "helper, run by ferrules_process_is_shut_to_the_sandbox"]
fn open_process_helper() {
    let Some(pid) = std::env::var("FERRULE_OPEN_PID").ok() else {
        return;
    };
    let readable = ferrule_sandbox::can_read_process(pid.parse().unwrap());
    println!("readable={readable}");
}

#[test]
fn ferrules_process_is_shut_to_the_sandbox() {
    // The Windows twin of /proc/<ppid>/environ: ferrule's memory holds the
    // real secrets the proxy swaps in.
    let sb = sandbox(policy(Mode::WorkspaceWrite, false));
    let out = open_self(&sb);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("readable=false"),
        "{}",
        show(&out)
    );
    // Not sandboxed, the same user still gets in: only the token is shut out.
    let out = open_self(&Sandbox::off());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("readable=true"),
        "{}",
        show(&out)
    );
}
