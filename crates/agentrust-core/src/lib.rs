//! agentrust-core: provider-agnostic agent runtime.
//!
//! The harness, not the model, is the performance lever. This crate owns the
//! loop; providers and harness profiles are swappable traits/config so each
//! model is driven the way it was trained to be driven.

pub mod agent;
pub mod error;
pub mod event;
pub mod message;
pub mod profile;
pub mod provider;
pub mod tool;
pub mod transcript;

pub use agent::{Agent, AgentConfig};
pub use error::CoreError;
pub use event::AgentEvent;
pub use message::{Message, Role, ToolCall, Usage};
pub use profile::HarnessProfile;
pub use provider::{CompletionRequest, CompletionResponse, Provider};
pub use tool::{Tool, ToolContext, ToolOutput, ToolRegistry};
pub use transcript::Transcript;
