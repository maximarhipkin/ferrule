//! What the pass reviews: one thing that went wrong recently, with the goal,
//! the outcome and the tail of its transcript (`docs/m16-learning-loop.md`
//! §1). Scheduled-task runs are collected by the caller, which owns the task
//! store; retried sessions are found here by scanning `sessions/`.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// The verifier's failure marker (`ferrule-core`'s agent loop).
pub const VERIFY_MARKER: &str = "fails, so this isn't done yet";
/// The marker of a run stopped before it finished (step limit, budget …).
pub const STOP_MARKER: &str = "[ferrule] Stopping here:";
/// Characters of transcript the reflector sees, and of each tool result.
pub const TAIL_CHARS: usize = 12_000;
pub const TOOL_RESULT_CHARS: usize = 1_500;

#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    /// Stable id: `task:<task id>:<run id>` or `session:<session id>`.
    pub key: String,
    /// For people: the task name or the session id.
    pub label: String,
    /// What the run was asked to do; the gate re-runs it.
    pub goal: String,
    /// `failed`, `incomplete`, `retried` or `stopped`.
    pub outcome: String,
    /// The run's detail or the marker line.
    pub detail: String,
    /// A later run of the same task succeeded, or the session finished
    /// after its retries.
    pub fixed: bool,
    pub transcript: Option<PathBuf>,
    /// Unix seconds; the review cursor moves over it.
    pub at: i64,
}

/// Transcript lines that are messages, as JSON; unreadable lines skipped.
fn messages(path: &Path) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("message"))
        .map(|v| v["message"].clone())
        .collect()
}

fn content(m: &Value) -> &str {
    m.get("content").and_then(Value::as_str).unwrap_or("")
}

fn role(m: &Value) -> &str {
    m.get("role").and_then(Value::as_str).unwrap_or("")
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    cut(line, max)
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

/// Sessions under `dir` changed after `cursor` whose transcript shows a
/// verifier retry or an early stop. Scheduler sessions are left to the
/// caller (they are reviewed per run).
pub fn scan_sessions(dir: &Path, cursor: i64) -> Vec<Episode> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if id.starts_with("scheduler__") {
            continue;
        }
        let at = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if at <= cursor {
            continue;
        }
        if let Some(ep) = session_episode(id, &path, at) {
            out.push(ep);
        }
    }
    out.sort_by(|a, b| a.at.cmp(&b.at).then(a.key.cmp(&b.key)));
    out
}

fn session_episode(id: &str, path: &Path, at: i64) -> Option<Episode> {
    let msgs = messages(path);
    let goal = msgs
        .iter()
        .find(|m| role(m) == "user" && !content(m).starts_with("[ferrule]"))
        .map(|m| content(m).to_string())?;
    let markers: Vec<(usize, &str)> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| role(m) == "user")
        .map(|(i, m)| (i, content(m)))
        .filter(|(_, c)| {
            (c.starts_with("[ferrule]") && c.contains(VERIFY_MARKER)) || c.starts_with(STOP_MARKER)
        })
        .collect();
    let (_, last) = *markers.last()?;
    let stopped = markers.iter().any(|(_, c)| c.starts_with(STOP_MARKER));
    Some(Episode {
        key: format!("session:{id}"),
        label: id.to_string(),
        goal,
        outcome: if stopped { "stopped" } else { "retried" }.into(),
        detail: first_line(last, 300),
        // Retried and then finished without being stopped.
        fixed: !stopped,
        transcript: Some(path.to_path_buf()),
        at,
    })
}

