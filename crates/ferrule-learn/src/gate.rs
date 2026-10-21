//! The success gate (`docs/m16-learning-loop.md` §5): an added or edited
//! lesson is kept only if the episode's goal, re-run in a scratch copy of
//! the workspace with the candidate playbook in its prompt, finishes and
//! then passes the check twice.

use crate::budget::Meter;
use crate::episode::Episode;
use ferrule_core::{AgentEvent, Budget, HarnessProfile, LedgerSink, Provider, ToolContext};
use ferrule_core::{Transcript, Verifier};
use ferrule_eval::variant::{self, Variant};
use ferrule_sandbox::Sandbox;
use ferrule_tools::CommandVerifier;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// A workspace bigger than this isn't copied; the delta is rejected.
pub const MAX_FILES: usize = 20_000;
pub const MAX_BYTES: u64 = 200 * 1024 * 1024;
/// Directory names never copied into the scratch workspace.
const SKIP_DIRS: &[&str] = &["target", "node_modules"];

/// One gate run.
pub struct GateRun<'a> {
    /// The proposal's number within the pass: `gate-<n>`.
    pub n: usize,
    pub episode: &'a Episode,
    /// The candidate playbook's `[Playbook]` section.
    pub playbook_block: Option<&'a str>,
    /// Tags, prices and charges the gate agent's calls.
    pub sink: Arc<dyn LedgerSink>,
    pub meter: Arc<Meter>,
    /// Where the gate agent's transcript goes.
    pub pass_dir: &'a Path,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GateVerdict {
    pub passed: bool,
    pub reason: String,
}

impl GateVerdict {
    pub(crate) fn fail(reason: impl Into<String>) -> Self {
        Self {
            passed: false,
            reason: reason.into(),
        }
    }
}

#[async_trait::async_trait]
pub trait Gate: Send + Sync {
    /// The command a verdict rests on; `None` rejects every add and edit
    /// unless the gate also [`probe`](Gate::probe)s.
    fn check(&self) -> Option<String>;
    async fn run(&self, run: GateRun<'_>) -> GateVerdict;
    /// M53: re-run the episode with the candidate lesson, no check, and
    /// bring back the probe's transcript tail for the judge. `None` =
    /// this gate can't probe (the default).
    async fn probe(&self, run: GateRun<'_>) -> Option<ProbeOut> {
        let _ = run;
        None
    }
}

/// What a probe (M53) brings back.
#[derive(Debug, Clone)]
pub struct ProbeOut {
    /// The re-run finished (no `incomplete` note, inside the timeout).
    pub finished: bool,
    /// Why it didn't, when it didn't.
    pub incomplete: Option<String>,
    /// The probe transcript's tail, rendered for the judge.
    pub tail: String,
}

/// The real gate: ferrule's engineered harness (the one `ferrule eval`
/// builds) in a scratch copy of the workspace.
pub struct WorkspaceGate {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub profile: HarnessProfile,
    pub sandbox: Arc<Sandbox>,
    pub workspace: PathBuf,
    pub check: Option<String>,
    pub max_iterations: usize,
    /// Scratch copies are made under here and removed afterwards.
    pub scratch_root: PathBuf,
    /// Paths never copied (the data dir when it sits in the workspace).
    pub skip: Vec<PathBuf>,
    pub run_timeout: Duration,
    pub check_timeout: Duration,
}

#[async_trait::async_trait]
impl Gate for WorkspaceGate {
    fn check(&self) -> Option<String> {
        self.check.clone()
    }

    async fn run(&self, r: GateRun<'_>) -> GateVerdict {
        let Some(check) = self.check.clone() else {
            return GateVerdict::fail("no check to gate on");
        };
        let pass = r
            .pass_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("pass");
        let scratch = self.scratch_root.join(format!("scratch-{pass}-{}", r.n));
        let _ = fs::remove_dir_all(&scratch);
        let verdict = self.run_in(&scratch, &check, r).await;
        let _ = fs::remove_dir_all(&scratch);
        verdict
    }

