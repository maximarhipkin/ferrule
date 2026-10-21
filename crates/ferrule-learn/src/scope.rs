//! M53.4: where a learned lesson or skill applies
//! (`docs/m53-contrastive-probing.md`, "M53.4"). A scope is a short key:
//! the workspace a terminal run worked in, or, in the gateway, the chat or
//! the scheduled task. What was learned in one scope is shown only there
//! until it is kept in a second scope too; then it is global.

use std::path::Path;

/// A terminal run's: `workspace:<path>`.
pub fn workspace(path: &Path) -> String {
    format!("workspace:{}", clean(&path.display().to_string()))
}

/// A gateway chat's, by its session id (`<channel>__<chat>`).
pub fn chat(session: &str) -> String {
    format!("chat:{}", clean(session))
}

/// A scheduled task's, by its id.
pub fn task(id: &str) -> String {
    format!("task:{}", clean(id))
}

/// A scope key sits inside a playbook line's `[pb-N @scope]`: no `]`, no
/// line break.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c == ']' || c.is_control() { '_' } else { c })
        .collect()
}