/// The last [`TAIL_CHARS`] of a transcript's messages, for the reflector:
/// one block per message, tool results cut to [`TOOL_RESULT_CHARS`].
pub fn render_tail(path: &Path) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for m in messages(path) {
        let mut b = format!("[{}]", role(&m));
        let c = content(&m);
        if !c.is_empty() {
            let c = if role(&m) == "tool" {
                cut(c, TOOL_RESULT_CHARS)
            } else {
                c.to_string()
            };
            b.push(' ');
            b.push_str(&c);
        }
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let name = call.get("name").and_then(Value::as_str).unwrap_or("?");
                let args = call
                    .get("arguments")
                    .map(Value::to_string)
                    .unwrap_or_default();
                b.push_str(&format!("\n  -> {name} {}", cut(&args, TOOL_RESULT_CHARS)));
            }
        }
        blocks.push(b);
    }
    let mut out = String::new();
    let mut total = 0;
    let mut kept = Vec::new();
    for b in blocks.iter().rev() {
        let n = b.chars().count() + 1;
        if total + n > TAIL_CHARS {
            if kept.is_empty() {
                // One huge message: keep its end.
                let skip = b.chars().count().saturating_sub(TAIL_CHARS);
                kept.push(b.chars().skip(skip).collect::<String>());
            }
            break;
        }
        total += n;
        kept.push(b.clone());
    }
    if kept.len() < blocks.len() {
        out.push_str(&format!(
            "[… {} earlier messages left out]\n",
            blocks.len() - kept.len()
        ));
    }
    for b in kept.iter().rev() {
        out.push_str(b);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_core::{Message, Transcript};

    fn session(dir: &Path, id: &str, msgs: &[Message]) {
        let t = Transcript::create(dir, id).unwrap();
        for m in msgs {
            t.log_message(m).unwrap();
        }
    }

    #[test]
    fn finds_retried_and_stopped_sessions_only() {
        let d = tempfile::tempdir().unwrap();
        session(
            d.path(),
            "retried",
            &[
                Message::system("sys"),
                Message::user("fix the build"),
                Message::user(
                    "[ferrule] `cargo test` fails, so this isn't done yet. Fix it.\n\nerror",
                ),
                Message::assistant(Some("done".into()), vec![], None),
            ],
        );
        session(
            d.path(),
            "stopped",
            &[
                Message::user("write the report"),
                Message::user("[ferrule] Stopping here: it reached the limit of 2 steps. Don't call any tools."),
            ],
        );
        session(
            d.path(),
            "clean",
            &[
                Message::user("hi"),
                Message::assistant(Some("hello".into()), vec![], None),
            ],
        );
        session(
            d.path(),
            "scheduler__t1",
            &[
                Message::user("task"),
                Message::user("[ferrule] Stopping here: x"),
            ],
        );
        let eps = scan_sessions(d.path(), 0);
        let mut keys: Vec<_> = eps.iter().map(|e| e.key.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["session:retried", "session:stopped"]);
        let r = eps.iter().find(|e| e.label == "retried").unwrap();
        assert_eq!(r.goal, "fix the build");
        assert_eq!(r.outcome, "retried");
        assert!(r.fixed);
        let s = eps.iter().find(|e| e.label == "stopped").unwrap();
        assert_eq!(s.outcome, "stopped");
        assert!(!s.fixed);
        assert!(scan_sessions(d.path(), i64::MAX).is_empty());
    }

    #[test]
    fn the_tail_keeps_the_end_and_cuts_tool_results() {
        let d = tempfile::tempdir().unwrap();
        let mut msgs = vec![Message::user("goal")];
        for i in 0..40 {
            msgs.push(Message::tool_result(format!("c{i}"), "x".repeat(2_000)));
        }
        msgs.push(Message::assistant(Some("the end".into()), vec![], None));
        session(d.path(), "s", &msgs);
        let tail = render_tail(&d.path().join("s.jsonl"));
        assert!(tail.chars().count() <= TAIL_CHARS + 100, "{}", tail.len());
        assert!(tail.starts_with("[… "));
        assert!(tail.ends_with("[assistant] the end\n"));
        assert!(!tail.contains(&"x".repeat(TOOL_RESULT_CHARS + 1)));
    }
}
