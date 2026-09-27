//! The privileged half (docs/m36-self-update.md §3.2–§3.4): check, verify,
//! wait until the gateway is idle, swap, restart, watch the new gateway
//! come up, and roll back and pin the version if it doesn't.

use super::release::{self, Checked, Release, Source};
use super::state::{EventKind, Lock, State};
use super::{swap, Channel};
use anyhow::{bail, Result};
use ferrule_gateway::health::{RunningMarker, MARKER_STALE, RUNNING_FILE};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// The service the new binary has to come up in.
pub trait Service: Send + Sync {
    fn restart(&self) -> Result<()>;
}

/// `systemctl restart ferrule` / `launchctl kickstart`.
pub struct Installed;

impl Service for Installed {
    fn restart(&self) -> Result<()> {
        crate::service::restart()
    }
}

/// Everything the flow needs, so tests can point it at a mock and a temp
/// dir, and shorten the waits.
pub struct Apply<'a> {
    pub source: Source,
    pub target: String,
    /// The binary to replace.
    pub exe: PathBuf,
    /// The gateway's data dir (`running.json` is in `gateway/`).
    pub data: PathBuf,
    /// Where the state and the lock live.
    pub state_dir: PathBuf,
    pub channel: Channel,
    pub current: semver::Version,
    /// `None` without a service manager: swap, don't restart.
    pub service: Option<&'a dyn Service>,
    /// Run `<new> --version` before installing.
    pub check_runs: bool,
    pub idle_for: Duration,
    pub health_for: Duration,
    /// How long the new gateway must stay up to count as healthy.
    pub healthy_after: Duration,
    pub poll: Duration,
}

/// What to install.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Want {
    /// A tag asked for by name (`ferrule update --to`): any version,
    /// pinned or not.
    pub to: Option<String>,
    /// `--unsigned`: a release with no signature at all is taken.
    pub unsigned_ok: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    UpToDate,
    /// The gateway stayed busy for the whole wait; nothing changed.
    Busy,
    Updated {
        from: String,
        to: String,
        /// Swapped without a service to restart: the owner restarts it.
        restart_needed: bool,
    },
    RolledBack {
        from: String,
        to: String,
    },
}