    async fn probe(&self, r: GateRun<'_>) -> Option<ProbeOut> {
        let pass = r
            .pass_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("pass");
        let scratch = self.scratch_root.join(format!("probe-{pass}-{}", r.n));
        let _ = fs::remove_dir_all(&scratch);
        let name = format!("probe-{}", r.n);
        let ran = self.run_agent(&scratch, &name, None, &r).await;
        let _ = fs::remove_dir_all(&scratch);
        let (transcript, incomplete) = match ran {
            Ok(t) => t,
            Err(why) => {
                return Some(ProbeOut {
                    finished: false,
                    incomplete: Some(why),
                    tail: String::new(),
                })
            }
        };
        let finished = incomplete.is_none();
        Some(ProbeOut {
            finished,
            incomplete,
            tail: crate::episode::render_tail(&transcript),
        })
    }
}

impl WorkspaceGate {
    /// Copy the workspace, run the episode's goal with the candidate
    /// playbook: the shared half of the gate and the probe (M53). Returns
    /// the probe transcript's path (kept in the pass dir) and the agent's
    /// incomplete note, if it stopped early.
    async fn run_agent(
        &self,
        scratch: &Path,
        name: &str,
        check: Option<&str>,
        r: &GateRun<'_>,
    ) -> Result<(PathBuf, Option<String>), String> {
        let workspace = scratch.join("workspace");
        let state = scratch.join("state");
        copy_workspace(&self.workspace, &workspace, &self.skip)?;
        fs::create_dir_all(&state).map_err(|e| format!("could not make the scratch copy: {e}"))?;
        let transcript = Transcript::create(r.pass_dir, name)
            .map_err(|e| format!("could not start the transcript: {e}"))?;
        let transcript_path = transcript.path().to_path_buf();
        let mut agent = variant::build(variant::Build {
            variant: Variant::Engineered,
            provider: self.provider.clone(),
            profile: &self.profile,
            sandbox: &self.sandbox,
            memory_tools: None,
            workspace: &workspace,
            state: &state,
            max_iterations: self.max_iterations,
            check,
            transcript: Some(transcript),
            playbook: r.playbook_block,
            edit_tools: Default::default(),
            ablate: &[],
            check_sandbox: None,
        })
        .with_ledger(
            r.sink.clone(),
            crate::budget::CALL_KIND,
            Some(name.to_string()),
            self.model.clone(),
        )
        .with_budget(r.meter.clone() as Arc<dyn Budget>);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let run = tokio::time::timeout(self.run_timeout, agent.run(&r.episode.goal, tx)).await;
        let _ = drain.await;
        match run {
            Err(_) => {
                return Err(format!(
                    "the run took longer than {}s",
                    self.run_timeout.as_secs()
                ))
            }
            Ok(Err(e)) => return Err(format!("the run failed: {e}")),
            Ok(Ok(_)) => {}
        }
        Ok((transcript_path, agent.incomplete.clone()))
    }

    async fn run_in(&self, scratch: &Path, check: &str, r: GateRun<'_>) -> GateVerdict {
        let workspace = scratch.join("workspace");
        let (_, incomplete) = match self
            .run_agent(scratch, &format!("gate-{}", r.n), Some(check), &r)
            .await
        {
            Ok(t) => t,
            Err(why) => return GateVerdict::fail(why),
        };
        if let Some(why) = &incomplete {
            if r.meter.exceeded().is_some() {
                return GateVerdict::fail("budget reached during the gate");
            }
            return GateVerdict::fail(format!("the gate run stopped before finishing: {why}"));
        }

        let verifier = CommandVerifier::new(check, self.sandbox.clone(), self.check_timeout);
        let ctx = ToolContext {
            workspace: workspace.clone(),
            max_output_chars: 800,
        };
        for round in 1..=2 {
            if let Err(out) = verifier.verify(&ctx).await {
                let out = out.trim().replace('\n', " | ");
                return GateVerdict::fail(if round == 1 {
                    format!("`{check}` fails with the lesson: {out}")
                } else {
                    format!("`{check}` passed once, then failed (flaky): {out}")
                });
            }
        }
        GateVerdict {
            passed: true,
            reason: format!("the task finished with the lesson and `{check}` passed twice"),
        }
    }
}

/// Copies `from` into `to` for a gate run: `.git` without its objects,
/// no `target/` or `node_modules/`, nothing under `skip`, within
/// [`MAX_FILES`] and [`MAX_BYTES`].
pub fn copy_workspace(from: &Path, to: &Path, skip: &[PathBuf]) -> Result<(), String> {
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let skip: Vec<PathBuf> = skip.iter().map(|p| canon(p)).collect();
    let (mut files, mut bytes) = (0usize, 0u64);
    let mut stack = vec![(canon(from), to.to_path_buf())];
    let err = |e: std::io::Error| format!("could not make the scratch copy: {e}");
    while let Some((src, dst)) = stack.pop() {
        fs::create_dir_all(&dst).map_err(err)?;
        for e in fs::read_dir(&src).map_err(err)?.flatten() {
            let path = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if skip.iter().any(|s| path.starts_with(s)) {
                continue;
            }
            let ft = e.file_type().map_err(err)?;
            let target = dst.join(&*name);
            if ft.is_symlink() {
                #[cfg(unix)]
                if let Ok(link) = fs::read_link(&path) {
                    let _ = std::os::unix::fs::symlink(link, &target);
                }
                continue;
            }
            if ft.is_dir() {
                let in_git = src.file_name().is_some_and(|n| n == ".git");
                if SKIP_DIRS.contains(&&*name) || (in_git && name == "objects") {
                    continue;
                }
                stack.push((path, target));
                continue;
            }
            files += 1;
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            if files > MAX_FILES || bytes > MAX_BYTES {
                return Err("workspace too large to copy for the gate".into());
            }
            fs::copy(&path, &target).map_err(err)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_copy_skips_objects_builds_and_the_data_dir() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path().join("ws");
        for f in [
            "src/main.rs",
            ".git/HEAD",
            ".git/objects/ab/cdef",
            "target/debug/x",
            "web/node_modules/y",
            "data/learn/playbook.md",
        ] {
            let p = ws.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "x").unwrap();
        }
        let out = d.path().join("out");
        copy_workspace(&ws, &out, &[ws.join("data")]).unwrap();
        assert!(out.join("src/main.rs").exists());
        assert!(out.join(".git/HEAD").exists());
        assert!(!out.join(".git/objects").exists());
        assert!(!out.join("target").exists());
        assert!(!out.join("web/node_modules").exists());
        assert!(!out.join("data").exists());
    }
}
