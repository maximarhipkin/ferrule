//! M18: the hooks around a sub-agent's run. Its PreToolUse and PostToolUse
//! come from the root's set; SubagentStart and SubagentStop fire in the
//! parent's session, from the child's background task (docs/m18-hooks.md §7).

use ferrule_core::lifecycle::HookInput;
use ferrule_core::{Agent, CoreError, HookEvent, HookSet, ToolContext};
use std::path::PathBuf;
use tokio::sync::mpsc;

/// Who the child is, for SubagentStart/Stop's payload.
pub(crate) struct ChildRun {
    pub id: String,
    pub role: String,
    pub task: String,
    /// The parent's session id: the hooks fire in the parent.
    pub parent_session: String,
    /// The parent's workspace: the hooks' working directory.
    pub cwd: PathBuf,
}

impl ChildRun {
    fn input(&self, hooks: &HookSet) -> HookInput {
        let mut input = hooks.input(&self.parent_session, None, self.cwd.clone());
        input.agent_id = Some(self.id.clone());
        input.agent_type = Some(self.role.clone());
        input.task = Some(self.task.clone());
        input
    }
}

/// Runs `agent` on `message` between the parent's SubagentStart and
/// SubagentStop hooks. A blocking SubagentStart fails the child with the
/// reason; a blocking SubagentStop sends it on with the reason as its next
/// message, at most `max_stop_blocks` times.
pub(crate) async fn run_child(
    agent: &mut Agent,
    hooks: &HookSet,
    child: &ChildRun,
    message: String,
) -> Result<String, CoreError> {
    if hooks.is_empty() {
        return agent.run(&message, closed()).await;
    }
    let ctx = ToolContext {
        workspace: child.cwd.clone(),
        max_output_chars: 30_000,
    };
    let started = hooks
        .fire(HookEvent::SubagentStart, child.input(hooks), &ctx)
        .await;
    if let Some((_, reason)) = started.block {
        return Err(CoreError::Aborted(format!(
            "a SubagentStart hook blocked it: {reason}"
        )));
    }
    let mut message = match started.context {
        Some(note) => format!("{message}\n\n[hook: SubagentStart]\n{note}"),
        None => message,
    };
    let mut blocks = 0;
    loop {
        let result = agent.run(&message, closed()).await;
        let mut input = child.input(hooks);
        input.stop_hook_active = Some(blocks > 0);
        input.last_assistant_message = result.as_ref().ok().cloned();
        let stopped = hooks.fire(HookEvent::SubagentStop, input, &ctx).await;
        // A failed run ends as failed; the hook only saw it.
        let (Ok(answer), Some((hook, reason))) = (&result, stopped.block) else {
            return result;
        };
        blocks += 1;
        let rounds = hooks.limits.max_stop_blocks;
        if blocks > rounds {
            return Ok(format!(
                "{answer}\n\n[ferrule] Stopped here: the SubagentStop hook `{hook}` still blocks after {rounds} tr{}.",
                if rounds == 1 { "y" } else { "ies" }
            ));
        }
        message = format!("[hook: SubagentStop] This isn't done yet:\n\n{reason}");
    }
}

/// Nobody watches a child's events; a closed channel makes every send
/// return at once.
fn closed() -> mpsc::Sender<ferrule_core::AgentEvent> {
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    tx
}
