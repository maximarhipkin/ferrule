//! Git worktrees for children that work on their parent's repo, and the
//! verifier's throwaway snapshot. Plain `git` calls; when one fails the
//! child shares its parent's workspace and a note says why.
//!
//! These calls run in ferrule's own process, outside any sandbox, on a
//! repo a child could write to, so every one turns off the two things git
//! would otherwise run from it: hooks and an fsmonitor.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A child's own copy of the repo.
#[derive(Debug, Clone)]
pub(crate) struct Made {
    /// Where the child works: the worktree, or the same subdirectory of it
    /// its parent was in.
    pub workspace: PathBuf,
    pub worktree: PathBuf,
    /// `ferrule/<id>` for a worker; none for a snapshot.
    pub branch: Option<String>,
    /// The commit it started at.
    pub base: String,
    pub notes: Vec<String>,
}

/// Why a child shares its parent's workspace: not a repo (nothing to say)
/// or a note for the spawn result.
pub(crate) type Shared = Option<String>;

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.fsmonitor=false", "-c"])
        .arg(format!("core.hooksPath={}", no_hooks().display()))
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|e| format!("git couldn't run: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        let last = err.lines().last().unwrap_or(err);
        Err(format!("git {} failed: {last}", args[0]))
    }
}

/// A directory that holds no hooks.
fn no_hooks() -> PathBuf {
    std::env::temp_dir().join("ferrule-no-git-hooks")
}

struct Repo {
    top: PathBuf,
    common: PathBuf,
}

fn repo_of(dir: &Path) -> Option<Repo> {
    let out = git(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ],
    )
    .ok()?;
    let mut lines = out.lines();
    Some(Repo {
        top: PathBuf::from(lines.next()?),
        common: PathBuf::from(lines.next()?),
    })
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// `dir`'s place under the repo's top, reproduced in the copy at `copy`.
fn below_top(repo: &Repo, dir: &Path, copy: &Path) -> PathBuf {
    match canonical(dir).strip_prefix(canonical(&repo.top)) {
        Ok(rel) if !rel.as_os_str().is_empty() => copy.join(rel),
        _ => copy.to_path_buf(),
    }
}

/// The git dir a worktree child may write to so it can commit: the repo's
/// common dir, but only when it's inside the parent's workspace, so the
/// child gets nothing its parent couldn't already write.
pub(crate) fn writable_git_dir(worktree: &Path, parent_workspace: &Path) -> Option<PathBuf> {
    let repo = repo_of(worktree)?;
    canonical(&repo.common)
        .starts_with(canonical(parent_workspace))
        .then_some(repo.common)
}

/// A worktree on a new branch `ferrule/<id>` at the parent's HEAD, under
/// `root`.
pub(crate) fn for_worker(parent_workspace: &Path, root: &Path, id: &str) -> Result<Made, Shared> {
    let repo = repo_of(parent_workspace).ok_or(None)?;
    if !canonical(&repo.common).starts_with(canonical(parent_workspace)) {
        return Err(Some(
            "It shares your workspace: the repo's git directory is outside your workspace, so a \
             child in its own worktree couldn't commit."
                .into(),
        ));
    }
    let shared = |e: String| Some(format!("It shares your workspace: {e}."));
    let base = git(parent_workspace, &["rev-parse", "HEAD"])
        .map_err(|_| shared("the repo has no commits yet".into()))?;
    std::fs::create_dir_all(root).map_err(|e| shared(e.to_string()))?;
    let path = root.join(id);
    let branch = format!("ferrule/{id}");
    git(
        parent_workspace,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &branch,
            &path.to_string_lossy(),
            &base,
        ],
    )
    .map_err(shared)?;
    let mut notes = vec![format!(
        "It has its own worktree on branch {branch}; merge or cherry-pick that branch when you want its \
         work."
    )];
    if git(parent_workspace, &["status", "--porcelain"]).is_ok_and(|s| !s.is_empty()) {
        notes.push(
            "Your uncommitted changes aren't in its copy; commit them first if it needs them."
                .into(),
        );
    }
    Ok(Made {
        workspace: below_top(&repo, parent_workspace, &path),
        worktree: path,
        branch: Some(branch),
        base,
        notes,
    })
}

