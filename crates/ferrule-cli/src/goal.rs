//! Goal loops (M42 part 5, `docs/m42-harness-engineering.md`): a goal, an
//! independent judge, a stopping condition — across sessions, not prompts.
//! `ferrule run --goal --verify CMD "…"` starts one; a run the budget cuts
//! short ends goal-pending, and `ferrule run --resume SESSION` picks it up
//! with the folded transcript and this file's memory of the judge.

use anyhow::{anyhow, bail, Result};
use ferrule_core::tool::ToolContext;
use ferrule_core::verify::Verifier;
use ferrule_core::Transcript;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The state of one goal loop, kept as `<session>.goal.json` beside the
/// session's transcript so a later process resumes where the last stopped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopState {
    pub goal: String,
    pub verify: Vec<String>,
    /// Judge runs so far, passes and failures.
    pub attempts: usize,
    /// The judge's latest failing word; `None` when it last passed.
    pub last_failure: Option<String>,
    pub created: String,
    pub updated: String,
}

impl LoopState {
    fn new(goal: &str, verify: Vec<String>) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            goal: goal.to_string(),
            verify,
            attempts: 0,
            last_failure: None,
            created: now.clone(),
            updated: now,
        }
    }

    /// The judge ran at least once and its latest word was a pass.
    pub fn judge_satisfied(&self) -> bool {
        self.attempts > 0 && self.last_failure.is_none()
    }
}

/// A goal loop attached to a session: its id and where its file lives.
pub struct GoalSession {
    pub session_id: String,
    pub sessions: PathBuf,
    pub state: LoopState,
}

pub fn path(sessions: &Path, session_id: &str) -> PathBuf {
    sessions.join(format!("{session_id}.goal.json"))
}

