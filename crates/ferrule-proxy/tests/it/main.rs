//! ferrule-proxy's integration tests, linked into one binary so a change
//! relinks one executable, not one per file (docs/m40-build-diet.md).
//! A new test file is a module here, not a file in `tests/`.

mod common;
mod egress;
mod embed;
mod plugin;
mod policy;
mod proxy;
mod search;
