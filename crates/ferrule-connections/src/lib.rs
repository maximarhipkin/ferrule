//! M20 connections: the agent asks for a service, the owner approves with
//! one button, and the token is sealed on disk and added to MCP requests
//! outside the model's view. See `docs/m20-connections.md`.

pub mod catalog;
pub mod config;
pub mod credential;
pub mod keyform;
pub mod oauth;
pub mod paste;
pub mod relay;
pub mod seal;
pub mod service;
pub mod store;
pub mod tools;
pub mod tunnel;

pub use catalog::{AuthKind, Catalog, ClientKind, ReadOnly, Service};
pub use config::ConnectionsConfig;
pub use service::{Action, Actor, Button, Chat, Connections, Events, Reply, Snapshot, Status};
pub use store::{Record, State, Store};
