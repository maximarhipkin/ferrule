//! `ferrule eval`: runs task suites against the agent loop, grades the
//! results, writes them to the ledger, and compares ferrule's engineered
//! harness with a naive one on the same model. Design:
//! `docs/m14-eval.md`; how to run it: `docs/eval.md`.

pub mod fixture;
pub mod grade;
pub mod history;
pub mod plan;
pub mod report;
pub mod rubric;
pub mod runner;
pub mod sink;
pub mod suite;
pub mod variant;

pub use rubric::Judge;
pub use runner::{
    run_suite, Arm, Env, Options, Outcome, OwnerTrust, Routing, RoutingPair, SuiteRun, TaskResult,
    RESULT_KIND,
};
pub use sink::{Caps, Pricing, Totals};
pub use suite::{Suite, SuiteKind, Task};
pub use variant::{EditTools, MemoryTools, Variant};
