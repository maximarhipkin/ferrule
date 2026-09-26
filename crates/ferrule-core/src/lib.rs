//! ferrule-core: provider-agnostic agent runtime.
//!
//! The harness, not the model, is the performance lever. This crate owns the
//! loop; providers and harness profiles are swappable traits/config so each
//! model is driven the way it was trained to be driven.

pub mod agent;
pub mod baseline;
pub mod error;
pub mod event;
pub mod guard;
pub mod history;
pub mod hooks;
pub mod ledger;
pub mod lifecycle;
pub mod message;
pub mod profile;
pub mod provider;
pub mod routing;
pub mod stuck;
pub mod tool;
pub mod transcript;
pub mod verify;

pub use agent::{Agent, AgentConfig, ContextOverflow, RetryPolicy};
pub use baseline::load_context_baseline;
pub use error::{CoreError, FailureClass};
pub use event::AgentEvent;
pub use guard::{Guard, GuardedCall, Verdict};
pub use history::{result_ref, SearchHistoryTool, SEARCH_HISTORY};
pub use hooks::{Budget, Inbox, SessionRecall, StopFlag};
pub use ledger::{EvalTag, LedgerContext, LedgerRecord, LedgerSink, SpeedStats, ToolBatch};
pub use lifecycle::{HookEvent, HookSet};
pub use message::{Message, NativeBlocks, Role, ToolCall, Usage};
pub use profile::HarnessProfile;
pub use provider::{CompletionRequest, CompletionResponse, FailOver, Provider, Served};
pub use routing::{Escalation, Ladder, Policy, RouteTag, Signal, Tier, Tiered};
pub use tool::{Tool, ToolContext, ToolOutput, ToolRegistry, ToolSource};
pub use transcript::Transcript;
pub use verify::Verifier;
