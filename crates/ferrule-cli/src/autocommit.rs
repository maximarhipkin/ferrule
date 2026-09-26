//! M29: `[agent] auto_commit`. Each run that changed files ends in one git
//! commit of exactly the files the agent changed, and `ferrule undo` /
//! `/undo` take the latest one back (docs/m29-edit-mechanics.md §4).
//!
//! - A path is the agent's when it's dirty at the end of the run and
//!   wasn't at the start. A path the owner had dirty is never committed.
//! - Git runs through the sandbox (it reads `.git/hooks` and `.git/config`,
//!   which the agent can write), and the repo's own hooks apply.
//! - Nothing here pushes, fetches or touches a remote.

use anyhow::{anyhow, bail, Result};
use ferrule_core::{RunEnd, RunObserver};
use ferrule_sandbox::Sandbox;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

/// The trailer every agent commit carries; `undo` keys on it.
pub const TRAILER: &str = "Ferrule-Auto-Commit";
pub const DEFAULT_AUTHOR: &str = "ferrule <ferrule@localhost>";
const BRANCH_PREFIX: &str = "ferrule/";
const SUBJECT_CHARS: usize = 72;
const BODY_LINES: usize = 8;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BranchMode {
    /// Commit on a `ferrule/auto-…` branch made at the first commit.
    #[default]
    New,
    /// Commit on whatever branch HEAD is on (the explicit opt-in).
    Current,
}

/// Git through the sandbox, in one directory.
#[derive(Clone)]
struct Git {
    sandbox: Arc<Sandbox>,
    dir: PathBuf,
}

struct Out {
    ok: bool,
    stdout: Vec<u8>,
    stderr: String,
}

