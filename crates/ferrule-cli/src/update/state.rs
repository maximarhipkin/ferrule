//! What the updater remembers (docs/m36-self-update.md §3.2): the last
//! check, pinned versions and the last 20 events, where the daemon can read
//! but (in system scope) not write; the daemon's request to run now; and the
//! apply lock.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const STATE_FILE: &str = "state.json";
pub const REQUEST_FILE: &str = "request";
pub const TOLD_FILE: &str = "told.json";
const LOCK_FILE: &str = "apply.lock";
const KEEP_EVENTS: usize = 20;
const MAX_REQUEST: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Ferrule went from `from` to `to`.
    Updated,
    /// `to` didn't come up healthy; `from` is back and `to` is pinned.
    RolledBack,
    /// An update was found but not installed: a failed check, a failed swap.
    Failed,
    /// `claude` went from `from` to `to`.
    ClaudeUpdated,
    /// `claude` couldn't be updated.
    ClaudeFailed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: u64,
    pub kind: EventKind,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    /// The release notes' first line, or what went wrong.
    #[serde(default)]
    pub notes: String,
    /// Unix seconds.
    pub at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Unix seconds of the last release check, and whether it worked.
    pub last_check: Option<u64>,
    pub last_check_ok: Option<bool>,
    /// Why the last check failed.
    pub last_error: Option<String>,
    /// Unix seconds of the last check that worked (the self-check's "failing
    /// for days").
    pub last_ok: Option<u64>,
    /// The newest release the last check saw (`v0.6.0`), installed or not.
    pub latest: Option<String>,
    /// A signed release has been seen.
    pub signed_seen: bool,
    /// Tags never installed automatically (a rollback pins).
    pub pinned: Vec<String>,
    pub events: Vec<Event>,
    /// Unix seconds of the last `claude` update attempt.
    pub claude_checked: Option<u64>,
    /// What the last `claude` check found: installed and latest versions.
    pub claude_installed: Option<String>,
    pub claude_latest: Option<String>,
    /// The last request id a claude run answered.
    pub claude_answered: Option<u64>,
}

impl State {
    /// A missing or unreadable file is a fresh state.
    pub fn load(dir: &Path) -> Self {
        std::fs::read(dir.join(STATE_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        ferrule_tools::fs_tools::write_atomic(&dir.join(STATE_FILE), &bytes)
            .with_context(|| format!("writing {}", dir.join(STATE_FILE).display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Readable by the service's user in system scope.
            let _ = std::fs::set_permissions(
                dir.join(STATE_FILE),
                std::fs::Permissions::from_mode(0o644),
            );
        }
        Ok(())
    }

    pub fn push(&mut self, kind: EventKind, from: &str, to: &str, notes: &str) -> &Event {
        let id = self.events.last().map_or(1, |e| e.id + 1);
        self.events.push(Event {
            id,
            kind,
            from: from.into(),
            to: to.into(),
            notes: notes.into(),
            at: now(),
        });
        let extra = self.events.len().saturating_sub(KEEP_EVENTS);
        self.events.drain(..extra);
        self.events.last().expect("just pushed")
    }

    pub fn is_pinned(&self, tag: &str) -> bool {
        self.pinned.iter().any(|p| super::release::same_tag(p, tag))
    }

    pub fn pin(&mut self, tag: &str) {
        if !self.is_pinned(tag) {
            self.pinned.push(tag.into());
        }
    }

    pub fn unpin(&mut self, tag: &str) {
        self.pinned.retain(|p| !super::release::same_tag(p, tag));
    }

    pub fn last_update(&self) -> Option<&Event> {
        self.events
            .iter()
            .rev()
            .find(|e| matches!(e.kind, EventKind::Updated | EventKind::RolledBack))
    }
}

/// What the daemon asks of the apply unit: `{ferrule, claude, to}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Request {
    pub ferrule: bool,
    pub claude: bool,
    /// Only `ferrule update --to` sets this, from a terminal; a request
    /// file's `to` is ignored (the daemon doesn't pick versions).
    pub to: Option<String>,
    /// Echoed back in the state's `claude_answered` once the unit has run
    /// claude's update, so the gateway knows its answer is in.
    pub id: u64,
}

/// Ask the apply unit to run now: the path unit watches for this file.
pub fn write_request(data: &Path, request: &Request) -> Result<()> {
    let path = data.join("update").join(REQUEST_FILE);
    let bytes = serde_json::to_vec(&Request {
        to: None,
        ..request.clone()
    })?;
    ferrule_tools::fs_tools::write_atomic(&path, &bytes)
        .with_context(|| format!("writing {}", path.display()))
}

/// Read the request, then delete it (§3.2 1): at most 4 KiB, never through
/// a symlink, anything unparsable ignored. `None` when there is none.
pub fn take_request(data: &Path) -> Option<Request> {
    let path = data.join("update").join(REQUEST_FILE);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    let read = if meta.file_type().is_file() && meta.len() <= MAX_REQUEST {
        read_nofollow(&path)
    } else {
        None
    };
    let _ = std::fs::remove_file(&path);
    let mut request: Request = serde_json::from_slice(&read?).unwrap_or_default();
    request.to = None;
    Some(request)
}

#[cfg(unix)]
fn read_nofollow(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let mut out = Vec::new();
    file.take(MAX_REQUEST).read_to_end(&mut out).ok()?;
    Some(out)
}

#[cfg(not(unix))]
fn read_nofollow(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut out = Vec::new();
    file.take(MAX_REQUEST).read_to_end(&mut out).ok()?;
    Some(out)
}

/// What the gateway has already told the owner (it can't write the state).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Told {
    /// The last event id told.
    pub id: u64,
    /// The last release offered ("vX is out" or the install question).
    pub offered: Option<String>,
    /// Unix seconds of the gateway's own last release check.
    pub checked: Option<u64>,
    /// The last version a failure was told for.
    pub failed: Option<String>,
    /// The last claude update failure told, until one works.
    pub claude_failed: Option<String>,
}

impl Told {
    /// `None` before the gateway has told anything.
    pub fn load(data: &Path) -> Option<Self> {
        let bytes = std::fs::read(data.join("update").join(TOLD_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self, data: &Path) -> Result<()> {
        let path = data.join("update").join(TOLD_FILE);
        ferrule_tools::fs_tools::write_atomic(&path, &serde_json::to_vec(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// One apply process at a time: the lock file holds its pid, and a lock
/// whose pid is gone is taken over.
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    pub fn take(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(LOCK_FILE);
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    write!(f, "{}", std::process::id())?;
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok());
                    let alive = holder.and_then(crate::health::pid_alive);
                    if alive == Some(true) {
                        bail!(
                            "another update is running (pid {})",
                            holder.unwrap_or_default()
                        );
                    }
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        bail!("couldn't take {}", path.display())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
