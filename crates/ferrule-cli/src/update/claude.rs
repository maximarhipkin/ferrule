//! M36 §5 in the binary: the `claude` the Claude plan runs, kept current
//! daily by the apply unit (or the gateway, without the units), and at once
//! when a turn failed because it was too old or broken. The update itself
//! is claude's own updater or its package manager's
//! ([`ferrule_plans::claude::update`]).

use super::state::{self, EventKind, Request, State};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use ferrule_plans::claude::cli;
use ferrule_plans::claude::update::{self, Install, Updated};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// After a failed turn, the gateway waits this long for the apply unit.
const WAIT: Duration = Duration::from_secs(300);
/// A repair after a failed turn runs at most this often.
const REPAIR_GAP: Duration = Duration::from_secs(1800);

/// The `claude` to keep current and the engine's config dir.
#[derive(Debug, Clone)]
pub struct Claude {
    pub binary: PathBuf,
    pub config_dir: PathBuf,
    /// npm's dist-tags for the package.
    pub npm: String,
}

impl Claude {
    /// The configured one, when a provider runs on the Claude plan and
    /// `[update] claude` is on.
    pub fn from_config(cfg: &crate::config::Config, data: &Path) -> Option<Claude> {
        (cfg.update.claude && cfg.uses_claude_code()).then(|| Claude {
            binary: cfg.plans.claude_code.binary(),
            config_dir: cfg.plans.claude_code.config_dir(data),
            npm: update::NPM_URL.to_string(),
        })
    }
}

fn client() -> reqwest::Client {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .user_agent(concat!("ferrule/", env!("CARGO_PKG_VERSION"), " (updater)"));
    #[cfg(test)]
    let http = http.no_proxy();
    http.build().expect("static client config")
}

/// Check, and update when npm has a newer one (or `why` says a turn needs
/// it), recording what happened in the update state. `Ok(None)`: nothing to
/// do.
pub async fn run(state_dir: &Path, claude: &Claude, why: Option<&str>) -> Result<Option<Updated>> {
    let binary = claude.binary.clone();
    let install = tokio::task::spawn_blocking(move || update::detect(&binary)).await??;
    let (found, config_dir) = (install.found.clone(), claude.config_dir.clone());
    let installed = tokio::task::spawn_blocking(move || cli::version(&found, &config_dir)).await?;
    let latest = update::latest(&client(), &claude.npm).await;
    let mut st = State::load(state_dir);
    st.claude_checked = Some(state::now());
    if let Ok(v) = &installed {
        st.claude_installed = Some(v.clone());
    }
    if let Ok(v) = &latest {
        st.claude_latest = Some(v.clone());
    }
    let behind = match (&installed, &latest) {
        (Ok(have), Ok(new)) => update::is_newer(new, have),
        _ => false,
    };
    if why.is_none() && !behind {
        save(&st, state_dir);
        if let Err(e) = &latest {
            tracing::debug!("claude's latest version: {e:#}");
        }
        return installed.map(|_| None);
    }
    let (i, config_dir) = (install.clone(), claude.config_dir.clone());
    let done =
        tokio::task::spawn_blocking(move || update::update(&i, &config_dir, update::LIMIT)).await?;
    let from = installed.as_deref().unwrap_or("?").to_string();
    let result = match done {
        Ok(u) if u.to == u.from && latest.as_ref().is_ok_and(|l| update::is_newer(l, &u.to)) => {
            Err(anyhow!(
                "the update ran, but claude is still {} ({} is out)",
                u.to,
                latest.as_deref().unwrap_or("?")
            ))
        }
        other => other,
    };
    // Reloaded: the update can take minutes, and the ferrule side may
    // have written meanwhile.
    let mut st = State {
        claude_checked: st.claude_checked,
        claude_installed: st.claude_installed,
        claude_latest: st.claude_latest,
        ..State::load(state_dir)
    };
    match &result {
        Ok(u) => {
            st.claude_installed = Some(u.to.clone());
            if u.to != u.from {
                st.push(
                    EventKind::ClaudeUpdated,
                    &u.from,
                    &u.to,
                    why.map_or("", |_| "after a turn failed"),
                );
            }
        }
        Err(e) => {
            let notes = format!("{e:#}");
            let to = latest.as_deref().unwrap_or("").to_string();
            let again = st
                .events
                .iter()
                .rev()
                .find(|e| matches!(e.kind, EventKind::ClaudeUpdated | EventKind::ClaudeFailed))
                .is_some_and(|e| e.kind == EventKind::ClaudeFailed && e.notes == notes);
            if !again {
                st.push(EventKind::ClaudeFailed, &from, &to, &notes);
            }
        }
    }
    save(&st, state_dir);
    result.map(Some)
}