impl Git {
    fn run(&self, args: &[&str]) -> Result<Out> {
        let mut all = vec!["-c", "core.quotepath=off"];
        all.extend_from_slice(args);
        let mut cmd = self
            .sandbox
            .command("git", &all, &self.dir)
            .map_err(|e| anyhow!("couldn't start git: {e}"))?;
        cmd.stdin(Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0");
        let out = cmd.output().map_err(|e| anyhow!("couldn't run git: {e}"))?;
        Ok(Out {
            ok: out.status.success(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }

    /// Stdout, trimmed, or an error with git's own words.
    fn text(&self, args: &[&str]) -> Result<String> {
        let out = self.run(args)?;
        if !out.ok {
            bail!("git {}: {}", args.join(" "), last_lines(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn succeeds(&self, args: &[&str]) -> bool {
        self.run(args).is_ok_and(|o| o.ok)
    }

    /// The work tree's root; `None` outside a work tree.
    fn toplevel(&self) -> Option<PathBuf> {
        self.text(&["rev-parse", "--show-toplevel"])
            .ok()
            .filter(|t| !t.is_empty())
            .map(|t| dunce::canonicalize(&t).unwrap_or_else(|_| PathBuf::from(t)))
    }

    /// Paths `git status` lists (root-relative, `/`-separated), renames'
    /// sources included; ignored files aren't listed.
    fn dirty(&self) -> Result<BTreeSet<String>> {
        let out = self.run(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ])?;
        if !out.ok {
            bail!("git status: {}", last_lines(&out.stderr));
        }
        let mut paths = BTreeSet::new();
        let mut fields = out.stdout.split(|b| *b == 0).filter(|f| !f.is_empty());
        while let Some(entry) = fields.next() {
            if entry.len() < 4 {
                continue;
            }
            let (xy, path) = entry.split_at(3);
            paths.insert(String::from_utf8_lossy(path).into_owned());
            if matches!(xy[0], b'R' | b'C') {
                if let Some(from) = fields.next() {
                    paths.insert(String::from_utf8_lossy(from).into_owned());
                }
            }
        }
        Ok(paths)
    }

    fn git_path_exists(&self, root: &Path, name: &str) -> bool {
        self.text(&["rev-parse", "--git-path", name])
            .is_ok_and(|p| root.join(p).exists())
    }
}

/// `:(top,literal)path`: root-relative and never a glob, wherever git runs.
fn pathspecs(paths: &[&String]) -> Vec<String> {
    paths.iter().map(|p| format!(":(top,literal){p}")).collect()
}

fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(4)..];
    if tail.is_empty() {
        "failed".into()
    } else {
        tail.join(" / ")
    }
}

fn content_hash(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    Some(h.finish())
}

/// `Name <email>` → (name, email).
fn parse_author(author: &str) -> Result<(String, String)> {
    let (name, rest) = author
        .trim()
        .split_once('<')
        .ok_or_else(|| anyhow!("auto_commit_author `{author}` isn't `Name <email>`"))?;
    let email = rest
        .strip_suffix('>')
        .ok_or_else(|| anyhow!("auto_commit_author `{author}` isn't `Name <email>`"))?;
    let name = name.trim();
    if name.is_empty() || email.trim().is_empty() {
        bail!("auto_commit_author `{author}` isn't `Name <email>`");
    }
    Ok((name.to_string(), email.trim().to_string()))
}

/// What the workspace looked like when the run started.
struct Start {
    root: PathBuf,
    /// Dirty paths and their content hash (`None`: deleted).
    dirty: BTreeMap<String, Option<u64>>,
}

/// Commits a run's files; [`AutoCommitObserver`] wires it to the agent.
pub struct AutoCommit {
    git: Git,
    author: String,
    name: String,
    email: String,
    branch: BranchMode,
    start: Mutex<Option<Start>>,
}

impl AutoCommit {
    pub fn new(
        workspace: &Path,
        sandbox: Arc<Sandbox>,
        author: &str,
        branch: BranchMode,
    ) -> Result<Self> {
        let (name, email) = parse_author(author)?;
        Ok(Self {
            git: Git {
                sandbox,
                dir: workspace.to_path_buf(),
            },
            author: format!("{name} <{email}>"),
            name,
            email,
            branch,
            start: Mutex::new(None),
        })
    }

    /// The snapshot, or `None` when there's nothing to commit onto: not a
    /// work tree, or no commit yet.
    fn snapshot(&self) -> Option<Start> {
        let root = self.git.toplevel()?;
        if !self
            .git
            .succeeds(&["rev-parse", "-q", "--verify", "HEAD^{commit}"])
        {
            return None;
        }
        let dirty = match self.git.dirty() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("auto-commit: {e}");
                return None;
            }
        };
        let dirty = dirty
            .into_iter()
            .map(|p| {
                let hash = content_hash(&root.join(&p));
                (p, hash)
            })
            .collect();
        Some(Start { root, dirty })
    }

    /// The commit (or why not) after a run; `None` when the run changed
    /// nothing and there's nothing to say.
    fn finish(&self, start: Start, run: &RunEnd<'_>) -> Option<String> {
        let now = match self.git.dirty() {
            Ok(d) => d,
            Err(e) => return Some(format!("not committed: {e}")),
        };
        let agent: Vec<&String> = now
            .iter()
            .filter(|p| !start.dirty.contains_key(*p))
            .collect();
        let owners: Vec<&String> = start
            .dirty
            .iter()
            .filter(|(p, h)| content_hash(&start.root.join(p)) != **h)
            .map(|(p, _)| p)
            .collect();
        let left = (!owners.is_empty()).then(|| {
            format!(
                "; left uncommitted: {} (you had changes there)",
                owners
                    .iter()
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
        let left = left.unwrap_or_default();
        if agent.is_empty() {
            return (!left.is_empty()).then(|| format!("nothing committed{left}"));
        }
        match self.commit(&start.root, &agent, run) {
            Ok(done) => Some(format!("{done}{left}")),
            Err(e) => Some(format!(
                "not committed ({} file{} left in the working tree): {e}{left}",
                agent.len(),
                if agent.len() == 1 { "" } else { "s" }
            )),
        }
    }

    fn commit(&self, root: &Path, paths: &[&String], run: &RunEnd<'_>) -> Result<String> {
        for (name, what) in [
            ("MERGE_HEAD", "a merge"),
            ("rebase-merge", "a rebase"),
            ("rebase-apply", "a rebase"),
            ("CHERRY_PICK_HEAD", "a cherry-pick"),
            ("REVERT_HEAD", "a revert"),
        ] {
            if self.git.git_path_exists(root, name) {
                bail!("{what} is in progress");
            }
        }
        let branch = self
            .git
            .text(&["symbolic-ref", "-q", "--short", "HEAD"])
            .ok();
        let branch = match (self.branch, branch) {
            (BranchMode::Current, None) => bail!("HEAD is detached"),
            (BranchMode::Current, Some(b)) => b,
            (BranchMode::New, Some(b)) if b.starts_with(BRANCH_PREFIX) => b,
            (BranchMode::New, _) => self.new_branch()?,
        };
        let specs = pathspecs(paths);
        let mut add = vec!["add", "-A", "--"];
        add.extend(specs.iter().map(String::as_str));
        self.git.text(&add)?;

        let (subject, body) = message(run);
        let user_name = format!("user.name={}", self.name);
        let user_email = format!("user.email={}", self.email);
        let author = format!("--author={}", self.author);
        let trailer = format!("{TRAILER}: {}", run.session_id);
        let mut commit = vec![
            "-c",
            &user_name,
            "-c",
            &user_email,
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--only",
            &author,
            "-m",
            &subject,
        ];
        if !body.is_empty() {
            commit.extend(["-m", &body]);
        }
        commit.extend(["-m", &trailer, "--"]);
        commit.extend(specs.iter().map(String::as_str));
        let out = self.git.run(&commit)?;
        if !out.ok {
            // `add` staged them; put the index back as the run found it.
            let mut reset = vec!["reset", "-q", "--"];
            reset.extend(specs.iter().map(String::as_str));
            let _ = self.git.run(&reset);
            bail!("git commit: {}", last_lines(&out.stderr));
        }
        let sha = self.git.text(&["rev-parse", "--short", "HEAD"])?;
        Ok(format!(
            "{sha} on {branch}: {} file{}",
            paths.len(),
            if paths.len() == 1 { "" } else { "s" }
        ))
    }

    /// `ferrule/auto-<yyyymmdd-hhmmss>` at HEAD, switched to; the working
    /// tree and index stay as they are.
    fn new_branch(&self) -> Result<String> {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        for n in 0..100 {
            let name = match n {
                0 => format!("{BRANCH_PREFIX}auto-{stamp}"),
                n => format!("{BRANCH_PREFIX}auto-{stamp}-{n}"),
            };
            let exists =
                self.git
                    .succeeds(&["rev-parse", "-q", "--verify", &format!("refs/heads/{name}")]);
            if !exists {
                self.git.text(&["switch", "-q", "-c", &name])?;
                return Ok(name);
            }
        }
        bail!("couldn't pick a free ferrule/auto-* branch name")
    }
}

/// The subject (the request's first line) and the body (the answer's
/// first lines, and why the run stopped short if it did).
fn message(run: &RunEnd<'_>) -> (String, String) {
    let first = run
        .goal
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("agent run");
    let subject = match first.char_indices().nth(SUBJECT_CHARS - 1) {
        Some((at, _)) => format!("{}…", &first[..at]),
        None => first.to_string(),
    };
    let mut body: Vec<String> = run
        .answer
        .unwrap_or("")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(BODY_LINES)
        .map(|l| l.trim_end().to_string())
        .collect();
    if let Some(why) = run.incomplete {
        body.push(format!("(stopped short: {why})"));
    } else if run.answer.is_none() {
        body.push("(the run ended with an error)".into());
    }
    (format!("[agent] {subject}"), body.join("\n"))
}

/// The [`RunObserver`]: git is blocking, so it runs off the async threads.
pub struct AutoCommitObserver(Arc<AutoCommit>);

impl AutoCommitObserver {
    pub fn new(inner: AutoCommit) -> Self {
        Self(Arc::new(inner))
    }
}

#[async_trait::async_trait]
impl RunObserver for AutoCommitObserver {
    fn name(&self) -> &str {
        "auto-commit"
    }

    async fn begin(&self, _session_id: &str) {
        let ac = self.0.clone();
        let start = tokio::task::spawn_blocking(move || ac.snapshot())
            .await
            .ok()
            .flatten();
        *self.0.start.lock().unwrap_or_else(|e| e.into_inner()) = start;
    }

    async fn end(&self, run: &RunEnd<'_>) -> Option<String> {
        let start = self
            .0
            .start
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        let ac = self.0.clone();
        let (session_id, goal) = (run.session_id.to_string(), run.goal.to_string());
        let answer = run.answer.map(str::to_string);
        let incomplete = run.incomplete.map(str::to_string);
        let note = tokio::task::spawn_blocking(move || {
            let run = RunEnd {
                session_id: &session_id,
                goal: &goal,
                answer: answer.as_deref(),
                incomplete: incomplete.as_deref(),
            };
            ac.finish(start, &run)
        })
        .await
        .unwrap_or_else(|e| Some(format!("not committed: {e}")));
        if let Some(note) = &note {
            tracing::info!(session = run.session_id, "auto-commit: {note}");
        }
        note
    }
}

/// Takes the latest agent commit back, when it's safe: HEAD carries the
/// trailer and has a parent, and none of its files changed since. The
/// branch moves back (compare-and-swap) and those files are restored;
/// nothing else in the working tree or index is touched.
pub fn undo(workspace: &Path, sandbox: Arc<Sandbox>) -> Result<String> {
    let git = Git {
        sandbox,
        dir: workspace.to_path_buf(),
    };
    if git.toplevel().is_none() {
        bail!("{} isn't in a git work tree", workspace.display());
    }
    let head = git
        .text(&["rev-parse", "-q", "--verify", "HEAD^{commit}"])
        .map_err(|_| anyhow!("nothing to undo: no commit yet"))?;
    let msg = git.text(&["log", "-1", "--format=%B", &head])?;
    let tagged = msg
        .lines()
        .any(|l| l.trim_start().starts_with(&format!("{TRAILER}:")));
    let subject = msg.lines().next().unwrap_or("").to_string();
    let short = &head[..head.len().min(7)];
    if !tagged {
        bail!("HEAD ({short} \"{subject}\") isn't an agent commit; nothing undone");
    }
    let parent = git
        .text(&["rev-parse", "-q", "--verify", &format!("{head}^")])
        .map_err(|_| anyhow!("{short} has no parent; nothing undone"))?;
    let changed = git.run(&[
        "diff-tree",
        "--no-commit-id",
        "-r",
        "--name-status",
        "--no-renames",
        "-z",
        &head,
    ])?;
    if !changed.ok {
        bail!("git diff-tree: {}", last_lines(&changed.stderr));
    }
    let mut fields = changed.stdout.split(|b| *b == 0).filter(|f| !f.is_empty());
    let (mut added, mut restored) = (Vec::new(), Vec::new());
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        let path = String::from_utf8_lossy(path).into_owned();
        match status.first() {
            Some(b'A') => added.push(path),
            _ => restored.push(path),
        }
    }
    let all: Vec<&String> = added.iter().chain(&restored).collect();
    if !all.is_empty() {
        let mut status = vec!["status", "--porcelain=v1", "--untracked-files=all", "--"];
        let specs = pathspecs(&all);
        status.extend(specs.iter().map(String::as_str));
        let since = git.run(&status)?;
        if !since.ok {
            bail!("git status: {}", last_lines(&since.stderr));
        }
        let since = String::from_utf8_lossy(&since.stdout).into_owned();
        if let Some(line) = since.lines().find(|l| !l.trim().is_empty()) {
            bail!(
                "`{}` changed since {short}; nothing undone (commit or discard that first)",
                line.get(3..).unwrap_or(line)
            );
        }
    }
    git.text(&["update-ref", "-m", "ferrule undo", "HEAD", &parent, &head])?;
    if !restored.is_empty() {
        let refs: Vec<&String> = restored.iter().collect();
        let specs = pathspecs(&refs);
        let mut checkout = vec!["checkout", "-q", &parent, "--"];
        checkout.extend(specs.iter().map(String::as_str));
        git.text(&checkout)?;
    }
    if !added.is_empty() {
        let refs: Vec<&String> = added.iter().collect();
        let specs = pathspecs(&refs);
        let mut rm = vec!["rm", "-q", "-f", "--"];
        rm.extend(specs.iter().map(String::as_str));
        git.text(&rm)?;
    }
    let branch = git
        .text(&["symbolic-ref", "-q", "--short", "HEAD"])
        .unwrap_or_else(|_| "a detached HEAD".into());
    Ok(format!(
        "undid {short} \"{subject}\" on {branch}: {} file{} restored",
        all.len(),
        if all.len() == 1 { "" } else { "s" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Plain git in `dir`, for the test's own setup and checks.
    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=owner",
                "-c",
                "user.email=owner@example.com",
            ])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(dir: &Path, rel: &str) -> String {
        std::fs::read_to_string(dir.join(rel)).unwrap()
    }

    /// A repo with one commit on `main`: a.txt, b.txt, keep.txt.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        sh(d, &["init", "-q"]);
        // Windows runners set core.autocrlf, which checks files out CRLF.
        sh(d, &["config", "core.autocrlf", "false"]);
        write(d, "a.txt", "a\n");
        write(d, "b.txt", "b\n");
        write(d, "keep.txt", "keep\n");
        write(d, ".gitignore", "target/\n");
        sh(d, &["add", "-A"]);
        sh(d, &["commit", "-q", "-m", "init"]);
        dir
    }

    fn ac(dir: &Path, branch: BranchMode) -> AutoCommit {
        AutoCommit::new(dir, Arc::new(Sandbox::off()), DEFAULT_AUTHOR, branch).unwrap()
    }

    fn end<'a>(goal: &'a str, answer: &'a str) -> RunEnd<'a> {
        RunEnd {
            session_id: "s-1",
            goal,
            answer: Some(answer),
            incomplete: None,
        }
    }

    /// One run: snapshot, then `work`, then the commit note.
    fn run(ac: &AutoCommit, dir: &Path, work: impl FnOnce(&Path)) -> Option<String> {
        let start = ac.snapshot();
        work(dir);
        ac.finish(
            start?,
            &end("Fix the parser\nand more", "Fixed it.\n\nDetails."),
        )
    }

    /// The core promise: only the files the agent changed are committed;
    /// the owner's dirty, staged and untracked work stays exactly as it
    /// was, even a file both of them touched.
    #[test]
    fn only_the_agents_files_are_committed() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let d = dir.path();
        // The owner's work in progress: modified, staged, untracked.
        write(d, "keep.txt", "owner edit\n");
        write(d, "b.txt", "owner staged\n");
        sh(d, &["add", "b.txt"]);
        write(d, "notes/owner.txt", "untracked\n");
        let ac = ac(d, BranchMode::Current);

        let note = run(&ac, d, |d| {
            write(d, "a.txt", "agent\n");
            write(d, "src/new.rs", "fn main() {}\n");
            write(d, "keep.txt", "owner edit\nagent too\n");
            write(d, "target/out.o", "ignored\n");
        })
        .unwrap();
        assert!(note.contains("on main: 2 files"), "{note}");
        assert!(note.contains("left uncommitted: `keep.txt`"), "{note}");

        let files = sh(d, &["show", "--name-only", "--format=", "HEAD"]);
        assert_eq!(files.lines().collect::<Vec<_>>(), ["a.txt", "src/new.rs"]);
        // The owner's state: keep.txt still modified, b.txt still staged,
        // the untracked file still untracked.
        let status = sh(d, &["status", "--porcelain", "--untracked-files=all"]);
        assert_eq!(
            status.lines().collect::<Vec<_>>(),
            ["M  b.txt", " M keep.txt", "?? notes/owner.txt"]
        );
        assert_eq!(sh(d, &["show", ":b.txt"]), "owner staged");
        assert_eq!(read(d, "keep.txt"), "owner edit\nagent too\n");

        let log = sh(d, &["log", "-1", "--format=%an <%ae>|%s|%b"]);
        assert!(
            log.starts_with("ferrule <ferrule@localhost>|[agent] Fix the parser|Fixed it."),
            "{log}"
        );
        assert!(log.contains("Ferrule-Auto-Commit: s-1"), "{log}");
    }

    #[test]
    fn the_owners_branch_is_left_alone_by_default() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let d = dir.path();
        let main = sh(d, &["rev-parse", "main"]);
        let ac = ac(d, BranchMode::New);
        let note = run(&ac, d, |d| write(d, "a.txt", "one\n")).unwrap();
        assert!(note.contains("on ferrule/auto-"), "{note}");
        let branch = sh(d, &["symbolic-ref", "--short", "HEAD"]);
        assert!(branch.starts_with("ferrule/auto-"));
        assert_eq!(sh(d, &["rev-parse", "main"]), main, "main never moves");
        // The next run keeps committing on the same agent branch.
        let note = run(&ac, d, |d| write(d, "b.txt", "two\n")).unwrap();
        assert!(note.contains(&format!("on {branch}")), "{note}");
        assert_eq!(sh(d, &["rev-list", "--count", "main..HEAD"]), "2");
    }

    #[test]
    fn a_run_that_changed_nothing_commits_nothing() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let d = dir.path();
        let ac = ac(d, BranchMode::Current);
        assert_eq!(run(&ac, d, |_| {}), None);
        assert_eq!(sh(d, &["rev-list", "--count", "HEAD"]), "1");
        // Outside a repo, or before its first commit: no snapshot at all.
        let bare = tempfile::tempdir().unwrap();
        assert!(ac_for(bare.path()).snapshot().is_none());
        sh(bare.path(), &["init", "-q"]);
        assert!(ac_for(bare.path()).snapshot().is_none());
    }

    fn ac_for(d: &Path) -> AutoCommit {
        ac(d, BranchMode::Current)
    }

    #[test]
    fn detached_head_in_current_mode_is_not_committed() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let d = dir.path();
        sh(d, &["switch", "-q", "--detach"]);
        let ac = ac(d, BranchMode::Current);
        let note = run(&ac, d, |d| write(d, "a.txt", "x\n")).unwrap();
        assert!(
            note.contains("not committed") && note.contains("detached"),
            "{note}"
        );
        assert_eq!(sh(d, &["status", "--porcelain"]), "M a.txt");
    }

    /// A repo hook that rejects the commit is obeyed: the changes stay in
    /// the working tree, unstaged, and the note says why.
    #[cfg(unix)]
    #[test]
    fn a_rejecting_pre_commit_hook_is_obeyed() {
        use std::os::unix::fs::PermissionsExt;
        if !git_available() {
            return;
        }
        let dir = repo();
        let d = dir.path();
        let hook = d.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho 'no commits today' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ac = ac(d, BranchMode::Current);
        let note = run(&ac, d, |d| write(d, "new.txt", "x\n")).unwrap();
        assert!(note.contains("no commits today"), "{note}");
        assert_eq!(sh(d, &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(sh(d, &["status", "--porcelain"]), "?? new.txt");
    }

    /// Undo restores modified and deleted files, removes added ones, and
    /// leaves the owner's edits elsewhere alone; a second undo refuses on
    /// a commit that isn't the agent's.
    #[test]
    fn undo_takes_the_agent_commit_back() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let d = dir.path();
        let before = sh(d, &["rev-parse", "HEAD"]);
        let ac = ac(d, BranchMode::Current);
        run(&ac, d, |d| {
            write(d, "a.txt", "changed\n");
            std::fs::remove_file(d.join("b.txt")).unwrap();
            write(d, "added.txt", "new\n");
        })
        .unwrap();
        write(d, "keep.txt", "owner, after the run\n");

        let said = undo(d, Arc::new(Sandbox::off())).unwrap();
        assert!(
            said.contains("[agent] Fix the parser") && said.contains("3 files"),
            "{said}"
        );
        assert_eq!(sh(d, &["rev-parse", "HEAD"]), before);
        assert_eq!(read(d, "a.txt"), "a\n");
        assert_eq!(read(d, "b.txt"), "b\n");
        assert!(!d.join("added.txt").exists());
        assert_eq!(sh(d, &["status", "--porcelain"]), "M keep.txt");

        let err = undo(d, Arc::new(Sandbox::off())).unwrap_err().to_string();
        assert!(err.contains("isn't an agent commit"), "{err}");
    }

    #[test]
    fn undo_refuses_when_its_files_changed_since() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let d = dir.path();
        let ac = ac(d, BranchMode::Current);
        run(&ac, d, |d| write(d, "a.txt", "agent\n")).unwrap();
        let head = sh(d, &["rev-parse", "HEAD"]);
        write(d, "a.txt", "agent\nowner on top\n");
        let err = undo(d, Arc::new(Sandbox::off())).unwrap_err().to_string();
        assert!(err.contains("`a.txt` changed since"), "{err}");
        assert_eq!(sh(d, &["rev-parse", "HEAD"]), head);
        assert_eq!(read(d, "a.txt"), "agent\nowner on top\n");
    }

    /// Commits and undos never reach a remote: a bare `origin` keeps its
    /// refs, and the local repo's remote-tracking refs don't move either.
    #[test]
    fn nothing_is_ever_pushed() {
        if !git_available() {
            return;
        }
        let dir = repo();
        let d = dir.path();
        let origin = tempfile::tempdir().unwrap();
        sh(origin.path(), &["init", "-q", "--bare"]);
        let url = origin.path().to_string_lossy().to_string();
        sh(d, &["remote", "add", "origin", &url]);
        sh(d, &["push", "-q", "origin", "main"]);
        let refs = || {
            sh(
                origin.path(),
                &["for-each-ref", "--format=%(refname) %(objectname)"],
            )
        };
        let tracking = || {
            sh(
                d,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/remotes",
                ],
            )
        };
        let (origin_before, tracking_before) = (refs(), tracking());

        for mode in [BranchMode::New, BranchMode::Current] {
            let ac = ac(d, mode);
            run(&ac, d, |d| {
                write(d, "a.txt", format!("{mode:?}\n").as_str())
            })
            .unwrap();
        }
        undo(d, Arc::new(Sandbox::off())).unwrap();
        assert_eq!(refs(), origin_before);
        assert_eq!(tracking(), tracking_before);
        // And no code path here could: the module never says it.
        let src = include_str!("autocommit.rs");
        let code = &src[..src.find("#[cfg(test)]").unwrap()];
        for word in ["\"push\"", "\"fetch\"", "\"remote\"", "\"pull\""] {
            assert!(!code.contains(word), "{word} in autocommit.rs");
        }
    }

    #[test]
    fn a_bad_author_is_refused_and_the_subject_is_clipped() {
        assert!(parse_author("just a name").is_err());
        assert!(parse_author("<a@b>").is_err());
        assert_eq!(
            parse_author(" Bot  <bot@x> ").unwrap(),
            ("Bot".to_string(), "bot@x".to_string())
        );
        let long = "x".repeat(100);
        let (subject, body) = message(&RunEnd {
            session_id: "s",
            goal: &long,
            answer: None,
            incomplete: None,
        });
        assert_eq!(subject.chars().count(), "[agent] ".len() + SUBJECT_CHARS);
        assert!(subject.ends_with('…'));
        assert_eq!(body, "(the run ended with an error)");
    }
}
