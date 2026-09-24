//! Everything the learning loop keeps, under `<data dir>/learn/`: the
//! playbook, the review cursor, one directory per pass (its journal,
//! changelog, before/after snapshots and diff), an event log and the lock.
//! See `docs/m16-learning-loop.md` §7.

use crate::budget::{Caps, Spent};
use crate::playbook::Applied;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A lock older than this belongs to a pass that died.
pub const STALE_LOCK: Duration = Duration::from_secs(3 * 3600);

pub const RUNNING: &str = "running";
pub const DONE: &str = "done";
pub const STOPPED_BUDGET: &str = "stopped-budget";
pub const STOPPED_ERRORS: &str = "stopped-errors";
pub const INTERRUPTED: &str = "interrupted";
pub const REVERTED: &str = "reverted";

/// The review cursor and the id counter; `state.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// Unix seconds of the newest episode a pass finished with.
    #[serde(default)]
    pub cursor: i64,
    /// The largest `[pb-N]` ever handed out; ids are never reused.
    #[serde(default)]
    pub last_id: u32,
}

/// One change a pass made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Change {
    Playbook {
        applied: Applied,
        reason: String,
        episode: String,
        #[serde(default)]
        gate: String,
    },
    Memory {
        new_id: i64,
        created: bool,
        replaced: Vec<i64>,
        content: String,
        reason: String,
    },
}

/// A proposal the pass didn't apply, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejected {
    /// The episode or memory cluster it came from.
    pub source: String,
    /// `add`, `edit`, `retire`, `merge`, or `answer` for an unusable reply.
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpisodeNote {
    pub key: String,
    pub label: String,
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevertNote {
    pub at: String,
    pub undone: Vec<String>,
    pub not_undone: Vec<String>,
}

/// A pass's journal, `passes/<id>/pass.json`; rewritten after every step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassRecord {
    pub id: String,
    pub status: String,
    pub trigger: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub caps: Caps,
    pub spent: Spent,
    pub day_spent_before: Spent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub cursor_before: i64,
    pub cursor_after: i64,
    #[serde(default)]
    pub episodes: Vec<EpisodeNote>,
    #[serde(default)]
    pub changes: Vec<Change>,
    #[serde(default)]
    pub rejected: Vec<Rejected>,
    #[serde(default)]
    pub skipped: Vec<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverted: Option<RevertNote>,
}

/// Writes `path` whole or not at all: a temp file beside it, then rename.
pub fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("no parent directory"))?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub struct LearnDir {
    root: PathBuf,
}

/// Held while a pass or a revert runs; removed on drop.
pub struct Lock {
    path: PathBuf,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl LearnDir {
    /// `<data dir>/learn`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("learn"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn playbook_path(&self) -> PathBuf {
        self.root.join("playbook.md")
    }

    /// The playbook's text; empty when there is none yet.
    pub fn read_playbook(&self) -> Result<String> {
        match fs::read_to_string(self.playbook_path()) {
            Ok(t) => Ok(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).context("reading the playbook"),
        }
    }

    pub fn write_playbook(&self, text: &str) -> Result<()> {
        write_atomic(&self.playbook_path(), text.as_bytes())
    }

    pub fn state(&self) -> Result<State> {
        match fs::read_to_string(self.root.join("state.json")) {
            Ok(t) => serde_json::from_str(&t).context("reading learn/state.json"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_state(&self, s: &State) -> Result<()> {
        write_atomic(
            &self.root.join("state.json"),
            serde_json::to_string_pretty(s)?.as_bytes(),
        )
    }

    /// Appends one event to `log.jsonl`.
    pub fn log(&self, pass: &str, event: &str, detail: serde_json::Value) {
        let line = serde_json::json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "pass": pass,
            "event": event,
            "detail": detail,
        });
        let _ = fs::create_dir_all(&self.root);
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("log.jsonl"))
        {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn pass_dir(&self, id: &str) -> PathBuf {
        self.root.join("passes").join(id)
    }

    pub fn new_pass_id(&self) -> String {
        let hex = uuid::Uuid::new_v4().simple().to_string();
        format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%d-%H%M%S"),
            &hex[..4]
        )
    }

    pub fn save_pass(&self, p: &PassRecord) -> Result<()> {
        write_atomic(
            &self.pass_dir(&p.id).join("pass.json"),
            serde_json::to_string_pretty(p)?.as_bytes(),
        )
    }

    pub fn load_pass(&self, id: &str) -> Result<PassRecord> {
        if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
            bail!("not a pass id: `{id}`");
        }
        let path = self.pass_dir(id).join("pass.json");
        let text =
            fs::read_to_string(&path).map_err(|_| anyhow!("there is no learning pass `{id}`"))?;
        serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
    }

    pub fn write_pass_file(&self, id: &str, name: &str, content: &str) -> Result<()> {
        write_atomic(&self.pass_dir(id).join(name), content.as_bytes())
    }

    pub fn read_pass_file(&self, id: &str, name: &str) -> Option<String> {
        fs::read_to_string(self.pass_dir(id).join(name)).ok()
    }

