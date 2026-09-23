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
    Message { message: &'a Message },
    Event { line: &'a str },
    Meta { key: &'a str, value: &'a str },
}

impl Transcript {
    pub fn create(dir: &Path, session_id: &str) -> Result<Self, CoreError> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{session_id}.jsonl"));
        let t = Self { path };
        t.write(&Record::Meta { key: "session_id", value: session_id })?;
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

    fn write<T: Serialize>(&self, record: &T) -> Result<(), CoreError> {
        let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        f.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Read back all messages (for resume).
    pub fn read_messages(&self) -> Result<Vec<Message>, CoreError> {
        let text = fs::read_to_string(&self.path)?;
        let mut out = Vec::new();
        for l in text.lines() {
            let v: serde_json::Value = serde_json::from_str(l)?;
            if v.get("type").and_then(|t| t.as_str()) == Some("message") {
                out.push(serde_json::from_value(v["message"].clone())?);
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
}
