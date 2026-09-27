//! The Claude plan through Claude Code (design §7): every model request is
//! made by the unmodified `claude` binary, signed in through Anthropic's
//! own flow. Ferrule never calls the Anthropic API with a subscription
//! token.
//!
//! - `token`: the credential, as far as ferrule holds one (a pasted
//!   setup-token, sealed; an exported one, moved out of the environment).
//! - `env`: the child's argv and environment, and the config-dir checks.
//! - `stream`: the stream-json events a turn sends.
//! - `bridge`: the loopback MCP server for ferrule's tools and the
//!   permission prompt, and the relay behind `ferrule claude-mcp`.
//! - `engine`: the `ClaudeCode` provider.

pub mod bridge;
pub mod cli;
pub mod engine;
pub mod env;
pub mod stream;
pub mod token;

pub use engine::{ClaudeCode, EngineConfig};
