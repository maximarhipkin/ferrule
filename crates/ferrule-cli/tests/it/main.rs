//! ferrule-cli's integration tests, linked into one binary so a change
//! relinks one executable, not one per file (docs/m40-build-diet.md).
//! A new test file is a module here, not a file in `tests/`.

mod agents;
mod backup;
mod channels;
mod claude_plan;
mod dashboard;
mod eval;
mod goal;
mod health;
mod hooks;
mod install_sh;
mod instances;
mod learn;
mod live_fixes;
mod local_models;
mod managed;
mod mcp_add;
mod memory;
mod models;
mod plugins;
mod ssh;
mod trust;
