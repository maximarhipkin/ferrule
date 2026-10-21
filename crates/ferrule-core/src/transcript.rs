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

/// M53: which harness produced a session (`docs/m53-contrastive-probing.md`
/// §4). Hashes are FNV-1a 64 — stable across builds, no dependency.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HarnessStamp {
    /// Skill name + hash of its SKILL.md.
    pub skills: Vec<(String, u64)>,
    pub playbook: Option<u64>,
    pub config: Option<u64>,
    /// M58.5: the system prompt as built, its workspace path replaced, so
    /// a prompt line that changed shows (`ferrule eval` sets it).
    pub prompt: Option<u64>,
    /// M58.5: the tool definitions offered, names, descriptions and
    /// schemas (`ferrule eval` sets it).
    pub tools: Option<u64>,
}

/// FNV-1a 64, hex-rendered by [`HarnessStamp::encode`].
pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl HarnessStamp {
    /// `skills=name@hex,name@hex;playbook=hex;config=hex;prompt=hex;tools=hex`
    /// — absent parts are left out, so the line never claims a part it
    /// didn't see.
    pub fn encode(&self) -> String {
        let mut s = String::new();
        let sep = |s: &mut String| {
            if !s.is_empty() {
                s.push(';');
            }
        };
        if !self.skills.is_empty() {
            sep(&mut s);
            s.push_str("skills=");
            for (i, (name, h)) in self.skills.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&format!("{name}@{h:016x}"));
            }
        }
        if let Some(p) = self.playbook {
            sep(&mut s);
            s.push_str(&format!("playbook={p:016x}"));
        }
        for (key, value) in [
            ("config", self.config),
            ("prompt", self.prompt),
            ("tools", self.tools),
        ] {
            if let Some(v) = value {
                sep(&mut s);
                s.push_str(&format!("{key}={v:016x}"));
            }
        }
        s
    }
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

    /// Open an existing transcript without writing anything (a fork reads
    /// its parent; `create` would append a meta line to it).
    pub fn open(dir: &Path, session_id: &str) -> Result<Self, CoreError> {
        let path = dir.join(format!("{session_id}.jsonl"));
        if !path.exists() {
            return Err(CoreError::Aborted(format!(
                "no session transcript at {}",
                path.display()
            )));
        }
        Ok(Self { path })
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

    /// A metadata pair (`parent`, `fork_at`, …).
    pub fn log_meta(&self, key: &str, value: &str) -> Result<(), CoreError> {
        self.write(&Record::Meta { key, value })
    }

    /// M53: one append-only `harness` meta line naming what produced this
    /// session's prompts — the active skills with content hashes, the
    /// playbook hash, the config hash. Hashes, never contents.
    pub fn log_harness(&self, stamp: &HarnessStamp) -> Result<(), CoreError> {
        self.log_meta("harness", &stamp.encode())
    }

    /// The metadata pairs, in the order they were written.
    pub fn read_meta(&self) -> Result<Vec<(String, String)>, CoreError> {
        let text = fs::read_to_string(&self.path)?;
        let mut out = Vec::new();
        for v in self.records(&text) {
            if v.get("type").and_then(|t| t.as_str()) == Some("meta") {
                let key = v["key"].as_str().unwrap_or_default().to_string();
                let value = v["value"].as_str().unwrap_or_default().to_string();
                out.push((key, value));
            }
        }
        Ok(out)
    }

    /// A branch of this session: a new transcript in the same directory,
    /// carrying `parent`/`fork_at` meta and this session's fold-applied
    /// messages up to `at` (all of them when None). The branch then grows
    /// on its own; the parent is never written to.
    pub fn fork(&self, session_id: &str, at: Option<usize>) -> Result<Transcript, CoreError> {
        let Some(dir) = self.path.parent() else {
            return Err(CoreError::Aborted("the transcript has no directory".into()));
        };
        let parent = self
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let messages = self.read_messages()?;
        let take = at.unwrap_or(messages.len()).min(messages.len());
        let branch = Transcript::create(dir, session_id)?;
        branch.log_meta("parent", &parent)?;
        branch.log_meta("fork_at", &take.to_string())?;
        for m in &messages[..take] {
            branch.log_message(m)?;
        }
        Ok(branch)
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

    /// Each line's record. A line that isn't JSON (torn by a crash or a
    /// full disk mid-write) is skipped, not fatal: failing here would
    /// resume the chat with no history at all. Requests are re-paired
    /// (`agent::paired`), so a call or result lost with it costs nothing
    /// more. ponytail: the record appended onto a torn line is lost with it.
    fn records(&self, text: &str) -> Vec<serde_json::Value> {
        text.lines()
            .filter_map(|l| match serde_json::from_str(l) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(path = %self.path.display(), error = %e, "transcript: skipping a line that isn't JSON");
                    None
                }
            })
            .collect()
    }

    /// Read back the messages a resume replays: every message record, with
    /// fold records applied — a fold replaces all but the last `kept`
    /// logged messages with its summary, so a resumed session continues
    /// from the compacted state instead of resurrecting the folded past.
    pub fn read_messages(&self) -> Result<Vec<Message>, CoreError> {
        let text = fs::read_to_string(&self.path)?;
        let mut out: Vec<Message> = Vec::new();
        for v in self.records(&text) {
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
        for v in self.records(&text) {
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

    #[test]
    fn a_fork_carries_the_fold_applied_prefix_and_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let t = Transcript::create(dir.path(), "s1").unwrap();
        for i in 0..5 {
            t.log_message(&Message::user(format!("m{i}"))).unwrap();
        }
        t.log_fold(&Message::user("summary so far"), 2).unwrap();

        let branch = t.fork("s2", None).unwrap();
        let meta = branch.read_meta().unwrap();
        assert!(meta.contains(&("parent".to_string(), "s1".to_string())));
        assert!(meta.contains(&("fork_at".to_string(), "3".to_string())));
        let msgs = branch.read_messages().unwrap();
        let bodies: Vec<&str> = msgs.iter().filter_map(|m| m.content.as_deref()).collect();
        assert_eq!(bodies, ["summary so far", "m3", "m4"]);

        // `at` cuts the prefix.
        let short = t.fork("s3", Some(1)).unwrap();
        let msgs = short.read_messages().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content.as_deref(), Some("summary so far"));

        // `open` reads without writing; a missing session is an error.
        assert!(Transcript::open(dir.path(), "nope").is_err());
        let before = std::fs::read_to_string(t.path()).unwrap();
        Transcript::open(dir.path(), "s1").unwrap();
        assert_eq!(before, std::fs::read_to_string(t.path()).unwrap());
    }

    #[test]
    fn the_harness_stamp_is_one_meta_line_of_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let t = Transcript::create(dir.path(), "s1").unwrap();
        let stamp = HarnessStamp {
            skills: vec![("review".into(), fnv64(b"skill body"))],
            playbook: Some(fnv64(b"- lesson one\n")),
            config: Some(fnv64(b"[agent]\n")),
            ..Default::default()
        };
        t.log_harness(&stamp).unwrap();
        let meta = t.read_meta().unwrap();
        let (key, value) = meta.iter().find(|(k, _)| k == "harness").unwrap();
        assert_eq!(key, "harness");
        assert_eq!(value, &stamp.encode());
        assert!(value.starts_with("skills=review@"));
        assert!(value.contains(";playbook=") && value.contains(";config="));
        // Hashes, never contents.
        assert!(!value.contains("skill body") && !value.contains("lesson one"));
        // Absent parts are left out.
        assert_eq!(HarnessStamp::default().encode(), "");
        assert!(!HarnessStamp {
            config: Some(1),
            ..Default::default()
        }
        .encode()
        .starts_with(';'));
        // Stable: the same bytes hash the same way across calls.
        assert_eq!(fnv64(b"skill body"), fnv64(b"skill body"));
        assert_ne!(fnv64(b"a"), fnv64(b"b"));
    }
}
