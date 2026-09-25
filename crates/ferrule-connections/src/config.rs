//! `[connections]` in `ferrule.toml`.

use crate::catalog::Service;
use serde::{Deserialize, Serialize};

pub const DEFAULT_LOOPBACK: &str = "http://127.0.0.1:8976/callback";

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConnectionsConfig {
    /// The owner's relay (`ferrule connections relay deploy` prints it).
    /// None: no relay, so a quick tunnel or paste-back.
    pub relay_url: Option<String>,
    /// `cloudflared` for the quick-tunnel fallback: a path, or "off".
    /// Unset: looked up on PATH.
    pub cloudflared: Option<String>,
    /// The redirect for paste-back and terminal flows. Google's client
    /// must list it.
    pub loopback_redirect: String,
    /// A connected service's tools that can change something ask the owner
    /// first (M19's gate), like a `rm -rf`.
    pub gate_writes: bool,
    /// Services beyond the catalog, or a built-in one changed.
    pub custom: Vec<Service>,
}

impl Default for ConnectionsConfig {
    fn default() -> Self {
        Self {
            relay_url: None,
            cloudflared: None,
            loopback_redirect: DEFAULT_LOOPBACK.into(),
            gate_writes: true,
            custom: Vec::new(),
        }
    }
}
