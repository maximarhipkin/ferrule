//! Multi-agent for Ferrule: an agent starts other agents in the background,
//! waits for their reports, resumes and closes them. The supervisor owns
//! their state (`agents.db`), their limits and the notices that reach a
//! parent; `docs/m12-multi-agent.md` is the design.

pub mod board;
pub mod error;
pub mod fence;
mod lifecycle;
pub mod prompts;
mod shared;
pub mod store;
pub mod supervisor;
pub mod tools;
mod worktree;

pub use board::{Entry, Task, TaskStatus};
pub use error::AgentsError;
pub use store::{AgentRow, AgentStore, Status};
pub use supervisor::{
    ChildFactory, ChildSpec, Closed, Limits, ModelCheck, Role, SpawnRequest, Spawned, Supervisor,
    Waker,
};
