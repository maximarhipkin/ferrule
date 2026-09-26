//! Self-extension: the agent installs MCP servers and skills mid-session,
//! under an owner-controlled allow-list, exact pins, a description scan and
//! an approval queue. See `docs/m13-self-extension.md`.

pub mod allowlist;
pub mod error;
pub mod git;
pub mod layout;
pub mod lock;
pub mod manager;
pub mod pending;
pub mod scan;
pub mod skill;
pub mod source;
pub mod tools;

pub use allowlist::{AllowEntry, AllowList, Kind, Source};
pub use error::{ExtError, Result};
pub use layout::Layout;
pub use lock::{LockFile, LockStore, Origin, PluginEntry, ServerEntry, SkillEntry, Status, Waiver};
pub use manager::{
    Approver, ExtensionManager, Listed, ManagerConfig, Outcome, Probe, QueueApprover, Review,
};
pub use pending::{Pending, PendingQueue, Request};
pub use scan::{scan_skill, scan_tool, tool_digest, Finding, Level};
pub use source::{McpRequest, PluginRequest, SkillRequest};
pub use tools::tools;
