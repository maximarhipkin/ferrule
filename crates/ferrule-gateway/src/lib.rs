//! ferrule-gateway: the long-running daemon process.
//!
//! This crate owns every external I/O boundary — channel adapters, session
//! routing, and (later) the task scheduler — so that `ferrule-core` and
//! `ferrule-cli` stay usable as a library and a plain one-shot CLI without
//! ever needing a daemon running. Mirrors OpenClaw's "Gateway as single
//! source of truth" pattern while keeping NanoClaw's minimalism: a queue and
//! a router, not a framework.

pub mod channel;
pub mod channels;
pub mod error;
pub mod gateway;
pub mod message;
pub mod router;
pub mod scheduler;
pub mod session;

pub use channel::{Channel, ChannelCapabilities};
pub use channels::{LocalChannel, TelegramChannel};
pub use error::GatewayError;
pub use gateway::Gateway;
pub use message::{Attachment, InboundMessage, OutboundMessage};
pub use router::{AgentFactory, Router};
pub use scheduler::{initial_next_run_at, NewTask, Run, RunOutcome, RunStatus, Scheduler, SchedulerError, Task, TaskKind, TaskStore, SCHEDULER_PSEUDO_CHANNEL};
