use crate::error::CoreError;
use crate::message::Message;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Append-only JSONL transcript per session — resumable, forkable, auditable.
#[derive(Debug, Clone)]
pub struct Transcript {
    path: PathBuf,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record<'a> {
    Message {
        message: &'a Message,
    },
    Event {
        line: &'a str,
    },
    Meta {
        key: &'a str,
        value: &'a str,
    },
    /// Compaction folded the log: `message` is the summary that replaced
    /// everything but the last `kept` logged messages. The folded messages
    /// stay in the file (append-only; `search_history` reads them); the
    /// fold only shapes what a resume replays.
    Fold {
        message: &'a Message,
        kept: usize,
    },
}

impl Transcript {
    pub fn create(dir: &Path, session_id: &str) -> Result<Self, CoreError> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{session_id}.jsonl"));
        let t = Self { path };
        t.write(&Record::Meta {
            key: "session_id",
            value: session_id,
        })?;
        Ok(t)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn log_message(&self, message: &Message) -> Result<(), CoreError> {
        self.write(&Record::Message { message })
    }

    pub fn log_event(&self, line: &str) -> Result<(), CoreError> {
        self.write(&Record::Event { line })
    }

    /// A compaction: the summary that replaced all but the last `kept`
    /// logged messages. Model-visible means logged — the summary reaches
    /// the model on every later turn, so it belongs in the log.
    pub fn log_fold(&self, summary: &Message, kept: usize) -> Result<(), CoreError> {
        self.write(&Record::Fold {
            message: summary,
            kept,
        })
    }

    fn write<T: Serialize>(&self, record: &T) -> Result<(), CoreError> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        f.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Read back the messages a resume replays: every message record, with
    /// fold records applied — a fold replaces all but the last `kept`
    /// logged messages with its summary, so a resumed session continues
    /// from the compacted state instead of resurrecting the folded past.
    pub fn read_messages(&self) -> Result<Vec<Message>, CoreError> {
        let text = fs::read_to_string(&self.path)?;
        let mut out: Vec<Message> = Vec::new();
        for l in text.lines() {
            let v: serde_json::Value = serde_json::from_str(l)?;
            match v.get("type").and_then(|t| t.as_str()) {
                Some("message") => out.push(serde_json::from_value(v["message"].clone())?),
                Some("fold") => {
                    let summary: Message = serde_json::from_value(v["message"].clone())?;
                    let kept = v["kept"].as_u64().unwrap_or(0) as usize;
                    let tail: Vec<Message> = out.split_off(out.len().saturating_sub(kept));
                    out.clear();
                    out.push(summary);
                    out.extend(tail);
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// Every message ever logged, fold summaries included and folds not
    /// applied: the append-only view, for audits and the
    /// model-visible-means-logged invariant.
    pub fn read_all_logged(&self) -> Result<Vec<Message>, CoreError> {
        let text = fs::read_to_string(&self.path)?;
        let mut out = Vec::new();
        for l in text.lines() {
            let v: serde_json::Value = serde_json::from_str(l)?;
            match v.get("type").and_then(|t| t.as_str()) {
                Some("message") | Some("fold") => {
                    out.push(serde_json::from_value(v["message"].clone())?)
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_messages() {
        let dir = tempfile::tempdir().unwrap();
        let t = Transcript::create(dir.path(), "s1").unwrap();
        t.log_message(&Message::system("sys")).unwrap();
        t.log_message(&Message::user("hello")).unwrap();
        t.log_event("tool ran").unwrap();
        let msgs = t.read_messages().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].content.as_deref(), Some("hello"));
    }

    #[test]
    fn a_fold_replaces_all_but_the_kept_tail_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let t = Transcript::create(dir.path(), "s1").unwrap();
        for i in 0..5 {
            t.log_message(&Message::user(format!("m{i}"))).unwrap();
        }
        t.log_fold(&Message::user("summary so far"), 2).unwrap();
        t.log_message(&Message::user("m5")).unwrap();

        let msgs = t.read_messages().unwrap();
        let bodies: Vec<&str> = msgs.iter().filter_map(|m| m.content.as_deref()).collect();
        assert_eq!(bodies, ["summary so far", "m3", "m4", "m5"]);

        // The append-only view keeps the folded past and the summary.
        let all = t.read_all_logged().unwrap();
        assert_eq!(all.len(), 7);
        assert!(all.iter().any(|m| m.content.as_deref() == Some("m0")));
        assert!(all
            .iter()
            .any(|m| m.content.as_deref() == Some("summary so far")));
    }
}
