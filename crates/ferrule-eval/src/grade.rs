//! Graders: a command whose exit code is the verdict, and (see
//! `rubric.rs`) a model judging against a rubric.

use crate::fixture::run_command;
use crate::suite::Task;
use ferrule_sandbox::Sandbox;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// Default wall-clock limit for one grader command.
pub const GRADE_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraderResult {
    /// `"command"` | `"rubric"`.
    pub kind: String,
    pub passed: bool,
    /// The grader couldn't reach a verdict (bad judge output, …): neither a
    /// pass nor the agent's failure.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
    /// The tail of the command's output on failure, or the judge's notes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// Runs `grade.command` in the workspace under the run's sandbox.
pub async fn command(
    task: &Task,
    workspace: &Path,
    sandbox: &Arc<Sandbox>,
) -> Option<GraderResult> {
    let cmd = task.grade.command.as_deref()?;
    let timeout = task.grade.timeout_secs.unwrap_or(GRADE_TIMEOUT_SECS);
    let (passed, detail) = match run_command(cmd, workspace, sandbox, timeout).await {
        Ok(()) => (true, String::new()),
        Err(out) => (false, out),
    };
    Some(GraderResult {
        kind: "command".into(),
        passed,
        error: false,
        detail,
    })
}