pub fn load(sessions: &Path, session_id: &str) -> Result<Option<LoopState>> {
    match std::fs::read_to_string(path(sessions, session_id)) {
        Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn save(sessions: &Path, session_id: &str, state: &LoopState) -> Result<()> {
    let mut stamped = state.clone();
    stamped.updated = chrono::Utc::now().to_rfc3339();
    std::fs::create_dir_all(sessions)?;
    let file = path(sessions, session_id);
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&stamped)?)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

/// The loop for the session a transcript belongs to, when it has one.
pub fn for_transcript(t: &Transcript) -> Option<GoalSession> {
    let sessions = t.path().parent()?.to_path_buf();
    let session_id = t.path().file_stem()?.to_string_lossy().into_owned();
    let state = load(&sessions, &session_id).ok()??;
    Some(GoalSession {
        session_id,
        sessions,
        state,
    })
}

/// A verifier that writes every verdict into the loop's state file: the
/// judge's word is the loop's memory across sessions.
struct RecordingVerifier {
    inner: Arc<dyn Verifier>,
    sessions: PathBuf,
    session_id: String,
}

/// Wrap `inner` so its verdicts land in the goal file, when this session
/// is a goal loop. Returns `inner` unchanged when it isn't.
pub fn record_verdicts(inner: Arc<dyn Verifier>, goal: &Option<GoalSession>) -> Arc<dyn Verifier> {
    match goal {
        Some(g) => Arc::new(RecordingVerifier {
            inner,
            sessions: g.sessions.clone(),
            session_id: g.session_id.clone(),
        }),
        None => inner,
    }
}

/// The most the state file keeps of a failure; the model already saw it.
const MAX_STORED_FAILURE: usize = 2_000;

#[async_trait::async_trait]
impl Verifier for RecordingVerifier {
    fn describe(&self) -> String {
        self.inner.describe()
    }

    async fn verify(&self, ctx: &ToolContext) -> Result<(), String> {
        let verdict = self.inner.verify(ctx).await;
        if let Ok(Some(mut state)) = load(&self.sessions, &self.session_id) {
            state.attempts += 1;
            state.last_failure = verdict.as_ref().err().map(|e| {
                let cut: String = e.chars().take(MAX_STORED_FAILURE).collect();
                cut
            });
            let _ = save(&self.sessions, &self.session_id, &state);
        }
        verdict
    }
}

/// The goal-loop block of the system prompt: the judge decides done-ness,
/// never the agent doing the work (L13's maker-checker rule).
pub fn prompt_block(state: &LoopState) -> String {
    let steps = state
        .verify
        .iter()
        .map(|c| format!("`{c}`"))
        .collect::<Vec<_>>()
        .join(", then ");
    format!(
        "[Goal loop] This run works one goal until an independent judge says it's met:\n{}\n\n\
         The judge is not you: ferrule runs {steps} when you finish, whether or not files \
         changed, and a failure comes back to you to fix. Never declare the goal met yourself — \
         finish and let the judge speak. If the budget runs out first, the run ends \
         goal-pending and a later `ferrule run --resume` continues it.",
        state.goal
    )
}

/// What a resumed loop continues with: where the judge left it, plus
/// anything the owner added on the command line.
fn resume_prompt(state: &LoopState, extra: Option<&str>) -> String {
    let verdict = match &state.last_failure {
        Some(f) => format!("the judge last said:\n{f}"),
        None if state.attempts == 0 => "the judge hasn't run yet".to_string(),
        None => "the judge last passed, but the run didn't finish".to_string(),
    };
    let mut p = format!(
        "[ferrule goal loop, resumed] Keep working the goal: {}\nLast time, {verdict}.",
        state.goal
    );
    if let Some(extra) = extra.map(str::trim).filter(|e| !e.is_empty()) {
        p.push_str(&format!("\n\nThe owner adds: {extra}"));
    }
    p
}

/// Everything `run_root` needs for a goal-loop run.
pub struct Prepared {
    pub session_id: String,
    pub resume: bool,
    pub prompt: String,
    pub verify: Vec<String>,
}

/// Validate and prepare a goal-loop run: fresh (`--goal`) or resumed
/// (`--resume`). A fresh loop writes its state file now, so the verifier
/// the agent gets has something to record into.
pub fn prepare(
    prompt: Option<String>,
    verify: Vec<String>,
    resume: Option<String>,
) -> Result<Prepared> {
    let sessions = crate::config::data_dir()?.join("sessions");
    match resume {
        Some(session_id) => {
            let Some(mut state) = load(&sessions, &session_id)? else {
                bail!(
                    "no goal loop state for session {session_id} — \
                     it wasn't started with `--goal`"
                );
            };
            if !verify.is_empty() {
                state.verify = verify;
            }
            save(&sessions, &session_id, &state)?;
            Ok(Prepared {
                prompt: resume_prompt(&state, prompt.as_deref()),
                verify: state.verify.clone(),
                session_id,
                resume: true,
            })
        }
        None => {
            let goal = prompt
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .ok_or_else(|| anyhow!("a goal loop needs the goal as the prompt"))?;
            let verify = if !verify.is_empty() {
                verify
            } else {
                crate::config::Config::load()?.0.agent.verify_command
            };
            if verify.is_empty() {
                bail!(
                    "a goal loop needs a judge: pass `--verify CMD` or set \
                     `[agent] verify_command`"
                );
            }
            let session_id = uuid::Uuid::new_v4().to_string();
            save(
                &sessions,
                &session_id,
                &LoopState::new(&goal, verify.clone()),
            )?;
            Ok(Prepared {
                session_id,
                resume: false,
                prompt: goal,
                verify,
            })
        }
    }
}

/// The loop's truthful status line, printed after the run's own ending.
pub fn report(session_id: &str) {
    let Ok(sessions) = crate::config::data_dir().map(|d| d.join("sessions")) else {
        return;
    };
    let Ok(Some(state)) = load(&sessions, session_id) else {
        return;
    };
    if state.judge_satisfied() {
        println!(
            "\x1b[1;32mgoal met\x1b[0m — the judge passed after {} run{} (session {session_id})",
            state.attempts,
            if state.attempts == 1 { "" } else { "s" },
        );
    } else {
        println!(
            "\x1b[1;33mgoal pending\x1b[0m — the judge hasn't passed yet ({} run{}). Resume with:\n  \
             ferrule run --resume {session_id}",
            state.attempts,
            if state.attempts == 1 { "" } else { "s" },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_file_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = LoopState::new("all tests pass", vec!["cargo test".into()]);
        save(dir.path(), "s1", &state).unwrap();
        let mut read = load(dir.path(), "s1").unwrap().unwrap();
        assert_eq!(read.goal, "all tests pass");
        assert!(!read.judge_satisfied(), "no attempts yet");
        read.attempts = 1;
        save(dir.path(), "s1", &read).unwrap();
        read = load(dir.path(), "s1").unwrap().unwrap();
        assert!(read.judge_satisfied(), "one pass satisfies the judge");
        state.last_failure = Some("1 failed".into());
        save(dir.path(), "s1", &state).unwrap();
        assert!(!load(dir.path(), "s1").unwrap().unwrap().judge_satisfied());
        assert!(load(dir.path(), "nope").unwrap().is_none());
    }

    #[test]
    fn the_resume_prompt_carries_the_judges_last_word() {
        let mut state = LoopState::new("the goal", vec!["make check".into()]);
        state.attempts = 2;
        state.last_failure = Some("test foo FAILED".into());
        let p = resume_prompt(&state, Some("hurry up"));
        assert!(p.contains("the goal"));
        assert!(p.contains("test foo FAILED"));
        assert!(p.contains("hurry up"));
        let p = resume_prompt(&state, None);
        assert!(!p.contains("The owner adds"));
    }
}
