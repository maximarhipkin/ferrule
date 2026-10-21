//! ferrule-learn: the M16 learning loop. An offline pass reviews runs that
//! failed or needed retries, proposes one-line playbook deltas or (M53.3)
//! skills that are kept only when a re-run of the task passes its check or
//! (M53) wins the judge's contrast, merges near-duplicate
//! memories through M15's UPDATE, and writes every change to a readable,
//! revertable file — within caps read from the ledger. Design:
//! `docs/m16-learning-loop.md`.

pub mod budget;
pub mod consolidate;
pub mod diff;
pub mod episode;
pub mod files;
pub mod gate;
pub mod judge;
pub mod pass;
pub mod playbook;
pub mod reflect;
pub mod scope;
pub mod screen;
pub mod skills;

pub use budget::{Caps, LearnSink, Meter, PriceFn, Spent, CALL_KIND};
pub use episode::Episode;
pub use files::{LearnDir, PassRecord, State};
pub use gate::{Gate, GateRun, GateVerdict, ProbeOut, WorkspaceGate};
pub use pass::{plan, revert, run_pass, Env, Options, Plan};
pub use playbook::{prompt_block, prompt_block_for, Applied, Delta, Lesson, Playbook, PromptBlock};
pub use skills::SkillDraft;
