//! `ferrule-mcp`: an MCP (Model Context Protocol) client. Spawns a server
//! process and speaks newline-delimited JSON-RPC 2.0 over its stdio, or
//! POSTs to a server's URL (Streamable HTTP), and exposes each of its tools as an ordinary `ferrule_core::tool::Tool` named
//! `mcp__<server>__<tool>` — the same convention Claude Code uses.
//!
//! A separate crate (rather than a module in `ferrule-tools`) because MCP
//! pulls in process-spawning and JSON-RPC framing machinery that has nothing
//! to do with the built-in fs/shell/web toolbelt, and because `ferrule-cli`
//! needs `McpServerConfig` for `ferrule.toml` regardless of which other
//! tools are compiled in.

pub mod browser;
pub mod client;
pub mod config;
pub mod error;
mod http;
pub mod tool;

pub use browser::BrowserConfig;
pub use client::{CallToolResult, McpClient, McpToolInfo, ServerHost};
pub use config::McpServerConfig;
pub use error::McpError;
pub use tool::{connect_and_build_tools, McpRemoteTool};
