//! M36 §7: the gateway's self-check. Every 15 minutes (the first 2 minutes
//! after start) it collects the problems it can see cheaply and offline —
//! plan sign-ins, `claude`, the update check, the disk, the data dir, the
//! channels — repairs what it can, and tells the owner only what changed:
//! a new problem once, "fixed" once when it clears, nothing while the set
//! stays the same.

use crate::update::claude::{Claude, Fixer};
use crate::update::notice::Owner;
use crate::update::state::{self, State};
use ferrule_gateway::Channel;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub const FILE: &str = "selfcheck.json";
const FIRST: Duration = Duration::from_secs(120);
const EVERY: Duration = Duration::from_secs(15 * 60);
/// Under this much free space on the data dir's disk is a problem.
pub const DISK_FLOOR: u64 = 500 * 1024 * 1024;
/// A channel that hasn't connected for this long is a problem.
const CHANNEL_DOWN: Duration = Duration::from_secs(10 * 60);
/// An update check failing for this long is a problem.
const CHECK_FAILING: u64 = 3 * 24 * 3600;

/// `key → line`: `disk`, `data`, `claude`, `updates`, `pinned`,
/// `signin:<plan>`, `channel:<name>`.
pub type Problems = BTreeMap<String, String>;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    problems: Problems,
    at: u64,
}

pub struct Check {
    pub data: PathBuf,
    pub claude: Option<Claude>,
    pub plans: Vec<crate::config::Plan>,
    pub channels: Vec<Arc<dyn Channel>>,
    pub started: SystemTime,
    pub disk_floor: u64,
}

impl Check {
    pub fn new(
        cfg: &crate::config::Config,
        data: PathBuf,
        channels: Vec<Arc<dyn Channel>>,
    ) -> Self {
        let mut plans: Vec<_> = cfg.providers.values().filter_map(|p| p.plan).collect();
        plans.sort_by_key(|p| p.as_str());
        plans.dedup();
        Check {
            claude: Claude::from_config(cfg, &data),
            data,
            plans,
            channels,
            started: SystemTime::now(),
            disk_floor: DISK_FLOOR,
        }
    }

    /// What's wrong now, repairing on the way what can be.
    pub async fn problems(&self) -> Problems {
        let mut found = Problems::new();
        self.disk(&mut found);
        self.updates(&mut found);
        for plan in &self.plans {
            let plan = *plan;
            let state = tokio::task::spawn_blocking(move || crate::subscription::state(plan))
                .await
                .ok();
            use crate::subscription::SignIn;
            if let Some(s @ (SignIn::Expired | SignIn::Out | SignIn::Unreadable(_))) = state {
                found.insert(
                    format!("signin:{}", plan.as_str()),
                    format!(
                        "{plan}: {} — on the server: `ferrule login {}`",
                        s.word(),
                        plan.login_word()
                    ),
                );
            }
        }
        if let Some(claude) = &self.claude {
            self.claude(claude, &mut found).await;
        }
        let now = SystemTime::now();
        for c in self.channels.iter().filter(|c| c.polls()) {
            let since = c.last_ok_poll().unwrap_or(self.started);
            let down = now.duration_since(since).unwrap_or_default();
            if down > CHANNEL_DOWN {
                found.insert(
                    format!("channel:{}", c.name()),
                    format!(
                        "{} hasn't connected for {}",
                        c.name(),
                        ferrule_gateway::health::human(down)
                    ),
                );
            }
        }
        found
    }

    fn disk(&self, found: &mut Problems) {
        let probe = self.data.join(".selfcheck-probe");
        let wrote = std::fs::write(&probe, b"ok");
        let _ = std::fs::remove_file(&probe);
        if let Err(e) = wrote {
            let kind = ferrule_core::failure::classify_io(&e);
            let words = if kind == Some(ferrule_core::failure::Kind::DiskFull) {
                "the disk is full"
            } else {
                "it isn't writable"
            };
            found.insert(
                "data".into(),
                format!(
                    "I can't write to my data dir {} ({words}: {e}); sessions and memory aren't saved",
                    self.data.display()
                ),
            );
        }
        if let Some(free) = free_space(&self.data) {
            if free < self.disk_floor {
                found.insert(
                    "disk".into(),
                    format!(
                        "only {} MB free on the disk with {}",
                        free / (1024 * 1024),
                        self.data.display()
                    ),
                );
            }
        }
    }

