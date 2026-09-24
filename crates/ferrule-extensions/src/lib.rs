//! Self-extension: the agent installs MCP servers and skills mid-session,
//! under an owner-controlled allow-list, exact pins, a description scan and
//! an approval queue. See `docs/m13-self-extension.md`.

pub mod allowlist;
pub mod scan;

pub use allowlist::{AllowEntry, AllowList, Kind, Source};
pub use scan::{scan_skill, scan_tool, tool_digest, Finding, Level};
