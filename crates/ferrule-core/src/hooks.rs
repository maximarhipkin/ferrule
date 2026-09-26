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

/// Long-term memory for the start of a session. On an agent's first run,
/// before the goal is added, the loop asks for a block about the session's
/// goal and adds it once, as a user message right after the goal (M27: the
/// system prompt stays the same bytes for every session, so it caches). `None` (or an empty block) adds nothing; a store
/// that can't be read should answer `None`, not fail the run.
#[async_trait::async_trait]
pub trait SessionRecall: Send + Sync {
    async fn recall(&self, goal: &str) -> Option<String>;
}

/// Context that is asked for at the start of every run (M29: the repo map).
/// The answer goes in as a user message after the goal and any recalled
/// memory, never into the system prompt (M27: the cached prefix). The
/// agent keeps the last block it added: an equal answer adds nothing while
/// that block is still in the history, so a turn in which nothing changed
/// adds no bytes. A changed block is appended; history is never edited.
/// `None` (or an empty block) adds nothing, and a source that fails should
/// answer `None`, not fail the run.
#[async_trait::async_trait]
pub trait TurnContext: Send + Sync {
    /// `history` is the conversation so far, this run's goal included.
    async fn context(&self, goal: &str, history: &[crate::message::Message]) -> Option<String>;
}

/// How a run ended, for a [`RunObserver`].
pub struct RunEnd<'a> {
    pub session_id: &'a str,
    pub goal: &'a str,
    /// The answer; `None` when `Agent::run` returned an error.
    pub answer: Option<&'a str>,
    /// Why the run stopped short, when it did.
    pub incomplete: Option<&'a str>,
}

/// Something done around every root run (M29: auto-commit). `begin` is
/// awaited before the run starts and `end` after it ends, however it ends:
/// done, step limit, stop or error. A note `end` returns goes out as
/// [`crate::AgentEvent::Notice`]; an observer that fails says so there
/// and never fails the run.
#[async_trait::async_trait]
pub trait RunObserver: Send + Sync {
    /// A short name for the notice (`auto-commit`).
    fn name(&self) -> &str;
    async fn begin(&self, session_id: &str);
    async fn end(&self, run: &RunEnd<'_>) -> Option<String>;
}