/// The gateway can't write the system service's state; the apply unit
/// records it there instead.
fn save(st: &State, dir: &Path) {
    if let Err(e) = st.save(dir) {
        tracing::debug!("update state not saved: {e:#}");
    }
}

/// Whether this process can run the update itself.
pub fn can_run_here(install: &Install) -> bool {
    install.method.argv(&install.found).is_some() && update::blocked(install).is_none()
}

/// The engine's repairer in the gateway: the update right here when this
/// user owns claude's files, else through the apply unit's request file.
#[derive(Debug)]
pub struct Fixer {
    pub data: PathBuf,
    pub claude: Claude,
}

impl Fixer {
    pub fn new(data: PathBuf, claude: Claude) -> Self {
        Fixer { data, claude }
    }
}

/// The last repair in this process: every provider has its own engine.
static LAST_REPAIR: Mutex<Option<Instant>> = Mutex::new(None);

#[async_trait]
impl ferrule_plans::claude::Repairer for Fixer {
    async fn update_claude(&self, why: &str) -> Result<()> {
        {
            let mut last = LAST_REPAIR.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|at| at.elapsed() < REPAIR_GAP) {
                bail!("claude was updated less than 30 minutes ago");
            }
            *last = Some(Instant::now());
        }
        tracing::info!(why = %super::release::clip(why, 200), "claude looks outdated; updating it");
        let state_dir = super::state_dir(&self.data);
        let binary = self.claude.binary.clone();
        let install = tokio::task::spawn_blocking(move || update::detect(&binary)).await??;
        if can_run_here(&install) {
            return run(&state_dir, &self.claude, Some(why)).await.map(|_| ());
        }
        if !super::units_installed() {
            bail!(
                "claude is a {} ferrule can't update from here: {}",
                install.method.name(),
                install.method.manual(&install.found)
            );
        }
        ask_unit(&self.data, &state_dir, WAIT, Duration::from_secs(2)).await
    }
}

/// Write the request, then wait for the apply unit's claude run.
pub async fn ask_unit(data: &Path, state_dir: &Path, wait: Duration, poll: Duration) -> Result<()> {
    let seen = State::load(state_dir).events.last().map_or(0, |e| e.id);
    let id = u64::from(uuid::Uuid::new_v4().as_u128() as u32) + 1;
    state::write_request(
        data,
        &Request {
            claude: true,
            id,
            ..Request::default()
        },
    )?;
    let deadline = Instant::now() + wait;
    loop {
        tokio::time::sleep(poll).await;
        let st = State::load(state_dir);
        if st.claude_answered == Some(id) {
            // A state that started over has ids from 1 again.
            let seen = if st.events.last().is_some_and(|e| e.id < seen) {
                0
            } else {
                seen
            };
            let failed = st
                .events
                .iter()
                .filter(|e| e.id > seen)
                .rfind(|e| matches!(e.kind, EventKind::ClaudeUpdated | EventKind::ClaudeFailed))
                .filter(|e| e.kind == EventKind::ClaudeFailed);
            return match failed {
                Some(e) => Err(anyhow!("{}", e.notes)),
                None => Ok(()),
            };
        }
        if Instant::now() >= deadline {
            bail!(
                "the update unit didn't run within {} min",
                wait.as_secs() / 60
            );
        }
    }
}

/// Doctor's line: the installed and latest versions, and how it updates.
pub async fn doctor_line(claude: &Claude, offline: bool) -> (bool, String) {
    let binary = claude.binary.clone();
    let config_dir = claude.config_dir.clone();
    let found = tokio::task::spawn_blocking(move || {
        let install = update::detect(&binary)?;
        let version = cli::version(&install.found, &config_dir)?;
        Ok::<_, anyhow::Error>((install, version))
    })
    .await;
    let (install, installed) = match found {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => return (false, format!("claude: {e:#}")),
        Err(e) => return (false, format!("claude: {e}")),
    };
    let latest = if offline {
        None
    } else {
        update::latest(&client(), &claude.npm).await.ok()
    };
    let behind = latest
        .as_deref()
        .is_some_and(|l| update::is_newer(l, &installed));
    let how = match install.method.argv(&install.found) {
        Some(_) if can_run_here(&install) || super::units_installed() => {
            "updated by itself daily".to_string()
        }
        Some(_) => format!(
            "can't be updated by this user: {}",
            install.method.manual(&install.found)
        ),
        None => install.method.manual(&install.found),
    };
    let latest = match latest {
        Some(l) if behind => format!(", {l} is out"),
        Some(_) => ", the latest".to_string(),
        None => String::new(),
    };
    (
        !behind,
        format!(
            "claude {installed}{latest} ({}); {how}",
            install.method.name()
        ),
    )
}