impl Apply<'_> {
    pub fn new_defaults(source: Source, exe: PathBuf, data: PathBuf, state_dir: PathBuf) -> Self {
        Apply {
            source,
            target: release::TARGET.to_string(),
            exe,
            data,
            state_dir,
            channel: Channel::Stable,
            current: release::current(),
            service: None,
            check_runs: true,
            idle_for: Duration::from_secs(6 * 3600),
            health_for: Duration::from_secs(180),
            healthy_after: Duration::from_secs(30),
            poll: Duration::from_secs(5),
        }
    }

    /// The newest release this would install, recording the check. `None`:
    /// up to date.
    pub async fn check(&self, want: &Want) -> Result<Option<Release>> {
        let mut state = State::load(&self.state_dir);
        let found = self.find(&mut state, want).await;
        state.last_check = Some(super::state::now());
        state.last_check_ok = Some(found.is_ok());
        state.last_error = found.as_ref().err().map(|e| format!("{e:#}"));
        // `ferrule update --check` as a user who can't write the system
        // state still gets its answer.
        if let Err(e) = state.save(&self.state_dir) {
            tracing::debug!("update state not saved: {e:#}");
        }
        found
    }

    async fn find(&self, state: &mut State, want: &Want) -> Result<Option<Release>> {
        if let Some(tag) = &want.to {
            let release = self.source.tag(tag).await?;
            return Ok((release.version != self.current).then_some(release));
        }
        let releases = self.source.list().await?;
        let archive = release::archive_name(&self.target);
        let signed = format!("{archive}.minisig");
        if releases.iter().any(|r| r.asset(&signed).is_some()) {
            state.signed_seen = true;
        }
        state.latest = releases
            .iter()
            .filter(|r| self.channel == Channel::Prerelease || !r.prerelease)
            .max_by(|a, b| a.version.cmp(&b.version))
            .map(|r| r.tag.clone());
        Ok(release::choose(
            &releases,
            &self.current,
            self.channel,
            &state.pinned,
            &self.target,
        )
        .cloned())
    }

    /// The whole flow, under the apply lock.
    pub async fn run(&self, want: &Want) -> Result<Outcome> {
        let _lock = Lock::take(&self.state_dir)?;
        let Some(release) = self.check(want).await? else {
            return Ok(Outcome::UpToDate);
        };
        let from = self.current.to_string();
        let to = release.version.to_string();
        let checked = match self.verify(&release, want).await {
            Ok(c) => c,
            Err(e) => {
                self.record_failure(&from, &to, &format!("{e:#}"));
                return Err(e);
            }
        };
        if !self.wait_idle().await {
            return Ok(Outcome::Busy);
        }
        if let Err(e) = swap::swap(&self.exe, &checked.binary) {
            self.record_failure(&from, &to, &format!("{e:#}"));
            return Err(e);
        }
        let mut state = State::load(&self.state_dir);
        if want.to.is_some() {
            state.unpin(&release.tag);
        }
        let Some(service) = self.service else {
            state.push(EventKind::Updated, &from, &to, &release.headline());
            state.save(&self.state_dir)?;
            return Ok(Outcome::Updated {
                from,
                to,
                restart_needed: true,
            });
        };
        let restarted = service.restart();
        if restarted.is_ok() && self.wait_healthy(&release.version).await {
            state.push(EventKind::Updated, &from, &to, &release.headline());
            state.save(&self.state_dir)?;
            return Ok(Outcome::Updated {
                from,
                to,
                restart_needed: false,
            });
        }
        swap::roll_back(&self.exe)?;
        let back = service.restart();
        state.pin(&release.tag);
        let why = match (&restarted, &back) {
            (Err(e), _) => format!("the restart failed: {e:#}"),
            (_, Err(e)) => format!("it didn't come up, and restarting {from} failed: {e:#}"),
            _ => format!(
                "it didn't report healthy within {} s",
                self.health_for.as_secs()
            ),
        };
        state.push(EventKind::RolledBack, &from, &to, &why);
        state.save(&self.state_dir)?;
        Ok(Outcome::RolledBack { from, to })
    }

    pub async fn verify(&self, release: &Release, want: &Want) -> Result<Checked> {
        let checked = release::fetch(
            &self.source,
            release,
            &self.target,
            want.unsigned_ok && want.to.is_some(),
            self.exe.parent(),
        )
        .await?;
        match &checked.signed_by {
            Some(key) => tracing::info!(tag = %release.tag, key = %key, "update verified"),
            None => tracing::warn!(tag = %release.tag, "update unsigned, checksum only"),
        }
        if self.check_runs {
            let (binary, version) = (checked.binary.clone(), release.version.clone());
            tokio::task::spawn_blocking(move || release::runs(&binary, &version)).await??;
        }
        Ok(checked)
    }

    /// One `failed` event per version, not one a day.
    fn record_failure(&self, from: &str, to: &str, why: &str) {
        let mut state = State::load(&self.state_dir);
        let again = state
            .events
            .last()
            .is_some_and(|e| e.kind == EventKind::Failed && e.to == to && e.notes == why);
        if !again {
            state.push(EventKind::Failed, from, to, why);
            let _ = state.save(&self.state_dir);
        }
    }

    /// Wait until no turn is running (§3.2 4); false when the wait ran out.
    async fn wait_idle(&self) -> bool {
        let deadline = Instant::now() + self.idle_for;
        loop {
            if idle(&self.data) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// The new gateway's marker names `version`, its pid is alive and it
    /// has been up `healthy_after` (§3.4).
    async fn wait_healthy(&self, version: &semver::Version) -> bool {
        let deadline = Instant::now() + self.health_for;
        loop {
            if let Some(marker) = fresh_marker(&self.data) {
                let up = super::state::now().saturating_sub(marker.started);
                if marker.version == version.to_string()
                    && crate::health::pid_alive(marker.pid) != Some(false)
                    && up >= self.healthy_after.as_secs()
                {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(self.poll).await;
        }
    }
}

/// No gateway, or one with no turn in progress.
pub fn idle(data: &Path) -> bool {
    fresh_marker(data).is_none_or(|m| m.turns.is_empty())
}

/// `running.json`, if it was rewritten in the last 30 seconds.
pub fn fresh_marker(data: &Path) -> Option<RunningMarker> {
    let path = data.join("gateway").join(RUNNING_FILE);
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .unwrap_or_default();
    if age >= MARKER_STALE {
        return None;
    }
    serde_json::from_slice(&std::fs::read(&path).ok()?).ok()
}

/// Refuse to touch a binary this process can't replace.
pub fn writable(exe: &Path) -> Result<()> {
    let dir = exe.parent().unwrap_or(Path::new("."));
    let probe = dir.join(format!(".ferrule-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(_) => bail!("{} isn't writable by you", dir.display()),
    }
}
