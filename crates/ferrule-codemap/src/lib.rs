//! M29: a repo map and `code_search`, from tree-sitter tags.
//!
//! - [`repo`]: the walk, the code-repo check, the per-file tag cache.
//! - [`tags`]: definitions and references out of one parse.
//! - [`rank`]: Aider-style PageRank and the budgeted map.
//! - [`search`]: the `code_search` tool.
//!
//! The map reaches the model through core's [`TurnContext`] seam as a user
//! message, never through the system prompt (M27's cached prefix); see
//! docs/m29-edit-mechanics.md §2.

pub mod lang;
pub mod rank;
pub mod repo;
pub mod search;
pub mod tags;

pub use rank::{repo_map, Mentions};
pub use repo::{looks_like_code_repo, CodeMap, Snapshot};
pub use search::CodeSearchTool;

use ferrule_core::{Message, Role, TurnContext};
use std::sync::Arc;

/// Default map budget, in tokens (chars / 4).
pub const DEFAULT_MAP_TOKENS: usize = 1024;
/// "The conversation" for mentions: the request and this many of the
/// latest messages.
pub const MENTION_WINDOW: usize = 20;

/// Which grammars this build has, for `ferrule doctor`.
pub fn languages() -> Vec<&'static str> {
    lang::all().iter().map(|l| l.name).collect()
}

/// The repo map as a [`TurnContext`]: refreshed (cheaply) at the start of
/// every run and ranked towards what the conversation mentions.
pub struct RepoMapContext {
    map: Arc<CodeMap>,
    budget: usize,
}

impl RepoMapContext {
    pub fn new(map: Arc<CodeMap>, budget: usize) -> Self {
        Self { map, budget }
    }
}

/// The text mentions are taken from: the goal and the latest user and
/// assistant messages (tool calls' arguments included). Tool results are
/// left out: one `list_dir` would "mention" every file. So are earlier
/// maps, or the map would feed on itself.
pub fn conversation_text(goal: &str, history: &[Message]) -> String {
    let mut text = goal.to_string();
    let start = history.len().saturating_sub(MENTION_WINDOW);
    for m in &history[start..] {
        if !matches!(m.role, Role::User | Role::Assistant) {
            continue;
        }
        if let Some(c) = m.content.as_ref().filter(|c| !c.starts_with(rank::HEADER)) {
            text.push('\n');
            text.push_str(c);
        }
        for call in &m.tool_calls {
            text.push('\n');
            text.push_str(&call.arguments.to_string());
        }
    }
    text
}

#[async_trait::async_trait]
impl TurnContext for RepoMapContext {
    async fn context(&self, goal: &str, history: &[Message]) -> Option<String> {
        if self.budget == 0 {
            return None;
        }
        let text = conversation_text(goal, history);
        let map = self.map.clone();
        let budget = self.budget;
        tokio::task::spawn_blocking(move || {
            let snapshot = map.refresh();
            repo_map(&snapshot, &Mentions::new(&text), budget)
        })
        .await
        .ok()
        .flatten()
    }
}