    fn updates(&self, found: &mut Problems) {
        let st = State::load(&crate::update::state_dir(&self.data));
        let now = state::now();
        if st.last_check_ok == Some(false) {
            if let Some(ok) = st
                .last_ok
                .filter(|ok| now.saturating_sub(*ok) > CHECK_FAILING)
            {
                found.insert(
                    "updates".into(),
                    format!(
                        "the update check has failed for {} days: {}",
                        now.saturating_sub(ok) / 86_400,
                        st.last_error.as_deref().unwrap_or("no reason recorded")
                    ),
                );
            }
        }
        if let Some(e) = st
            .events
            .iter()
            .rev()
            .find(|e| e.kind == state::EventKind::RolledBack)
            .filter(|e| st.is_pinned(&e.to))
        {
            found.insert(
                "pinned".into(),
                format!(
                    "{} didn't start properly and is pinned, so {} keeps running; `ferrule update --to v{}` retries it",
                    e.to, e.from, e.to
                ),
            );
        }
    }

    async fn claude(&self, claude: &Claude, found: &mut Problems) {
        let (binary, config_dir) = (claude.binary.clone(), claude.config_dir.clone());
        let ran = tokio::task::spawn_blocking(move || {
            let install = ferrule_plans::claude::update::detect(&binary)?;
            ferrule_plans::claude::cli::version(&install.found, &config_dir)
        })
        .await;
        let e = match ran {
            Ok(Ok(_)) => return,
            Ok(Err(e)) => format!("{e:#}"),
            Err(e) => e.to_string(),
        };
        // A claude that's there but doesn't run: its update may mend it,
        // tried once when the problem is new.
        let known = last(&self.data).is_some_and(|(_, p)| p.contains_key("claude"));
        if !known && ferrule_plans::claude::cli::find(&claude.binary).is_some() {
            use ferrule_plans::claude::Repairer;
            let fixer = Fixer::new(self.data.clone(), claude.clone());
            if fixer
                .update_claude(&format!("the self-check found claude not running: {e}"))
                .await
                .is_ok()
            {
                return;
            }
        }
        found.insert(
            "claude".into(),
            format!(
                "the claude CLI doesn't run ({}): {}",
                claude.binary.display(),
                ferrule_gateway::health::clip(&e, 300)
            ),
        );
    }

    /// One round: the problems, compared with the last round's. The owner
    /// hears what's new and what cleared.
    pub async fn tick(&self, owner: &dyn Owner) -> Problems {
        let now = self.problems().await;
        let path = self.data.join(FILE);
        let before: Saved = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let mut lines = Vec::new();
        for (key, line) in &now {
            if !before.problems.contains_key(key) {
                lines.push(format!("• {line}"));
            }
        }
        for (key, line) in &before.problems {
            if !now.contains_key(key) {
                lines.push(format!("• fixed: {line}"));
            }
        }
        if !lines.is_empty() {
            owner.tell(format!("Self-check:\n{}", lines.join("\n")));
        }
        let saved = Saved {
            problems: now.clone(),
            at: state::now(),
        };
        if let Ok(text) = serde_json::to_string_pretty(&saved) {
            // A full disk is one of the problems; not being able to note it
            // only means the next round tells it again.
            let _ = std::fs::write(&path, text);
        }
        now
    }
}

/// The last round's problems, for doctor and `/status`.
pub fn last(data: &Path) -> Option<(u64, Problems)> {
    let text = std::fs::read_to_string(data.join(FILE)).ok()?;
    let saved: Saved = serde_json::from_str(&text).ok()?;
    Some((saved.at, saved.problems))
}

/// Runs the check in the gateway until it exits.
pub fn spawn(check: Check, owner: Arc<dyn Owner>) {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST).await;
        loop {
            check.tick(owner.as_ref()).await;
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// Free bytes for this user on the disk holding `path`.
#[cfg(unix)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C string and a zeroed struct statvfs fills in.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some(s.f_bavail as u64 * s.f_frsize as u64)
}

#[cfg(windows)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    // SAFETY: a NUL-terminated path and a u64 out-parameter.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(test)]
mod tests;
