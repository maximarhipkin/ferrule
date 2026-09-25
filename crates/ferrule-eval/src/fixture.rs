//! One throwaway directory per task run: the workspace the agent works in
//! and a private state dir beside it (see `docs/m14-eval.md`, "Fixture
//! lifecycle").

use crate::suite::Task;
use anyhow::{bail, Context as _, Result};
use ferrule_core::verify::Verifier;
use ferrule_core::ToolContext;
use ferrule_sandbox::Sandbox;
use ferrule_tools::CommandVerifier;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

pub struct Fixture {
    root: PathBuf,
    /// The agent's workspace, canonical.
    pub workspace: PathBuf,
    /// Outside the workspace: the engineered variant's memory database.
    pub state: PathBuf,
    keep: bool,
}

impl Fixture {
    /// Creates `<parent>/<name>/{ws,state}` and fills `ws` from the task:
    /// the fixture directory, the inline files, `setup`, then git.
    pub async fn prepare(
        parent: &Path,
        name: &str,
        task: &Task,
        sandbox: &Arc<Sandbox>,
        keep: bool,
    ) -> Result<Fixture> {
        let root = parent.join(name);
        if root.exists() {
            std::fs::remove_dir_all(&root)
                .with_context(|| format!("clearing {}", root.display()))?;
        }
        let ws = root.join("ws");
        let state = root.join("state");
        std::fs::create_dir_all(&ws).with_context(|| format!("creating {}", ws.display()))?;
        std::fs::create_dir_all(&state)?;
        // macOS: /var/folders/… is /private/var/folders/…; the sandbox and
        // the file tools compare canonical paths.
        let workspace = dunce::canonicalize(ws)?;
        let fixture = Fixture {
            root,
            workspace,
            state: dunce::canonicalize(state)?,
            keep,
        };

        if let Some(src) = &task.fixture {
            copy_dir(src, &fixture.workspace)
                .with_context(|| format!("copying fixture {}", src.display()))?;
        }
        for (rel, content) in &task.files {
            let rel = Path::new(rel);
            if rel.is_absolute()
                || rel
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                bail!("task {:?}: file {:?} leaves the workspace", task.id, rel);
            }
            let path = fixture.workspace.join(rel);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, content)?;
        }
        if let Some(setup) = &task.setup {
            let ran = run_command(setup, &fixture.workspace, sandbox, 300).await;
            if let Err(out) = ran {
                bail!("task {:?}: setup `{setup}` failed:\n{out}", task.id);
            }
        }
        if task.git {
            git_init(&fixture.workspace)?;
        }
        Ok(fixture)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// Runs `command` in `dir` under `sandbox`: `Ok(())` on exit 0, otherwise
/// the tail of its output.
pub async fn run_command(
    command: &str,
    dir: &Path,
    sandbox: &Arc<Sandbox>,
    timeout_secs: u64,
) -> Result<(), String> {
    let v = CommandVerifier::new(command, sandbox.clone(), Duration::from_secs(timeout_secs));
    let ctx = ToolContext {
        workspace: dir.to_path_buf(),
        max_output_chars: 4_000,
    };
    v.verify(&ctx).await
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// `git init` + one commit, as `ferrule-eval`: the user's own git identity
/// and hooks are never needed or used.
fn git_init(ws: &Path) -> Result<()> {
    let git = |args: &[&str]| -> Result<()> {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=ferrule-eval",
                "-c",
                "user.email=eval@ferrule.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(ws)
            .output()
            .context("running git (is it installed?)")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    };
    git(&["init", "-q"])?;
    git(&["add", "-A"])?;
    git(&["commit", "-q", "--allow-empty", "-m", "fixture"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::Grade;
    use std::collections::BTreeMap;

    fn task() -> Task {
        Task {
            id: "t".into(),
            prompt: "p".into(),
            tags: vec![],
            fixture: None,
            files: BTreeMap::from([("src/a.txt".into(), "hello".into())]),
            setup: Some("echo made > made.txt".into()),
            git: true,
            check: None,
            max_iterations: None,
            timeout_secs: None,
            grade: Grade::default(),
            fingerprint: String::new(),
        }
    }

    #[tokio::test]
    async fn prepares_files_setup_and_git_then_cleans_up() {
        let parent = tempfile::tempdir().unwrap();
        let sb = Arc::new(Sandbox::off());
        let root;
        {
            let f = Fixture::prepare(parent.path(), "t--naive", &task(), &sb, false)
                .await
                .unwrap();
            root = f.root().to_path_buf();
            let ws = &f.workspace;
            assert_eq!(
                std::fs::read_to_string(ws.join("src/a.txt")).unwrap(),
                "hello"
            );
            assert!(ws.join("made.txt").exists());
            assert!(ws.join(".git").is_dir());
            assert!(f.state.is_dir());
            assert!(!f.state.starts_with(ws));
        }
        assert!(!root.exists(), "removed when dropped");
    }

    #[tokio::test]
    async fn a_failing_setup_is_an_error_and_files_cannot_escape() {
        let parent = tempfile::tempdir().unwrap();
        let sb = Arc::new(Sandbox::off());
        let mut t = task();
        t.git = false;
        t.setup = Some("echo nope; exit 4".into());
        let err = match Fixture::prepare(parent.path(), "x", &t, &sb, false).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("setup failure ignored"),
        };
        assert!(err.contains("nope"), "{err}");

        let mut t = task();
        t.setup = None;
        t.files = BTreeMap::from([("../out.txt".into(), "x".into())]);
        assert!(Fixture::prepare(parent.path(), "y", &t, &sb, false)
            .await
            .is_err());
    }
}
