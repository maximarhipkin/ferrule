//! ferrule-trust (M19): what the owner trusts an agent with. Hard caps on
//! tokens and dollars per run, per day and per scheduled task, read from
//! the ledger; a warning at 80%; the kill switch; approval gates on
//! destructive shell commands, answered on Telegram or at the terminal;
//! plan mode; and an audit log of all of it. The agent loop sees only
//! `ferrule_core::Guard`; `TrustGuard` is this crate's.

pub mod approval;
pub mod audit;
pub mod classify;
pub mod clock;
pub mod config;
pub mod guard;
pub mod hub;
pub mod kill;
pub mod meter;
pub mod plan;
pub mod sink;

pub use classify::{classify, classify_command, Gated, Kind};
pub use clock::{Clock, FakeClock, SystemClock};
pub use config::TrustConfig;
pub use guard::{Route, TrustGuard};
pub use hub::{Hub, Intercept, Notifier, Prompter};
pub use kill::{KillSwitch, StopInfo};
pub use meter::{Meter, Spend};
pub use plan::{Plan, PlanStatus, PlanStore};
pub use sink::{Pricer, TrustSink};
