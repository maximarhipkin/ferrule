//! M58.5: the grader-leak tripwire. The agent must neither find a copy of
//! its task's grader or check in its workspace nor be able to read the
//! originals: one of Harness²'s Codex rollouts downloaded its own rubric.
//! The suite file, the grader and check scripts its commands name, and the
//! files beside them in their own directory are hidden from the agent's
//! sandbox; ferrule runs the checks and graders in one that isn't. Before
//! each run the hiding is probed, and before and after it the workspace is
//! searched for copies. A leak voids the verdict, loudly.

use crate::fixture::run_command;
use crate::suite::{Suite, Task, SUITE_FILE};
use anyhow::Result;
use ferrule_sandbox::Sandbox;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Files shorter than this aren't searched for: a few bytes match by
/// chance.
const MIN_BYTES: u64 = 32;
/// The most workspace entries one search looks at.
const MAX_ENTRIES: usize = 20_000;

/// The agent's sandbox: `sandbox` with `files` hidden.
pub fn hiding(sandbox: &Sandbox, files: &[PathBuf]) -> Result<Sandbox> {
    let mut policy = sandbox.policy().clone();
    policy.deny_read.extend(files.iter().cloned());
    let mut hidden = Sandbox::new(policy).map_err(anyhow::Error::msg)?;
    if let Some(why) = sandbox.refusal() {
        hidden = hidden.refuse(why);
    }
    Ok(hidden.with_egress(sandbox.egress().cloned()))
}

/// What the agent of `task` must not see: the suite file, the files under
/// the suite's directory its grade command and check name, and the other
/// files in their directory when that isn't the suite's own (a grader's
/// test data; `graders/` and `checks/` in the starter suite, never its
/// `solutions/`, which the mock model plays).
pub fn protected(task: &Task, suite: &Suite) -> Vec<PathBuf> {
    let mut named = Vec::new();
    let root = suite.dir.to_string_lossy();
    let commands = [task.grade.command.as_deref(), task.check.as_deref()];
    for cmd in commands.into_iter().flatten() {
        let mut rest = cmd;
        while let Some(at) = rest.find(root.as_ref()) {
            let quote = rest[..at]
                .chars()
                .last()
                .filter(|c| matches!(c, '"' | '\''));
            let tail = &rest[at..];
            let end = match quote {
                Some(q) => tail.find(q),
                None => tail.find(char::is_whitespace),
            }
            .unwrap_or(tail.len());
            named.push(PathBuf::from(&tail[..end]));
            rest = &tail[end..];
        }
    }
    let mut out = vec![suite.dir.join(SUITE_FILE)];
    for file in named.into_iter().filter(|f| f.is_file()) {
        let beside = file
            .parent()
            .filter(|d| *d != suite.dir.as_path() && d.starts_with(&suite.dir));
        if let Some(dir) = beside {
            out.extend(files_under(dir));
        }
        out.push(file);
    }
    out.retain(|p| p.is_file());
    out.sort();
    out.dedup();
    out
}

/// Every file under `dir`, at most [`MAX_ENTRIES`] entries looked at.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0;
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return out;
            }
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(t) if t.is_file() => out.push(entry.path()),
                _ => {}
            }
        }
    }
    out
}

/// Why the run can't be trusted, if it can't: a copy of a protected file
/// in `workspace`, or (when `probe`) one the agent's sandbox doesn't hide.
/// Every file is checked against the sandbox's deny list, and on Unix one
/// is tried with `cat` where the agent's commands run.
pub async fn check(
    workspace: &Path,
    files: &[PathBuf],
    agent: &Arc<Sandbox>,
    probe: bool,
) -> Option<String> {
    if let Some(why) = copies(workspace, files) {
        return Some(why);
    }
    let first = files.first().filter(|_| probe)?;
    if !agent.hides_reads() {
        return Some("no OS sandbox hides the graders from the agent".into());
    }
    let denies = agent.read_denies(workspace);
    for file in files {
        let canonical = dunce::canonicalize(file).unwrap_or_else(|_| file.clone());
        if !denies.iter().any(|d| canonical.starts_with(d)) {
            return Some(format!(
                "the agent's sandbox doesn't hide {}",
                file.display()
            ));
        }
    }
    if cfg!(unix) {
        let quoted = first.to_string_lossy().replace('\'', r"'\''");
        let probe = format!("cat -- '{quoted}' > /dev/null 2>&1");
        if run_command(&probe, workspace, agent, 30).await.is_ok() {
            return Some(format!("the agent's sandbox read {}", first.display()));
        }
    }
    None
}

/// A copy of (or a link to) one of `files` in `workspace`, said in words.
pub fn copies(workspace: &Path, files: &[PathBuf]) -> Option<String> {
    let wanted: Vec<(u64, u64, &PathBuf, Option<PathBuf>)> = files
        .iter()
        .filter_map(|f| {
            let bytes = std::fs::read(f).ok()?;
            let len = bytes.len() as u64;
            Some((
                len,
                ferrule_core::fnv64(&bytes),
                f,
                dunce::canonicalize(f).ok(),
            ))
        })
        .collect();
    let mut stack = vec![workspace.to_path_buf()];
    let mut seen = 0;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return None;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let rel = path.strip_prefix(workspace).unwrap_or(&path).display();
            if kind.is_symlink() {
                let target = dunce::canonicalize(&path).ok();
                let hit = wanted.iter().find(|w| w.3.is_some() && w.3 == target);
                if let Some((_, _, file, _)) = hit {
                    return Some(format!(
                        "{rel} in the workspace links to {}",
                        file.display()
                    ));
                }
            } else if kind.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push(path);
                }
            } else if let Ok(meta) = entry.metadata() {
                let len = meta.len();
                let maybe: Vec<_> = wanted
                    .iter()
                    .filter(|w| w.0 == len && len >= MIN_BYTES)
                    .collect();
                if maybe.is_empty() {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                let hash = ferrule_core::fnv64(&bytes);
                if let Some((_, _, file, _)) = maybe.iter().find(|w| w.1 == hash) {
                    return Some(format!(
                        "{rel} in the workspace is a copy of {}",
                        file.display()
                    ));
                }
            }
        }
    }
    None
}
