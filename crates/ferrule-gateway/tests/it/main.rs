//! ferrule-gateway's integration tests, linked into one binary so a change
//! relinks one executable, not one per file (docs/m40-build-diet.md).
//! A new test file is a module here, not a file in `tests/`.

mod daily_use;
mod discord;
mod email;
mod http;
mod matrix;
mod mattermost;
mod send_file;
mod signal;
mod slack;
mod streaming;
mod support;
mod voice;
mod whatsapp;