    /// Every pass, oldest first.
    pub fn passes(&self) -> Vec<PassRecord> {
        let mut ids: Vec<String> = fs::read_dir(self.root.join("passes"))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids.iter()
            .filter_map(|id| self.load_pass(id).ok())
            .collect()
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join("lock")
    }

    /// Who holds the lock and since when, if anyone.
    pub fn lock_holder(&self) -> Option<(String, Duration)> {
        let path = self.lock_path();
        let holder = fs::read_to_string(&path).ok()?;
        let age = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .unwrap_or_default();
        Some((holder.trim().to_string(), age))
    }

    /// Takes the lock for `holder`. A lock older than [`STALE_LOCK`] is
    /// taken over; a live one is refused.
    pub fn lock(&self, holder: &str) -> Result<Lock> {
        fs::create_dir_all(&self.root)?;
        let path = self.lock_path();
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    f.write_all(holder.as_bytes())?;
                    return Ok(Lock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let (who, age) = self.lock_holder().unwrap_or_default();
                    if age < STALE_LOCK {
                        bail!(
                            "another learning pass is running ({who}, started {}s ago); try again later",
                            age.as_secs()
                        );
                    }
                    tracing::warn!(stale = %who, "taking over a stale learning-pass lock");
                    fs::remove_file(&path)?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        bail!("could not take the learning-pass lock")
    }

    /// Marks passes that are still `running` but hold no lock as
    /// `interrupted`, and removes their scratch copies. Returns their ids.
    /// Call with the lock held (so no pass is really running).
    pub fn recover(&self, holder: &str) -> Vec<String> {
        let mut out = Vec::new();
        for mut p in self.passes() {
            if p.status != RUNNING || p.id == holder {
                continue;
            }
            p.status = INTERRUPTED.into();
            p.notes.push(
                "the pass stopped midway (crash or kill); changes it applied are kept".into(),
            );
            if p.finished_at.is_none() {
                p.finished_at = Some(chrono::Utc::now().to_rfc3339());
            }
            let _ = self.save_pass(&p);
            remove_scratch(&self.pass_dir(&p.id));
            self.log(&p.id, "interrupted", serde_json::json!({}));
            out.push(p.id);
        }
        out
    }
}

/// Removes a pass directory's `scratch-*` copies.
pub fn remove_scratch(pass_dir: &Path) {
    if let Ok(rd) = fs::read_dir(pass_dir) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("scratch-") {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, status: &str) -> PassRecord {
        PassRecord {
            id: id.into(),
            status: status.into(),
            trigger: "manual".into(),
            started_at: "2026-09-24T03:00:00Z".into(),
            finished_at: None,
            caps: Caps {
                usd_per_pass: 0.5,
                usd_per_day: 1.0,
                tokens_per_pass: 1,
                tokens_per_day: 1,
            },
            spent: Spent::default(),
            day_spent_before: Spent::default(),
            check: None,
            workspace: None,
            cursor_before: 0,
            cursor_after: 0,
            episodes: vec![],
            changes: vec![],
            rejected: vec![],
            skipped: vec![],
            notes: vec![],
            reverted: None,
        }
    }

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = LearnDir::new(tmp.path());
        let lock = dir.lock("pass-a").unwrap();
        let err = dir.lock("pass-b").err().unwrap().to_string();
        assert!(err.contains("pass-a"), "{err}");
        drop(lock);
        let _again = dir.lock("pass-b").unwrap();
    }

    #[test]
    fn a_running_pass_without_the_lock_is_marked_interrupted() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = LearnDir::new(tmp.path());
        dir.save_pass(&record("20260924-030000-aaaa", RUNNING))
            .unwrap();
        dir.save_pass(&record("20260924-040000-bbbb", DONE))
            .unwrap();
        fs::create_dir_all(dir.pass_dir("20260924-030000-aaaa").join("scratch-1")).unwrap();
        let _lock = dir.lock("20260924-050000-cccc").unwrap();
        assert_eq!(
            dir.recover("20260924-050000-cccc"),
            vec!["20260924-030000-aaaa"]
        );
        let p = dir.load_pass("20260924-030000-aaaa").unwrap();
        assert_eq!(p.status, INTERRUPTED);
        assert!(!dir.pass_dir(&p.id).join("scratch-1").exists());
        assert_eq!(dir.passes().len(), 2);
        assert!(dir.load_pass("../x").is_err());
    }

    #[test]
    fn state_and_playbook_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = LearnDir::new(tmp.path());
        assert_eq!(dir.read_playbook().unwrap(), "");
        assert_eq!(dir.state().unwrap().cursor, 0);
        dir.write_playbook("- [pb-1] x\n").unwrap();
        dir.save_state(&State {
            cursor: 42,
            last_id: 1,
        })
        .unwrap();
        assert_eq!(dir.read_playbook().unwrap(), "- [pb-1] x\n");
        assert_eq!(dir.state().unwrap().cursor, 42);
    }
}
