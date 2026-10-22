//! `ferrule-mcp`: an MCP (Model Context Protocol) client. Spawns a server
//! process and speaks newline-delimited JSON-RPC 2.0 over its stdio, or
//! POSTs to a server's URL (Streamable HTTP), and exposes each of its tools as an ordinary `ferrule_core::tool::Tool` named
//! `mcp__<server>__<tool>` — the same convention Claude Code uses. It
//! speaks MCP 2025-06-18 (an `initialize` handshake) and, since wave C,
//! 2026-07-28 (no handshake, `_meta` on every request), finding out once
//! which a server speaks (`era`, `docs/m17-mcp-add.md`).
//!
//! A separate crate (rather than a module in `ferrule-tools`) because MCP
//! pulls in process-spawning and JSON-RPC framing machinery that has nothing
//! to do with the built-in fs/shell/web toolbelt, and because `ferrule-cli`
//! needs `McpServerConfig` for `ferrule.toml` regardless of which other
//! tools are compiled in.

pub mod auth;
pub mod browser;
pub mod client;
pub mod config;
mod era;
pub mod error;
mod http;
pub mod local;
pub mod tool;

pub use auth::{Auth, CredentialSource};
pub use browser::BrowserConfig;
pub use client::{CallToolResult, McpClient, McpToolInfo, SavedContent, ServerHost};
pub use config::McpServerConfig;
pub use error::McpError;
pub use local::{Local, LocalServer};
pub use tool::{build_tools, connect_and_build_tools, McpRemoteTool};
