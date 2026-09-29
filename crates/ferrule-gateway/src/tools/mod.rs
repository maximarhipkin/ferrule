//! Tools the gateway gives a chat's agent (M39: `send_file`).

pub mod send_file;

pub use send_file::{FileOut, SendFileTool};
