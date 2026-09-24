//! ferrule-learn: the M16 learning loop. An offline pass reviews runs that
//! failed or needed retries, proposes one-line playbook deltas that are kept
//! only when a re-run of the task passes its check, merges near-duplicate
//! memories through M15's UPDATE, and writes every change to a readable,
//! revertable file — within caps read from the ledger. Design:
//! `docs/m16-learning-loop.md`.

pub mod budget;
pub mod consolidate;
pub mod diff;
pub mod episode;
pub mod files;
pub mod gate;
pub mod pass;
pub mod playbook;
pub mod reflect;
pub mod screen;

pub use budget::{Caps, LearnSink, Meter, PriceFn, Spent, CALL_KIND};
pub use episode::Episode;
pub use files::{LearnDir, PassRecord, State};
pub use gate::{Gate, GateRun, GateVerdict, WorkspaceGate};
pub use pass::{plan, revert, run_pass, Env, Options, Plan};
pub use playbook::{prompt_block, Applied, Delta, Lesson, Playbook, PromptBlock};
