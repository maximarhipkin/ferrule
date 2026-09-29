//! ferrule-core's integration tests, linked into one binary so a change
//! relinks one executable, not one per file (docs/m40-build-diet.md).
//! A new test file is a module here, not a file in `tests/`.

mod history;
mod parallel;
mod prefix;
mod routing;
mod routing_off;
mod stream;
mod triggers;
