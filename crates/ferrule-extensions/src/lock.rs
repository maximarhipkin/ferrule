//! `extensions.lock.json`: what is installed, from where, at which pin,
//! with which approved surface. Machine-written; the owner's config file is
//! never touched.
//!
//! Writers (a daemon, the owner's CLI) serialise through a sibling `.lk`
//! file created with `create_new`, taken over once it is older than
//! [`STALE`]. Each write goes to a temp file in the same directory and is
//! renamed over the lock, so a reader sees the old file or the new one.

use crate::error::{ExtError, Result};
use crate::scan::Finding;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub const LOCK_VERSION: u32 = 1;
const WAIT: Duration = Duration::from_secs(5);
const STALE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Installed by the model (allow-listed, or approved by the owner).
    Agent,
    /// A skill the agent wrote and verified itself (`skill_keep`).
    #[serde(rename = "self")]
    SelfWritten,
    /// Installed by the owner from the CLI.
    Owner,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Agent => "agent",
            Origin::SelfWritten => "self",
            Origin::Owner => "owner",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    /// Not loaded: a flagged or changed surface, or a drifted checkout.
    /// Only the owner resumes it.
    Suspended,
}

/// The owner's "this hit is fine" for one rule on one tool, as long as the
/// tool's surface is exactly what it was when waived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiver {
    pub item: String,
    pub rule: String,
    pub digest: String,
}

pub fn waived(waivers: &[Waiver], finding: &Finding, digest: &str) -> bool {
    waivers
        .iter()
        .any(|w| w.item == finding.item && w.rule == finding.rule && w.digest == digest)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEntry {
    /// The source as requested, e.g. `npm:@scope/pkg@1.2.3`.
    pub source: String,
    /// The exact version, or the commit SHA for git; none for a URL.
    #[serde(default)]
    pub pin: Option<String>,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// A git server's checkout, verified at every load.
    #[serde(default)]
    pub checkout: Option<PathBuf>,
    pub origin: Origin,
    pub installed_at: String,
    pub status: Status,
    #[serde(default)]
    pub reason: Option<String>,
    /// The approved surface: tool name → digest.
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
    #[serde(default)]
    pub waivers: Vec<Waiver>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    pub source: String,
    #[serde(default)]
    pub pin: Option<String>,
    pub origin: Origin,
    pub installed_at: String,
    pub status: Status,
    #[serde(default)]
    pub reason: Option<String>,
    /// SHA-256 of the installed `SKILL.md`.
    pub digest: String,
    #[serde(default)]
    pub waivers: Vec<Waiver>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockFile {
    pub version: u32,
    #[serde(default)]
    pub servers: BTreeMap<String, ServerEntry>,
    #[serde(default)]
    pub skills: BTreeMap<String, SkillEntry>,
}

impl Default for LockFile {
    fn default() -> Self {
        Self {
            version: LOCK_VERSION,
            servers: BTreeMap::new(),
            skills: BTreeMap::new(),
        }
    }
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[derive(Debug, Clone)]
pub struct LockStore {
    path: PathBuf,
}

impl LockStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A missing file is an empty lock; an unreadable or newer-format one
    /// is an error, and is never overwritten.
    pub fn load(&self) -> Result<LockFile> {
        let text = match fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(LockFile::default()),
            Err(e) => return Err(e.into()),
        };
        let lock: LockFile = serde_json::from_str(&text).map_err(|e| {
            ExtError::Lock(format!(
                "{} is corrupt ({e}); fix or remove it",
                self.path.display()
            ))
        })?;
        if lock.version > LOCK_VERSION {
            return Err(ExtError::Lock(format!(
                "{} is version {}, this build reads {LOCK_VERSION}",
                self.path.display(),
                lock.version
            )));
        }
        Ok(lock)
    }

    /// Read, change, write — all under the `.lk` file. `f` returning an
    /// error writes nothing.
    pub fn update<R>(&self, f: impl FnOnce(&mut LockFile) -> Result<R>) -> Result<R> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;
        let _guard = Guard::acquire(self.lk_path())?;
        let mut lock = self.load()?;
        let out = f(&mut lock)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(serde_json::to_string_pretty(&lock)?.as_bytes())?;
        tmp.write_all(b"\n")?;
        tmp.as_file().sync_all()?;
        let start = Instant::now();
        loop {
            match tmp.persist(&self.path) {
                Ok(_) => break,
                Err(e) if transient(&e.error) && start.elapsed() < WAIT => {
                    tmp = e.file;
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(e.error.into()),
            }
        }
        Ok(out)
    }

    fn lk_path(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".lk");
        p.into()
    }
}

