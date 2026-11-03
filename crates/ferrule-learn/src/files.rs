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
    /// M53.4: when a pass last kept each `[pb-N]` (added, edited or
    /// promoted it), Unix seconds. The oldest is the first to make room.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub touched: std::collections::BTreeMap<u32, i64>,
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
        /// M53.2: the transcript moment the reflector cited.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        evidence: String,
    },
    Memory {
        new_id: i64,
        created: bool,
        replaced: Vec<i64>,
        content: String,
        reason: String,
    },
    /// M53.3: a skill kept in `<data>/learned-skills/<name>/`.
    Skill {
        name: String,
        description: String,
        /// `add`; `replace` when a learned skill of that name was there;
        /// `promote` (M53.4) when that one was kept in another scope.
        op: String,
        /// SHA-256 of the SKILL.md kept.
        digest: String,
        /// M53.4: where it applies; `None` is everywhere (a `promote`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        /// The skill it replaced, under the pass dir's `skills/`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backup: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous: Option<crate::skills::Entry>,
        /// Its scripts, by path.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scripts: Vec<String>,
        reason: String,
        episode: String,
        gate: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        evidence: String,
        /// What the validators couldn't check here.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notes: Vec<String>,
    },
}

/// What a proposed change would do, once the owner keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "lowercase")]
pub enum Proposal {
    /// A playbook delta: `add`, `edit` or `promote`.
    Lesson {
        op: String,
        /// The lesson an edit or a promotion is of.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// The reflector's pick, should the add need to make room.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retire: Option<u32>,
    },
    /// A skill, its validated draft kept under the pass dir.
    Skill {
        name: String,
        description: String,
        /// The draft, under the pass dir.
        dir: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scripts: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notes: Vec<String>,
    },
}

pub const PROPOSED: &str = "proposed";
pub const KEPT: &str = "kept";
pub const DROPPED: &str = "dropped";

/// "Proposed, not proven": a change the gate could neither prove nor
/// disprove (no check, the judge couldn't decide, no control run fitted
/// the caps). It does nothing until the owner keeps it; then it lands as
/// a change of its pass, revertable like the others.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposed {
    /// `p1`, `p2`, … within the pass.
    pub id: String,
    pub change: Proposal,
    /// Where it would apply; `None` is everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub reason: String,
    pub episode: String,
    /// Why the gate couldn't prove it.
    pub gate: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub evidence: String,
    /// [`PROPOSED`], [`KEPT`] or [`DROPPED`].
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    /// Once kept: its number among the pass's changes, from 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_as: Option<usize>,
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

/// M53.5: one change undone on its own (`learn revert --change N`, the
/// owner's Undo button). `change` counts from 1, as `learn show` does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeRevert {
    pub change: usize,
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
    /// M53.5: changes undone one by one; a pass revert skips them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undone: Vec<ChangeRevert>,
    /// Changes the gate couldn't prove, waiting for the owner's Keep or
    /// Drop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed: Vec<Proposed>,
}

impl PassRecord {
    /// Whether change `n` (from 1) was undone on its own.
    pub fn is_undone(&self, n: usize) -> bool {
        self.undone.iter().any(|u| u.change == n)
    }
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

/// `learn/signals.jsonl` and `learn/usage.jsonl` grow with use: an append
/// that takes one past this many bytes prunes it ([`prune_log`]).
pub const LOG_PRUNE_AT: u64 = 4 * 1024 * 1024;
/// What a prune leaves at most: its lines older than [`LOG_KEEP_DAYS`]
/// go first, then the oldest until the file is under this.
pub const LOG_KEEP_TO: u64 = 2 * 1024 * 1024;
/// How long a line of those logs is kept once one is pruned.
pub const LOG_KEEP_DAYS: i64 = 90;

/// `<log>.lock` beside a log: appends hold it shared, a prune holds it
/// alone while it rewrites (as the trust audit log does), so no line is
/// lost in the swap. `None` when it can't be made: writing goes on
/// unlocked.
fn log_lock(path: &Path) -> Option<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))
        .ok()
}

