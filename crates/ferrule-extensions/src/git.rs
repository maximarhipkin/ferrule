//! Git sources through the `git` CLI: clone, resolve the requested rev to
//! one commit, check that commit out, and at every later load prove the
//! checkout is still exactly that commit with nothing changed.
//!
//! Never interactive (`GIT_TERMINAL_PROMPT=0`), never submodules, and the
//! owner's system/global git config is not consulted, so a credential
//! helper or `url.insteadOf` can't change what gets fetched.

use crate::error::{ExtError, Result};
use std::path::Path;
use std::process::{Command, Stdio};

fn git(cwd: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "http.lowSpeedLimit=1000",
        "-c",
        "http.lowSpeedTime=60",
        "-c",
        "advice.detachedHead=false",
    ])
    .args(args)
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .env("GIT_CONFIG_GLOBAL", null_device())
    .env_remove("GIT_DIR")
    .env_remove("GIT_WORK_TREE")
    .stdin(Stdio::null());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let out = cmd
        .output()
        .map_err(|e| ExtError::Git(format!("can't run git: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(ExtError::Git(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            err.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

pub fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Clone `url` into `dest` (which must not exist), resolve `rev` (default:
/// the remote's HEAD) to a commit and check it out detached. Returns the
/// full SHA.
pub fn fetch_pinned(url: &str, rev: Option<&str>, dest: &Path) -> Result<String> {
    let dest_s = dest.to_string_lossy();
    git(
        None,
        &[
            "clone",
            "--quiet",
            "--no-checkout",
            "--no-recurse-submodules",
            "--",
            url,
            &dest_s,
        ],
    )?;
    let sha = resolve(dest, rev.unwrap_or("HEAD"))?;
    git(Some(dest), &["checkout", "--quiet", "--detach", &sha])?;
    Ok(sha)
}

/// A rev as the remote named it: a remote branch first (a local `main`
/// doesn't exist after `--no-checkout`), then a tag or a commit.
fn resolve(repo: &Path, rev: &str) -> Result<String> {
    if rev.starts_with('-') {
        return Err(crate::error::refused("a git rev can't start with `-`"));
    }
    let candidates = [format!("refs/remotes/origin/{rev}"), rev.to_string()];
    for c in &candidates {
        let spec = format!("{c}^{{commit}}");
        if let Ok(sha) = git(Some(repo), &["rev-parse", "--verify", "--quiet", &spec]) {
            if is_full_sha(&sha) {
                return Ok(sha.to_lowercase());
            }
        }
    }
    Err(ExtError::Git(format!(
        "`{rev}` is not a branch, tag or commit of the repo"
    )))
}

/// The checkout is at `sha` and has no changed, added or removed files.
pub fn verify(dir: &Path, sha: &str) -> Result<()> {
    if !dir.join(".git").exists() {
        return Err(ExtError::Git(format!(
            "{} is not a git checkout",
            dir.display()
        )));
    }
    let head = git(Some(dir), &["rev-parse", "HEAD"])?;
    if !head.eq_ignore_ascii_case(sha) {
        return Err(ExtError::Git(format!(
            "checkout moved: HEAD is {head}, pinned {sha}"
        )));
    }
    let status = git(
        Some(dir),
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored=no",
        ],
    )?;
    if !status.is_empty() {
        return Err(ExtError::Git(format!(
            "checkout was modified: {}",
            status.lines().take(3).collect::<Vec<_>>().join("; ")
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod testrepo {
    use super::*;
    use std::fs;

    /// A repo in `dir` with `files`, committed; returns the SHA.
    pub fn init(dir: &Path, files: &[(&str, &str)]) -> String {
        fs::create_dir_all(dir).unwrap();
        git(Some(dir), &["init", "--quiet", "-b", "main"]).unwrap();
        commit(dir, files, "initial")
    }

    pub fn commit(dir: &Path, files: &[(&str, &str)], msg: &str) -> String {
        for (path, text) in files {
            let p = dir.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
        git(Some(dir), &["add", "-A"]).unwrap();
        git(
            Some(dir),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--quiet",
                "-m",
                msg,
            ],
        )
        .unwrap();
        git(Some(dir), &["rev-parse", "HEAD"]).unwrap()
    }

    pub fn file_url(dir: &Path) -> String {
        let p = dir.to_string_lossy().replace('\\', "/");
        if p.starts_with('/') {
            format!("file://{p}")
        } else {
            format!("file:///{p}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testrepo::*;
    use super::*;
    use std::fs;

    #[test]
    fn a_branch_is_pinned_to_its_commit_and_later_commits_dont_move_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let first = init(&repo, &[("server.py", "v1")]);
        let dest = tmp.path().join("co");
        let sha = fetch_pinned(&file_url(&repo), Some("main"), &dest).unwrap();
        assert_eq!(sha, first);
        verify(&dest, &sha).unwrap();

        let second = commit(&repo, &[("server.py", "v2")], "second");
        assert_ne!(first, second);
        verify(&dest, &sha).unwrap();
        assert_eq!(fs::read_to_string(dest.join("server.py")).unwrap(), "v1");

        let dest2 = tmp.path().join("co2");
        assert_eq!(
            fetch_pinned(&file_url(&repo), None, &dest2).unwrap(),
            second
        );
        let dest3 = tmp.path().join("co3");
        assert_eq!(
            fetch_pinned(&file_url(&repo), Some(&first[..10]), &dest3).unwrap(),
            first
        );
    }

    #[test]
    fn a_tampered_or_moved_checkout_fails_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init(&repo, &[("server.py", "v1")]);
        let dest = tmp.path().join("co");
        let sha = fetch_pinned(&file_url(&repo), None, &dest).unwrap();

        fs::write(dest.join("server.py"), "evil").unwrap();
        assert!(verify(&dest, &sha)
            .unwrap_err()
            .to_string()
            .contains("modified"));
        git(Some(&dest), &["checkout", "--quiet", "--", "server.py"]).unwrap();
        fs::write(dest.join("extra.py"), "x").unwrap();
        assert!(verify(&dest, &sha).is_err());
        fs::remove_file(dest.join("extra.py")).unwrap();
        verify(&dest, &sha).unwrap();
        assert!(verify(&dest, &"0".repeat(40))
            .unwrap_err()
            .to_string()
            .contains("moved"));
    }

    #[test]
    fn unknown_and_flag_like_revs_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init(&repo, &[("a", "a")]);
        let url = file_url(&repo);
        assert!(fetch_pinned(&url, Some("nope"), &tmp.path().join("a")).is_err());
        assert!(fetch_pinned(&url, Some("--upload-pack=x"), &tmp.path().join("b")).is_err());
    }
}
