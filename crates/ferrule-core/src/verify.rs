//! The verify step, enforced by the runtime rather than asked for in the
//! prompt: before a run that changed files may finish, the check runs, and
//! a failure goes back to the model to fix. Claude Code's Stop hook does
//! the same (exit code 2 blocks the stop), and bounds it the same way.

use crate::tool::ToolContext;

#[async_trait::async_trait]
pub trait Verifier: Send + Sync {
    /// How the check is shown to the model and the owner, e.g. the command.
    fn describe(&self) -> String;
    /// `Ok` when the check passes. `Err` carries what the model needs to fix
    /// it, the tail of the output.
    async fn verify(&self, ctx: &ToolContext) -> Result<(), String>;
}