/// One process's writers take turns, the lock file keeps other processes'.
static LOG_HELD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Appends `line` (one JSON object with a Unix-seconds `at`) to `path`;
/// past [`LOG_PRUNE_AT`] the file is pruned to [`LOG_KEEP_DAYS`] and
/// [`LOG_KEEP_TO`].
pub fn append_log(path: &Path, line: &str) -> Result<(), String> {
    let _held = LOG_HELD.lock().unwrap_or_else(|e| e.into_inner());
    let err = |e: std::io::Error| format!("couldn't write {}: {e}", path.display());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(err)?;
    }
    let len = {
        let lock = log_lock(path);
        if let Some(l) = &lock {
            let _ = l.lock_shared();
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(err)?;
        writeln!(f, "{line}").map_err(err)?;
        f.metadata().map(|m| m.len()).unwrap_or(0)
    };
    if len > LOG_PRUNE_AT {
        let cutoff = chrono::Utc::now().timestamp() - LOG_KEEP_DAYS * 86_400;
        match prune_log(path, cutoff, LOG_KEEP_TO) {
            Ok((kept, dropped)) => {
                tracing::info!(log = %path.display(), kept, dropped, "learning: pruned a log")
            }
            Err(e) => tracing::warn!(log = %path.display(), "learning: a log wasn't pruned: {e}"),
        }
    }
    Ok(())
}

/// Drops the lines of `path` whose `at` is before `cutoff` (Unix seconds),
/// then the oldest until the rest is at most `keep_to` bytes; a line that
/// doesn't parse is kept by the age rule, like any other by the size one.
/// The file is rewritten through a temporary one and a rename, under the
/// lock every append takes. Returns (kept, dropped).
pub fn prune_log(path: &Path, cutoff: i64, keep_to: u64) -> std::io::Result<(usize, usize)> {
    let lock = log_lock(path);
    if let Some(l) = &lock {
        l.lock()?;
    }
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e),
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let old = |l: &str| {
        serde_json::from_str::<serde_json::Value>(l)
            .ok()
            .and_then(|v| v["at"].as_i64())
            .is_some_and(|at| at < cutoff)
    };
    let mut keep: std::collections::VecDeque<&str> =
        lines.iter().copied().filter(|l| !old(l)).collect();
    let mut size: u64 = keep.iter().map(|l| l.len() as u64 + 1).sum();
    while size > keep_to {
        let Some(l) = keep.pop_front() else { break };
        size -= l.len() as u64 + 1;
    }
    let dropped = lines.len() - keep.len();
    if dropped > 0 {
        let mut out = String::with_capacity(size as usize);
        for l in &keep {
            out.push_str(l);
            out.push('\n');
        }
        write_atomic(path, out.as_bytes()).map_err(std::io::Error::other)?;
    }
    Ok((keep.len(), dropped))
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
            undone: vec![],
            proposed: vec![],
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
    fn a_log_past_its_size_drops_old_lines_then_the_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("learn/usage.jsonl");
        let now = chrono::Utc::now().timestamp();
        let line = |at: i64, pad: usize| format!(r#"{{"at":{at},"x":"{}"}}"#, "y".repeat(pad));
        // Age, then size: two old lines go, then the oldest new one.
        let body: String = [
            line(1, 10),
            line(now - 100 * 86_400, 10),
            "not json".to_string(),
            line(now - 3, 10),
            line(now - 2, 10),
        ]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        let cutoff = now - LOG_KEEP_DAYS * 86_400;
        let keep_to = 2 * (line(now - 2, 10).len() as u64 + 1);
        assert_eq!(prune_log(&path, cutoff, keep_to).unwrap(), (2, 3));
        let left = fs::read_to_string(&path).unwrap();
        assert_eq!(
            left,
            format!("{}\n{}\n", line(now - 3, 10), line(now - 2, 10))
        );
        assert_eq!(prune_log(&path, cutoff, keep_to).unwrap(), (2, 0));

        // An append past LOG_PRUNE_AT prunes: here every old line goes.
        let old = line(1, 1000);
        let n = (LOG_PRUNE_AT as usize / (old.len() + 1)) + 1;
        fs::write(&path, format!("{old}\n").repeat(n)).unwrap();
        append_log(&path, &line(now, 10)).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{}\n", line(now, 10))
        );
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
            ..Default::default()
        })
        .unwrap();
        assert_eq!(dir.read_playbook().unwrap(), "- [pb-1] x\n");
        assert_eq!(dir.state().unwrap().cursor, 42);
    }
}
