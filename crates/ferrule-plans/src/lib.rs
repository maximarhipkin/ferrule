//! M35: subscription plans instead of API keys.
//!
//! - `chatgpt`: signing in to a ChatGPT plan (device code, PKCE with a
//!   loopback callback, a pasted redirect), the sealed tokens in
//!   `private/plans/`, refresh under a cross-process lock, and the
//!   `PlanAuth` the Codex driver asks for a token.
//! - `usage`: the plans' usage windows, in a file that status, doctor and
//!   the dashboard read.
//!
//! The design is `docs/m35-subscriptions.md`.

pub mod chatgpt;
mod jwt;
mod lock;
pub mod usage;

pub use chatgpt::ChatGpt;
pub use usage::{Reading, UsageFile, Window};

/// Seconds since the epoch.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
