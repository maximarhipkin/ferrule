//! ferrule-eval's integration tests, linked into one binary so a change
//! relinks one executable, not one per file (docs/m40-build-diet.md).
//! A new test file is a module here, not a file in `tests/`.

mod edit_tools;
mod starter_suite;
mod suite_run;
