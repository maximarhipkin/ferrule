//! M56.5: a turn a restart cut short is taken up again. While a person's
//! turn runs, its lane keeps `<sessions>/<sid>.turn.json`: the message it
//! answers, word for word, and the tools it called that may have changed
//! something. The turn's end removes it, unless a restart ended the turn;
//! a crash never gets to. At the next start each one left is renamed
//! `<sid>.interrupted.json` and its chat is told: a turn that had only
//! read runs again at once, any other is offered (`/continue`). So is one
//! that was itself a resumed turn, so a turn that crashes the process
//! can't loop.

use crate::message::InboundMessage;
use ferrule_core::ToolRegistry;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The command that resumes an interrupted turn (`/continue drop` lets it
/// go). `/resume` is the kill switch's.
pub const CONTINUE: &str = "/continue";

/// What a running turn's marker holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlight {
    /// The message the turn answers, as it arrived.
    pub msg: InboundMessage,
    /// Unix seconds.
    pub started: i64,
    /// Tools it called that aren't read-only, each once, in order.
    #[serde(default)]
    pub acted: Vec<String>,
    /// It was already taken up once after a restart.
    #[serde(default)]
    pub resumed: bool,
}

impl InFlight {
    /// Safe to run again without asking: it only read, and it isn't a
    /// second try.
    pub fn runs_again(&self) -> bool {
        self.acted.is_empty() && !self.resumed
    }
}

/// The marker of a running turn.
pub(crate) fn turn_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.turn.json"))
}

/// An interrupted turn waiting for its chat's answer.
pub(crate) fn offer_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.interrupted.json"))
}

pub(crate) fn read(path: &Path) -> Option<InFlight> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "an unreadable turn marker; dropping it");
            let _ = std::fs::remove_file(path);
            None
        }
    }
}

/// One lane's marker, written as its turn goes.
pub(crate) struct Marker {
    path: PathBuf,
    /// Which tools only read (the agent's own registry).
    tools: ToolRegistry,
    turn: Mutex<Option<InFlight>>,
}

impl Marker {
    pub(crate) fn new(dir: &Path, session_id: &str, tools: ToolRegistry) -> Self {
        Self {
            path: turn_path(dir, session_id),
            tools,
            turn: Mutex::new(None),
        }
    }

    /// A person's turn starts on `msg`; `resumed` when it's a second try.
    pub(crate) fn begin(&self, msg: &InboundMessage, resumed: bool) {
        let turn = InFlight {
            msg: msg.clone(),
            started: chrono::Utc::now().timestamp(),
            acted: Vec::new(),
            resumed,
        };
        self.write(&turn);
        *self.turn.lock().unwrap() = Some(turn);
    }

    /// The turn calls `tool`; noted once if it may change something.
    pub(crate) fn called(&self, tool: &str) {
        if self.tools.read_only(tool) {
            return;
        }
        let mut turn = self.turn.lock().unwrap();
        let Some(t) = turn.as_mut() else {
            return;
        };
        if t.acted.iter().any(|a| a == tool) {
            return;
        }
        t.acted.push(tool.to_string());
        self.write(t);
    }

    /// The turn is over: `keep` (a restart ended it) leaves the file for
    /// the next start.
    pub(crate) fn end(&self, keep: bool) {
        let had = self.turn.lock().unwrap().take().is_some();
        if had && !keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn write(&self, turn: &InFlight) {
        let bytes = serde_json::to_vec_pretty(turn).unwrap_or_default();
        if let Err(e) = crate::health::write_atomic(&self.path, &bytes) {
            tracing::warn!(error = %e, "couldn't write the turn marker");
        }
    }
}

/// The goal a resumed turn runs on: the original request, word for word.
pub(crate) fn goal(original: &str) -> String {
    format!(
        "[Resuming after a restart] Your previous turn was interrupted by a restart before it \
         finished. The conversation above shows how far it got. Check what was already done, \
         don't repeat a step that changed something, and finish the request. It was:\n\n{original}"
    )
}

/// The chat's message about an interrupted turn: picked up again, or
/// offered with why it isn't.
pub(crate) fn notice(turn: &InFlight) -> String {
    let text = crate::health::clip(turn.msg.text.trim(), 200);
    let what = if text.is_empty() {
        "your last request".to_string()
    } else {
        format!("your request '{text}'")
    };
    if turn.runs_again() {
        format!(
            "A restart interrupted {what}. It had only read so far, so I'm picking it up again."
        )
    } else if turn.acted.is_empty() {
        format!(
            "A restart interrupted {what} again, after I had already picked it up once, so I won't \
             start it a third time on my own. Resume it?"
        )
    } else {
        format!(
            "A restart interrupted {what}. It had already used {}, which may have changed something, \
             so resuming could repeat a step. Resume it?",
            turn.acted
                .iter()
                .map(|t| format!("`{t}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}
