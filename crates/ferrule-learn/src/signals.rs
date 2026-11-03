//! The owner's signals (`docs/m16-learning-loop.md`, "Signals"): what the
//! owner said about the agent's work, kept whether learning is on or not,
//! one line each in `learn/signals.jsonl`. A 👍 or 👎 on a dashboard chat
//! reply, `/undo`, the Undo of a learned change, and a Keep or Drop of a
//! proposed one. A 👎 keeps the chat's transcript cut where the reply
//! ended (as M53.5's Telegram 👎 does), so it becomes an episode for the
//! next pass; nothing else of the reply is kept. Approvals the owner
//! refused and the proactive agent's Not now / Never / Undo are already in
//! the trust audit log; the CLI reads them from there.

use crate::files::LearnDir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// A 👍 on a reply.
pub const UP: &str = "up";
/// A 👎 on a reply.
pub const DOWN: &str = "down";
/// `/undo` of a run's commit.
pub const UNDO: &str = "undo";
/// The Undo of a learned change (`/unlearn`, the page's Revert).
pub const UNLEARN: &str = "unlearn";
/// A proposed change the owner kept.
pub const KEEP: &str = "keep";
/// A proposed change the owner dropped.
pub const DROP: &str = "drop";
/// The longest "why" kept from the owner, in characters.
pub const WHY_MAX: usize = 500;

/// One line of `learn/signals.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    /// Unix seconds.
    pub at: i64,
    /// [`UP`], [`DOWN`], [`UNDO`], [`UNLEARN`], [`KEEP`] or [`DROP`].
    pub kind: String,
    /// Where it was given: `dashboard`, `chat`, `cli`.
    pub source: String,
    /// The chat's session (`<channel>__<chat>`), for a reply's thumb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The reply, by the channel's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The owner's own words on why, capped at [`WHY_MAX`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// A 👎's transcript cut, under `learn/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    /// What it is about when it isn't a reply: a pass's change
    /// (`<pass>:<n>`, `<pass>:p<k>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
}

impl Signal {
    /// A signal of `kind` from `source`, now, about nothing in particular.
    pub fn new(kind: &str, source: &str, at: i64) -> Self {
        Self {
            at,
            kind: kind.into(),
            source: source.into(),
            session: None,
            message: None,
            why: None,
            transcript: None,
            about: None,
        }
    }

    /// Whether it is a 👍 or 👎 on a reply.
    pub fn is_thumb(&self) -> bool {
        matches!(self.kind.as_str(), UP | DOWN) && self.session.is_some()
    }
}

/// `why` trimmed and capped, or `None` when empty.
pub fn clip_why(why: Option<&str>) -> Option<String> {
    let w = why?.trim();
    if w.is_empty() {
        return None;
    }
    Some(if w.chars().count() > WHY_MAX {
        format!("{}…", w.chars().take(WHY_MAX).collect::<String>())
    } else {
        w.to_string()
    })
}

impl LearnDir {
    pub fn signals_path(&self) -> PathBuf {
        self.root().join("signals.jsonl")
    }

    /// Every signal kept, oldest first; unreadable lines skipped.
    pub fn signals(&self) -> Vec<Signal> {
        fs::read_to_string(self.signals_path())
            .map(|t| {
                t.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Appends `s` to `learn/signals.jsonl`, bounded as
    /// [`crate::files::append_log`] says.
    pub fn add_signal(&self, s: &Signal) -> Result<(), String> {
        let line = serde_json::to_string(s).map_err(|e| e.to_string())?;
        crate::files::append_log(&self.signals_path(), &line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_round_trip_and_the_why_is_capped() {
        let d = tempfile::tempdir().unwrap();
        let dir = LearnDir::new(d.path());
        assert!(dir.signals().is_empty());
        let mut s = Signal::new(DOWN, "dashboard", 7);
        s.session = Some("dashboard__owner".into());
        s.why = clip_why(Some(&"x".repeat(WHY_MAX + 5)));
        dir.add_signal(&s).unwrap();
        dir.add_signal(&Signal::new(UNDO, "chat", 8)).unwrap();
        let all = dir.signals();
        assert_eq!(all.len(), 2);
        assert!(all[0].is_thumb() && !all[1].is_thumb());
        assert_eq!(all[0].why.as_ref().unwrap().chars().count(), WHY_MAX + 1);
        assert_eq!(clip_why(Some("  ")), None);
    }
}
