//! Remote workspaces over SSH (M34, docs/m34-ssh-local.md): the shell and
//! file tools run on another machine through the system `ssh`, with
//! strict host keys, a reconnecting link, and commands that die with it.
//!
//! The remote account is the boundary. ferrule's local sandbox doesn't
//! reach the remote: a command there can do whatever that account can.

pub mod classify;
pub mod link;
pub mod script;
pub mod target;
pub mod tools;
pub mod trust;

pub use classify::Failure;
pub use link::{default_user_known_hosts, DenySpec, EnvFn, Forward, Link, LinkOptions, Remote};
pub use target::{is_remote, HostConfig, Target};
pub use tools::{
    RemoteEditFile, RemoteListDir, RemoteReadFile, RemoteShellTool, RemoteVerifier, RemoteWriteFile,
};

use ferrule_core::tool::{Tool, ToolRegistry};
use std::sync::Arc;

/// Put the remote tools in `registry`, over any local ones of the same
/// name.
pub fn register(registry: &mut ToolRegistry, link: &Arc<Link>) {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(RemoteShellTool::new(link.clone())),
        Arc::new(RemoteReadFile(link.clone())),
        Arc::new(RemoteWriteFile(link.clone())),
        Arc::new(RemoteEditFile(link.clone())),
        Arc::new(RemoteListDir(link.clone())),
    ];
    for t in tools {
        registry.register(t);
    }
}