/// Windows answers "access denied" for a moment where other systems
/// don't: creating the `.lk` file while the last holder's delete is still
/// pending, or renaming over the lock while something (an antivirus or
/// indexer scan) has it open. Both are waited out like a held lock.
/// Elsewhere it is a real permission error.
fn transient(e: &std::io::Error) -> bool {
    cfg!(windows) && e.kind() == ErrorKind::PermissionDenied
}

struct Guard(PathBuf);

impl Guard {
    fn acquire(path: PathBuf) -> Result<Self> {
        let start = Instant::now();
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self(path));
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists || transient(&e) => {
                    let age = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok());
                    if age.is_some_and(|a| a > STALE) {
                        tracing::warn!(path = %path.display(), "taking over a stale extensions lock");
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if start.elapsed() > WAIT {
                        return Err(ExtError::Lock(format!(
                            "{} is held by another process; try again",
                            path.display()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn entry() -> SkillEntry {
        SkillEntry {
            source: "git:file:///x".into(),
            pin: Some("a".repeat(40)),
            origin: Origin::Agent,
            installed_at: now(),
            status: Status::Active,
            reason: None,
            digest: "d".into(),
            waivers: vec![],
        }
    }

    #[test]
    fn round_trips_and_a_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = LockStore::new(dir.path().join("sub/extensions.lock.json"));
        assert_eq!(store.load().unwrap(), LockFile::default());
        store
            .update(|l| {
                l.skills.insert("pdf".into(), entry());
                Ok(())
            })
            .unwrap();
        let back = store.load().unwrap();
        assert_eq!(back.skills["pdf"].origin, Origin::Agent);
        let text = fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("\"origin\": \"agent\"") && text.contains("\"status\": \"active\""));
        assert!(!store.lk_path().exists());
    }

    #[test]
    fn a_failed_update_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = LockStore::new(dir.path().join("l.json"));
        let r: Result<()> = store.update(|l| {
            l.skills.insert("x".into(), entry());
            Err(crate::error::refused("no"))
        });
        assert!(r.is_err());
        assert!(!store.path().exists());
    }

    #[test]
    fn a_corrupt_lock_is_an_error_and_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = LockStore::new(dir.path().join("l.json"));
        fs::write(store.path(), "{ not json").unwrap();
        assert!(matches!(store.load(), Err(ExtError::Lock(_))));
        assert!(store.update(|_| Ok(())).is_err());
        assert_eq!(fs::read_to_string(store.path()).unwrap(), "{ not json");
    }

    #[test]
    fn concurrent_writers_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(LockStore::new(dir.path().join("l.json")));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for j in 0..5 {
                        store
                            .update(|l| {
                                l.skills.insert(format!("s{i}-{j}"), entry());
                                Ok(())
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(store.load().unwrap().skills.len(), 40);
    }

    #[test]
    fn a_held_lock_times_out_and_a_stale_one_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let store = LockStore::new(dir.path().join("l.json"));
        fs::write(store.lk_path(), "999999").unwrap();
        let t = Instant::now();
        assert!(matches!(store.update(|_| Ok(())), Err(ExtError::Lock(_))));
        assert!(t.elapsed() >= WAIT);

        let old = SystemTime::now() - Duration::from_secs(120);
        fs::File::options()
            .write(true)
            .open(store.lk_path())
            .unwrap()
            .set_modified(old)
            .unwrap();
        store.update(|_| Ok(())).unwrap();
        assert!(store.path().exists() && !store.lk_path().exists());
    }
}
