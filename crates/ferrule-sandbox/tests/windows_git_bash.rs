//! Git Bash under the Windows backend. MSYS ACLs its own pipes and shared
//! memory to the user's SID, which a write-restricted token can't match, so
//! it can't hold tier 1. What must hold is that it never half-works: the
//! sandbox either confines it or reports itself degraded, and says why.
//! A process of its own, because the shell is picked once per process.

#![cfg(windows)]

use ferrule_sandbox::{Backend, Mode, Policy, Sandbox, Shell, ShellKind};
use std::path::PathBuf;
use std::process::Stdio;

#[test]
fn git_bash_is_confined_or_reported_degraded() {
    let installed = ["ProgramFiles", "ProgramW6432"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|d| PathBuf::from(d).join("Git").join("bin").join("bash.exe"))
        .any(|p| p.is_file());
    if !installed {
        eprintln!("skipping: Git Bash isn't installed");
        return;
    }
    std::env::set_var(
        ferrule_sandbox::launch::LAUNCHER_VAR,
        env!("CARGO_BIN_EXE_ferrule-sandbox-launch"),
    );
    std::env::set_var(ferrule_sandbox::SHELL_VAR, "bash");
    let shell = Shell::get();
    assert_eq!((shell.kind, shell.name), (ShellKind::Posix, "Git Bash"));

    let policy = Policy {
        mode: Mode::WorkspaceWrite,
        tmp: false,
        state_dir: Some(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sandbox")),
        ..Policy::default()
    };
    // `require` turns the degrade into a refusal to start.
    let refused = Sandbox::new(Policy {
        require: true,
        ..policy.clone()
    });
    let sb = Sandbox::new(policy).unwrap();
    match sb.degraded() {
        Some(why) => {
            eprintln!("Git Bash degrades: {why}");
            assert_eq!(sb.backend(), Backend::None);
            assert!(why.contains("probe"), "{why}");
            assert!(refused.is_err());
        }
        None => {
            // If a later Git or Windows lets it run, it must be confined.
            eprintln!("Git Bash holds tier 1");
            assert!(refused.is_ok());
            assert_eq!(sb.backend(), Backend::Windows);
            let ws = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let target = outside.path().join("x.txt");
            let script = format!(
                "echo in > ok.txt; echo out > '{}'",
                target.display().to_string().replace('\\', "/")
            );
            let out = sb
                .command(&shell.program, shell.args(&script), ws.path())
                .unwrap()
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(ws.path().join("ok.txt").exists(), "{out:?}");
            assert!(!target.exists(), "wrote outside the workspace: {out:?}");
        }
    }
}
