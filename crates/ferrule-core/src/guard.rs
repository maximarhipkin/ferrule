//! The owner's guard over a run (M19): it can stop the run before a model
//! call, refuse a tool call (or ask the owner first), and halt the run in
//! the middle of a call. What it guards against — spending caps, the kill
//! switch, the approval gates, plan mode — lives in `ferrule-trust`; the
//! loop only knows these four questions.

use serde_json::Value;
use std::future::Future;
use std::sync::Arc;

/// What the guard says about one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Not run; the reason is the tool's result, so the model sees why.
    Refuse(String),
}

/// One tool call, as the guard sees it.
#[derive(Debug, Clone, Copy)]
pub struct GuardedCall<'a> {
    pub tool: &'a str,
    pub args: &'a Value,
    /// The tool says it can change files (`Tool::changes_files`).
    pub changes_files: bool,
    /// The tool asks for the owner's approval on every call
    /// (`Tool::needs_approval`, e.g. a plugin tool marked `approval`).
    pub needs_approval: bool,
}

#[async_trait::async_trait]
pub trait Guard: Send + Sync {
    /// A run of the agent starts.
    fn begin(&self) {}
    /// Asked before every model call: `Some(message)` ends the run with
    /// that message as its answer, without the call.
    fn before_model_call(&self) -> Option<String>;
    /// Asked before every tool call. May wait (for the owner's approval).
    async fn before_tool_call(&self, call: GuardedCall<'_>) -> Verdict;
    /// Resolves, with the message to end the run with, once the run must
    /// stop now, even in the middle of a call. Never resolves otherwise.
    async fn halted(&self) -> String;
}

/// Runs `fut` unless the guard halts first; the halt wins a tie, and `fut`
/// is dropped (a shell command's process group goes with it).
pub async fn unless_halted<T>(
    guard: Option<&Arc<dyn Guard>>,
    fut: impl Future<Output = T>,
) -> Result<T, String> {
    let Some(guard) = guard else {
        return Ok(fut.await);
    };
    tokio::select! {
        biased;
        why = guard.halted() => Err(why),
        out = fut => Ok(out),
    }
}