/// A detached worktree with the parent's HEAD plus its uncommitted diff
/// and untracked files: a copy to run the tests in, thrown away after.
/// Replaces any earlier snapshot at the same place.
pub(crate) fn snapshot(parent_workspace: &Path, root: &Path, id: &str) -> Result<Made, Shared> {
    let repo = repo_of(parent_workspace).ok_or(None)?;
    let shared = |e: String| {
        Some(format!(
            "It works in your workspace, read-only, because the snapshot failed: {e}."
        ))
    };
    let base = git(parent_workspace, &["rev-parse", "HEAD"])
        .map_err(|_| shared("the repo has no commits yet".into()))?;
    std::fs::create_dir_all(root).map_err(|e| shared(e.to_string()))?;
    let path = root.join(id);
    discard(parent_workspace, &path);
    git(
        parent_workspace,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &path.to_string_lossy(),
            &base,
        ],
    )
    .map_err(shared)?;
    let copied = copy_changes(&repo, &path, root, id);
    if let Err(e) = copied {
        discard(parent_workspace, &path);
        return Err(shared(e));
    }
    Ok(Made {
        workspace: below_top(&repo, parent_workspace, &path),
        worktree: path,
        branch: None,
        base,
        notes: vec![
            "It checks a disposable copy of your work (your commits, uncommitted changes and new \
             files); nothing it does there reaches your files."
                .into(),
        ],
    })
}

fn copy_changes(repo: &Repo, to: &Path, root: &Path, id: &str) -> Result<(), String> {
    let patch = root.join(format!("{id}.patch"));
    let diff = Command::new("git")
        .arg("-C")
        .arg(&repo.top)
        .args(["-c", "core.fsmonitor=false"])
        .args(["diff", "HEAD", "--binary", "--no-ext-diff", "--no-color"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !diff.status.success() {
        return Err("git diff failed".into());
    }
    if !diff.stdout.is_empty() {
        std::fs::write(&patch, &diff.stdout).map_err(|e| e.to_string())?;
        let applied = git(
            to,
            &[
                "apply",
                "--binary",
                "--whitespace=nowarn",
                &patch.to_string_lossy(),
            ],
        );
        let _ = std::fs::remove_file(&patch);
        applied?;
    }
    let untracked = git(
        &repo.top,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    for rel in untracked.split('\0').filter(|r| !r.is_empty()) {
        let from = repo.top.join(rel);
        // Only plain files: a link could point anywhere.
        if !std::fs::symlink_metadata(&from).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let dest = to.join(rel);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::copy(&from, &dest).map_err(|e| format!("copying {rel}: {e}"))?;
    }
    Ok(())
}

/// Removes a worktree whatever state it's in.
pub(crate) fn discard(repo_dir: &Path, path: &Path) {
    if !path.exists() {
        return;
    }
    let _ = git(
        repo_dir,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    );
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
    let _ = git(repo_dir, &["worktree", "prune"]);
}

/// Closes a worker's worktree: commits what's left uncommitted to its
/// branch, removes the worktree, and deletes the branch only if it holds
/// nothing beyond `base`. Returns a note when the branch is kept.
pub(crate) fn close(
    repo_dir: &Path,
    worktree: &Path,
    branch: &str,
    base: &str,
    id: &str,
) -> Option<String> {
    let mut saved = false;
    if worktree.exists() && git(worktree, &["status", "--porcelain"]).is_ok_and(|s| !s.is_empty()) {
        let has_identity = git(worktree, &["config", "user.email"]).is_ok_and(|e| !e.is_empty());
        let mut args = Vec::new();
        if !has_identity {
            args.extend([
                "-c",
                "user.name=ferrule",
                "-c",
                "user.email=ferrule@localhost",
            ]);
        }
        let message = format!("ferrule: agent {id}'s uncommitted work, saved when it was closed");
        args.extend(["commit", "-q", "--no-verify", "-m", &message]);
        saved = git(worktree, &["add", "-A"]).is_ok() && git(worktree, &args).is_ok();
        if !saved {
            // Keep the files rather than lose them.
            return Some(format!(
                "Agent {id}'s worktree {} is kept: its changes couldn't be committed.",
                worktree.display()
            ));
        }
    }
    discard(repo_dir, worktree);
    let ahead = git(
        repo_dir,
        &["rev-list", "--count", &format!("{base}..{branch}")],
    )
    .ok()
    .and_then(|n| n.parse::<u64>().ok());
    match ahead {
        Some(0) => {
            let _ = git(repo_dir, &["branch", "-q", "-D", branch]);
            None
        }
        Some(n) => Some(format!(
            "Kept branch {branch} ({n} commit{}{}).",
            if n == 1 { "" } else { "s" },
            if saved {
                ", the last one its uncommitted work"
            } else {
                ""
            }
        )),
        None => Some(format!("Kept branch {branch}.")),
    }
}
