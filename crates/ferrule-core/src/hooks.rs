//! Hooks a supervisor can attach to an [`crate::Agent`] without the loop
//! knowing why: a spending limit, an inbox of messages that arrive while it
//! runs, and a flag that stops it. Multi-agent (`ferrule-agents`) is the
//! user; the loop itself stays agnostic.

use crate::message::Usage;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A shared spending limit. Every provider call the agent makes is charged;
/// before each model turn the loop asks whether the budget is spent, and if
/// so stops with a status answer, as at the step limit.
pub trait Budget: Send + Sync {
    fn charge(&self, usage: &Usage);
    /// `Some(why)` once nothing more may be spent.
    fn exhausted(&self) -> Option<String>;
}

/// Messages that reach an agent while it works. Drained before every model
/// call and added as one user message; [`Inbox::begin`] and [`Inbox::end`]
/// bracket each run so the owner of the inbox knows whether the agent is
/// running or idle (and can wake it for anything left at `end`).
pub trait Inbox: Send + Sync {
    fn begin(&self) {}
    fn take(&self) -> Vec<String>;
    fn end(&self) {}
}

/// A cooperative stop: once set, the agent stops before its next model call
/// or tool call and the run returns [`crate::CoreError::Aborted`].
#[derive(Clone, Default, Debug)]
pub struct StopFlag(Arc<AtomicBool>);

impl StopFlag {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    /// Clears it, so a stopped agent can be run again.
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
