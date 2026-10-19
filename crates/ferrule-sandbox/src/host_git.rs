//! Git run by ferrule itself — as the owner, outside the sandbox — on a
//! repo a sandboxed command may have written to: a child's worktree, the
//! verifier's snapshot, an eval fixture (docs/sandbox.md, "Ferrule's own
//! git").
//!
//! Git runs commands its config names: hooks, an fsmonitor, a signing
//! program, and the filter drivers that `.gitattributes` or
//! `.git/info/attributes` assign. A sandboxed command can write
//! `.git/config` and those attribute files, so every call here pins the
//! first three off and empties every filter driver the repo's own config
//! defines. Drivers from the owner's global or system config still run, so
//! git-lfs (`git lfs install` writes `filter.lfs.*` there) keeps working;
//! one the repo redefines is off entirely.
//!
//! A caller that diffs passes `--no-ext-diff --no-textconv`: an empty
//! `diff.<x>.textconv` isn't "none" to git but a command that fails.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Long enough for a checkout of a big repo; bounds one that never ends (a
/// FIFO planted as `.git/HEAD` blocks git's read forever).
pub const TIMEOUT: Duration = Duration::from_secs(300);

/// `git -C dir args…` with stdout and stderr captured. A run past
/// [`TIMEOUT`] is killed and is an error, and so is a repo config this
/// can't neutralise.
pub fn git(dir: &Path, args: &[&str]) -> io::Result<Output> {
    let listed = run(
        base(dir).args(["config", "--list", "--show-scope", "-z"]),
        TIMEOUT,
    )?;
    if !listed.status.success() {
        let err = String::from_utf8_lossy(&listed.stderr);
        return Err(io::Error::other(format!(
            "git config --list failed: {}",
            err.trim().lines().last().unwrap_or_default()
        )));
    }
    let mut cmd = base(dir);
    for name in repo_filters(&listed.stdout)? {
        for key in ["clean", "smudge", "process"] {
            cmd.arg("-c").arg(format!("filter.{name}.{key}="));
        }
        cmd.arg("-c").arg(format!("filter.{name}.required=false"));
    }
    run(cmd.args(args), TIMEOUT)
}

fn base(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        // `/dev/null` holds no hooks on any platform and nobody can add one;
        // a temp dir could be written by a sandboxed command.
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        // ferrule's own commits are never signed: `gpg.program` is config
        // too, and the owner's global gpgsign would prompt for a commit
        // that isn't theirs.
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    cmd
}

/// The filter drivers named by config that isn't the owner's (anything but
/// system, global and command-line: `.git/config`, a worktree's
/// `config.worktree`, and files they include), from `git config --list
/// --show-scope -z`: `scope NUL key [LF value] NUL`, repeated.
fn repo_filters(listing: &[u8]) -> io::Result<BTreeSet<String>> {
    let mut fields = listing.split(|&b| b == 0);
    let mut names = BTreeSet::new();
    while let (Some(scope), Some(entry)) = (fields.next(), fields.next()) {
        if matches!(scope, b"system" | b"global" | b"command") {
            continue;
        }
        let key = entry.split(|&b| b == b'\n').next().unwrap_or_default();
        let Some(rest) = key.strip_prefix(b"filter.") else {
            continue;
        };
        // The subsection may hold dots; the variable can't.
        let Some(dot) = rest.iter().rposition(|&b| b == b'.') else {
            continue;
        };
        // `-c` splits at the first `=`, and a mangled name isn't pinned.
        match std::str::from_utf8(&rest[..dot]) {
            Ok(name) if !name.contains('=') => {
                names.insert(name.to_string());
            }
            _ => {
                return Err(io::Error::other(
                    "the repo's git config names a filter driver ferrule can't turn off",
                ))
            }
        }
    }
    Ok(names)
}

/// Runs `cmd` to its end or `timeout`, whichever comes first.
fn run(cmd: &mut Command, timeout: Duration) -> io::Result<Output> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    // Both pipes drain while git runs, so a big diff can't fill one and stall.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                // ponytail: polls; git calls take milliseconds, so this adds
                // at most 5 ms to each.
                std::thread::sleep(Duration::from_millis(5));
            }
            waited => {
                let _ = child.kill();
                let _ = child.wait();
                waited?;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("git was stopped after {}s", timeout.as_secs()),
                ));
            }
        }
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_drivers_outside_the_owners_config_are_turned_off() {
        let listing = b"system\0filter.lfs.clean\ngit-lfs clean -- %f\0\
            global\0filter.lfs.process\ngit-lfs filter-process\0\
            command\0filter.cli.clean\nx\0\
            local\0filter.evil.clean\ntouch pwned; cat\0\
            worktree\0filter.a.b.smudge\nx\0\
            local\0filter.flag.required\0\
            local\0core.editor\nvi\0";
        let names: Vec<String> = repo_filters(listing).unwrap().into_iter().collect();
        assert_eq!(names, ["a.b", "evil", "flag"]);
        assert!(repo_filters(b"local\0filter.a=b.clean\nx\0").is_err());
        assert!(repo_filters(b"local\0filter.\xff.clean\nx\0").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_run_past_the_timeout_is_killed() {
        let started = Instant::now();
        let err = run(Command::new("sleep").arg("5"), Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
