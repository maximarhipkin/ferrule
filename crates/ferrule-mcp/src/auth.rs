//! Credentials asked for per request (M20's connections): a remote
//! server's token lives in the connection store, is refreshed there, and
//! is never in the config, an environment or a header map built once.

use async_trait::async_trait;
use std::fmt;
use std::sync::Arc;

/// Hands the HTTP transport the header that authenticates it.
#[async_trait]
pub trait CredentialSource: Send + Sync {
    /// Stable for one connection: two configs with the same id are the
    /// same server (M17's follower compares configs to decide restarts).
    fn id(&self) -> String;
    /// `(header name, header value)` for the next request, refreshed first
    /// if it's about to expire. The error is a fixed phrase, never a token.
    async fn header(&self) -> Result<(String, String), String>;
    /// The server answered 401 to `rejected` (the value sent): a fresh
    /// one, or an error if there's none to be had.
    async fn refreshed(&self, rejected: &str) -> Result<(String, String), String>;
}

/// `McpServerConfig::auth`: a shared [`CredentialSource`].
#[derive(Clone)]
pub struct Auth(pub Arc<dyn CredentialSource>);

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Auth({})", self.0.id())
    }
}

impl PartialEq for Auth {
    fn eq(&self, other: &Self) -> bool {
        self.0.id() == other.0.id()
    }
}
