//! Multi-agent for Ferrule: an agent starts other agents in the background,
//! waits for their reports, resumes and closes them. The supervisor owns
//! their state (`agents.db`), their limits and the notices that reach a
//! parent; `docs/m12-multi-agent.md` is the design.

pub mod error;
pub mod fence;
pub mod prompts;
pub mod store;
pub mod supervisor;
pub mod tools;

pub use error::AgentsError;
pub use store::{AgentRow, AgentStore, Status};
pub use supervisor::{
    ChildFactory, ChildSpec, Limits, Role, SpawnRequest, Spawned, Supervisor, Waker,
};
