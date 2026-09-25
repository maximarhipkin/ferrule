//! M20 connections: the agent asks for a service, the owner approves with
//! one button, and the token is sealed on disk and added to MCP requests
//! outside the model's view. See `docs/m20-connections.md`.

pub mod catalog;
pub mod keyform;
pub mod oauth;
pub mod paste;
pub mod relay;
pub mod seal;
pub mod store;

pub use catalog::{AuthKind, Catalog, ClientKind, ReadOnly, Service};
pub use store::{Record, State, Store};
