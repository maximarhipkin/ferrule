//! ferrule-mcp's integration tests, linked into one binary so a change
//! relinks one executable, not one per file (docs/m40-build-diet.md).
//! A new test file is a module here, not a file in `tests/`.

mod browser;
mod mcp_client;
