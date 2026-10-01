//! M36: `ferrule update` (docs/m36-self-update.md, docs/updates.md). A
//! signed release from GitHub, checked, installed when the gateway is idle,
//! and rolled back and pinned if it doesn't come up.

pub mod apply;
pub mod claude;
pub mod notice;
pub mod release;
pub mod state;
pub mod swap;

#[cfg(test)]
pub(crate) mod tests;

use crate::service;
use anyhow::{anyhow, bail, Context, Result};
use apply::{Apply, Installed, Outcome, Service, Want};
use release::Source;
use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Which releases count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Stable,
    /// Release candidates too.
    Prerelease,
}

/// `ferrule update` in a terminal waits this long for a running turn.
const FOREGROUND_IDLE: Duration = Duration::from_secs(600);
/// A timer run with no request skips the check if one ran this recently
/// (launchd also starts the job when the gateway's request goes away).
const TIMER_GAP: u64 = 12 * 3600;

/// Where the state and lock live: root's `/var/lib/ferrule/update` for
/// the system service (its user reads, can't write), else `<data>/update`.
/// A named system instance's is `/var/lib/ferrule-<name>/update` (M38).
pub fn state_dir(data: &Path) -> PathBuf {
    let system_home = data
        .parent()
        .filter(|_| data.file_name().is_some_and(|n| n == "data"))
        .filter(|home| {
            home.parent() == Some(Path::new("/var/lib"))
                && home.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n == "ferrule"
                        || n.strip_prefix("ferrule-")
                            .is_some_and(|name| crate::instance::validate(name).is_ok())
                })
        });
    match system_home {
        Some(home) => home.join("update"),
        None => data.join("update"),
    }
}

/// The apply units are there (setup installed them).
pub fn units_installed() -> bool {
    service::update_units_installed()
}

/// `ferrule update`'s flags.
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub check: bool,
    pub to: Option<String>,
    pub unsigned: bool,
    pub yes: bool,
    pub apply: bool,
}

pub async fn cli(args: Args) -> Result<()> {
    // An unreadable config mustn't stop the fix for it from installing.
    let cfg = match crate::config::Config::load() {
        Ok((cfg, _)) => Some(cfg),
        Err(e) => {
            eprintln!("ferrule: the config didn't load ({e:#}); using the update defaults");
            None
        }
    };
    let settings = cfg.as_ref().map(|c| c.update.clone()).unwrap_or_default();
    let data = crate::config::data_dir_path().context("no data dir; set FERRULE_DATA_DIR")?;
    let exe = dunce::canonicalize(std::env::current_exe()?)?;
    swap::clean_old(&exe);
    if args.apply {
        let claude = cfg
            .as_ref()
            .and_then(|c| claude::Claude::from_config(c, &data));
        return apply_unit(&settings, &data, &exe, claude.as_ref()).await;
    }
    if args.unsigned && args.to.is_none() {
        bail!("--unsigned is for an old release asked for by name: ferrule update --to <tag> --unsigned");
    }
    let mut apply = Apply::new_defaults(
        Source::github(),
        exe.clone(),
        data.clone(),
        state_dir(&data),
    );
    apply.channel = settings.channel;
    apply.idle_for = FOREGROUND_IDLE;
    let want = Want {
        to: args.to.clone(),
        unsigned_ok: args.unsigned,
    };
    let current = release::current();
    let Some(found) = apply.check(&want).await? else {
        match &args.to {
            Some(tag) => println!("Ferrule {tag} is the version running."),
            None => println!("Ferrule {current} is the newest release."),
        }
        return Ok(());
    };
    let headline = found.headline();
    println!(
        "Ferrule {} is out (this is {current}){}",
        found.tag,
        if headline.is_empty() {
            ".".to_string()
        } else {
            format!(": {headline}")
        }
    );
    if args.check {
        return Ok(());
    }
    if let Some(why) = someone_elses(&exe) {
        bail!(why);
    }
    apply::writable(&exe).map_err(|e| {
        anyhow!(
            "{e:#}; update it as its owner: sudo {} update",
            exe.display()
        )
    })?;
    if found.version < current || args.unsigned {
        let question = if args.unsigned {
            format!(
                "{} may predate signed releases: without a signature only its checksum is \
                 checked. Install it?",
                found.tag
            )
        } else {
            format!("{} is older than {current}. Install it anyway?", found.tag)
        };
        confirm(&question, args.yes)?;
    }
    let service_exe = matches!(service::status(), service::Status::Installed { .. })
        .then(service::installed_exe)
        .flatten()
        .and_then(|e| dunce::canonicalize(e).ok());
    let restarts = service_exe.as_deref() == Some(exe.as_path());
    if let Some(other) = service_exe.as_ref().filter(|_| !restarts) {
        println!(
            "The service runs {}, not this binary; only this one is updated.",
            other.display()
        );
    }
    apply.service = restarts.then_some(&Installed as &dyn Service);
    // M38 §4: the other instances on this binary restart with it.
    let others = others_running(&exe);
    if !others.is_empty() {
        let names: Vec<&str> = others.iter().map(|o| o.name.as_str()).collect();
        confirm(
            &format!(
                "This binary also runs the instance{} {}: {} restart{} with it. Go on?",
                if names.len() == 1 { "" } else { "s" },
                names.join(", "),
                if names.len() == 1 { "it" } else { "they" },
                if names.len() == 1 { "s" } else { "" },
            ),
            args.yes,
        )?;
    }
    apply.siblings = siblings(&others);
    println!("Checking and installing {}…", found.tag);
    match apply.run(&want).await? {
        Outcome::UpToDate => println!("Nothing to install."),
        Outcome::Busy => println!(
            "The gateway was busy for {} minutes, so nothing changed. Try again later{}.",
            FOREGROUND_IDLE.as_secs() / 60,
            if units_installed() {
                ", or let the daily update install it"
            } else {
                ""
            }
        ),
        Outcome::Updated {
            from,
            to,
            restart_needed,
        } => {
            println!("Updated Ferrule {from} → {to}.");
            if restart_needed && service_exe.is_some() {
                println!("Restart the service to run it: {}", service::restart_hint());
            } else if restart_needed {
                println!("A ferrule that is running keeps the old version until it restarts.");
            }
        }
        Outcome::RolledBack { from, to } => bail!(
            "Ferrule {to} didn't start properly, so {from} is back and {to} is pinned. \
             `ferrule update --to v{to}` retries it; the service's log says why: {}",
            service::logs_hint()
        ),
    }
    Ok(())
}

/// `update --apply`, from the units: a request from the gateway, or the
/// daily timer.
async fn apply_unit(
    settings: &crate::config::UpdateConfig,
    data: &Path,
    exe: &Path,
    claude: Option<&claude::Claude>,
) -> Result<()> {
    let state_dir = state_dir(data);
    let request = state::take_request(data);
    let asked = request.as_ref().is_some_and(|r| r.claude);
    let due = match &request {
        Some(r) => r.claude,
        None => !state::State::load(&state_dir)
            .claude_checked
            .is_some_and(|at| state::now().saturating_sub(at) < TIMER_GAP),
    };
    match claude.filter(|_| due) {
        Some(c) => {
            let why = asked.then_some("after a turn failed");
            match claude::run(&state_dir, c, why).await {
                Ok(Some(u)) => println!("claude updated {} → {}", u.from, u.to),
                Ok(None) => println!("claude is current"),
                Err(e) => println!("claude: {e:#}"),
            }
        }
        // The gateway waits for an answer.
        None if asked => {
            let mut st = state::State::load(&state_dir);
            st.claude_checked = Some(state::now());
            st.push(
                state::EventKind::ClaudeFailed,
                "",
                "",
                "updating claude is off here: [update] claude = false, or no Claude plan",
            );
            st.save(&state_dir)?;
        }
        None => {}
    }
    if let Some(id) = request.as_ref().filter(|r| r.claude).map(|r| r.id) {
        let mut st = state::State::load(&state_dir);
        st.claude_answered = Some(id);
        st.save(&state_dir)?;
    }
    let mut apply = Apply::new_defaults(
        Source::github(),
        exe.to_path_buf(),
        data.into(),
        state_dir.clone(),
    );
    apply.channel = settings.channel;
    let installed = matches!(service::status(), service::Status::Installed { .. });
    apply.service = installed.then_some(&Installed as &dyn Service);
    let others = others_running(exe);
    apply.siblings = siblings(&others);
    let autos: Vec<(String, Option<bool>)> =
        others.iter().map(|o| (o.name.clone(), o.auto)).collect();
    let held = held_by(&autos);
    let ferrule = match &request {
        // The owner pressed "update" on one instance: it restarts the
        // others, so one that said auto = false holds it.
        Some(r) if r.ferrule && held.is_some() => {
            let who = held.unwrap_or_default();
            if let Some(found) = apply.check(&Want::default()).await? {
                apply::record_failure(
                    &state_dir,
                    &apply.current.to_string(),
                    &found.version.to_string(),
                    &format!(
                        "the instance `{who}` runs this binary too and has [update] auto = false; \
                         `ferrule update` in a terminal installs it for both, after asking"
                    ),
                );
            }
            println!("not installed: the instance `{who}` has [update] auto = false");
            false
        }
        Some(r) => r.ferrule,
        None => {
            let recent = state::State::load(&state_dir)
                .last_check
                .is_some_and(|at| state::now().saturating_sub(at) < TIMER_GAP);
            if recent {
                return Ok(());
            }
            // The units exist, so unset means on.
            if settings.auto == Some(false) {
                apply.check(&Want::default()).await?;
                false
            } else if let Some(who) = held {
                apply.check(&Want::default()).await?;
                println!("only checked: the instance `{who}` runs this binary too and has [update] auto = false");
                false
            } else {
                true
            }
        }
    };
    if ferrule {
        match apply.run(&Want::default()).await? {
            Outcome::UpToDate => println!("up to date ({})", apply.current),
            Outcome::Busy => println!("the gateway stayed busy; trying again tomorrow"),
            Outcome::Updated { from, to, .. } => println!("updated {from} → {to}"),
            Outcome::RolledBack { from, to } => {
                println!("{to} didn't come up; rolled back to {from} and pinned {to}")
            }
        }
    }
    Ok(())
}

/// Another instance whose service runs this binary (M38 §4).
pub struct Other {
    pub name: String,
    pub svc: service::Svc,
    pub data: PathBuf,
    /// Its `[update] auto`; unreadable reads as unset.
    pub auto: Option<bool>,
}

impl apply::Service for service::Svc {
    fn restart(&self) -> Result<()> {
        service::Svc::restart(self)
    }
}

/// The instances in this scope, this one aside, whose installed service
/// runs `exe`.
pub fn others_running(exe: &Path) -> Vec<Other> {
    let Ok(places) = crate::instances::Places::here() else {
        return Vec::new();
    };
    let me = crate::instance::current();
    let scope = service::scope();
    crate::instances::discover(&places)
        .into_iter()
        .filter(|f| f.scope == scope && f.name != me)
        .filter(|f| {
            f.unit
                .as_deref()
                .and_then(service::installed_exe_at)
                .and_then(|e| dunce::canonicalize(e).ok())
                .as_deref()
                == Some(exe)
        })
        .map(|f| Other {
            name: f.label().to_string(),
            svc: f.svc(),
            auto: crate::config::Config::from_file(&f.config)
                .ok()
                .and_then(|c| c.update.auto),
            data: f.data,
        })
        .collect()
}

/// The siblings as the apply flow takes them.
fn siblings(others: &[Other]) -> Vec<apply::Sibling<'_>> {
    others
        .iter()
        .map(|o| apply::Sibling {
            name: o.name.clone(),
            state_dir: state_dir(&o.data),
            data: o.data.clone(),
            service: &o.svc,
        })
        .collect()
}

/// Who says a run from the units mustn't install (§4 4): the first sibling
/// with `[update] auto = false`.
pub fn held_by(others: &[(String, Option<bool>)]) -> Option<&str> {
    others
        .iter()
        .find(|(_, auto)| *auto == Some(false))
        .map(|(name, _)| name.as_str())
}

/// Not this user's to replace: a system service's binary, run by root's
/// units — the default's or any named instance's (M38).
pub(crate) fn someone_elses(exe: &Path) -> Option<String> {
    if service::is_root() || !cfg!(target_os = "linux") {
        return None;
    }
    let runs_it = |svc: service::Svc| {
        let system = service::installed_exe_at(&svc.system_unit_path())?;
        (dunce::canonicalize(system).ok()? == exe).then_some(())
    };
    let named = service::unit_names(service::Scope::System);
    std::iter::once(None)
        .chain(named.iter().map(|n| Some(n.as_str())))
        .find_map(|n| runs_it(service::Svc::new(n, service::Scope::System)))
        .map(|()| {
            "a system service runs this binary; update it as root: sudo ferrule update".into()
        })
}

fn confirm(question: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("{question} Answer with --yes when there's no terminal to ask in.");
    }
    let ok = inquire::Confirm::new(question)
        .with_default(false)
        .prompt()
        .unwrap_or(false);
    if !ok {
        bail!("nothing changed");
    }
    Ok(())
}

/// How a report line reads in `ferrule doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Note,
    Warn,
}

/// The update state in words (status, `/status`, doctor, dashboard): how
/// updates happen here, the last check, the last update and what's pinned.
pub fn report(data: &Path, auto: Option<bool>, units: bool) -> Vec<(Tone, String)> {
    let state = state::State::load(&state_dir(data));
    let ago = |at: u64| {
        ferrule_gateway::health::human(Duration::from_secs(state::now().saturating_sub(at)))
    };
    let mut lines = Vec::new();
    let current = release::current();
    lines.push(match (units, auto) {
        (true, Some(false)) => (
            Tone::Ok,
            format!("{current}; checked daily, installed when you say so"),
        ),
        (true, _) => (
            Tone::Ok,
            format!("{current}; checked daily, installed by itself when idle"),
        ),
        (false, _) if matches!(service::status(), service::Status::Installed { .. }) => (
            Tone::Warn,
            format!(
                "{current}; the service has no update units: `ferrule setup --refresh-service` \
                 adds them (from 0.5.x, run the install one-liner once first)"
            ),
        ),
        (false, _) => (
            Tone::Note,
            format!("{current}; you're told when a release is out, /update installs it"),
        ),
    });
    match (state.last_check, state.last_check_ok) {
        (Some(at), Some(false)) => lines.push((
            Tone::Warn,
            format!(
                "the last check, {} ago, failed: {}",
                ago(at),
                release::clip(state.last_error.as_deref().unwrap_or("unknown"), 200)
            ),
        )),
        (Some(at), _) => {
            let newer = state
                .latest
                .as_deref()
                .filter(|t| release::parse_version(t).is_some_and(|v| v > current));
            let text = match newer {
                Some(tag) if state.is_pinned(tag) => {
                    format!("last checked {} ago; {tag} is out but pinned", ago(at))
                }
                Some(tag) => format!("last checked {} ago; {tag} is out", ago(at)),
                None => format!("last checked {} ago; up to date", ago(at)),
            };
            lines.push((Tone::Ok, text));
        }
        _ => {}
    }
    if let Some(e) = state.events.iter().rev().find(|e| {
        matches!(
            e.kind,
            state::EventKind::Updated | state::EventKind::RolledBack | state::EventKind::Failed
        )
    }) {
        lines.push(match e.kind {
            state::EventKind::Updated => (
                Tone::Ok,
                format!("updated {} → {}, {} ago", e.from, e.to, ago(e.at)),
            ),
            state::EventKind::RolledBack => (
                Tone::Warn,
                format!(
                    "{} didn't start properly {} ago and was rolled back ({}); \
                     the next release is offered as usual",
                    e.to,
                    ago(e.at),
                    release::clip(&e.notes, 160),
                ),
            ),
            _ => (
                Tone::Warn,
                format!(
                    "{} wasn't installed, {} ago: {}",
                    e.to,
                    ago(e.at),
                    release::clip(&e.notes, 200)
                ),
            ),
        });
    }
    if let Some(at) = state.claude_checked {
        let installed = state.claude_installed.as_deref().unwrap_or("?");
        let failed = state
            .events
            .iter()
            .rev()
            .find(|e| {
                matches!(
                    e.kind,
                    state::EventKind::ClaudeUpdated | state::EventKind::ClaudeFailed
                )
            })
            .filter(|e| e.kind == state::EventKind::ClaudeFailed);
        lines.push(match (failed, state.claude_latest.as_deref()) {
            (Some(e), _) => (
                Tone::Warn,
                format!(
                    "claude {installed}: its update failed {} ago: {}",
                    ago(e.at),
                    release::clip(&e.notes, 200)
                ),
            ),
            (None, Some(l)) if ferrule_plans::claude::update::is_newer(l, installed) => (
                Tone::Note,
                format!("claude {installed}, {l} is out; checked {} ago", ago(at)),
            ),
            _ => (
                Tone::Ok,
                format!("claude {installed}, up to date; checked {} ago", ago(at)),
            ),
        });
    }
    let pinned: Vec<&str> = state
        .pinned
        .iter()
        .map(String::as_str)
        .filter(|t| {
            state
                .last_update()
                .is_none_or(|e| !release::same_tag(&e.to, t))
        })
        .collect();
    if !pinned.is_empty() {
        lines.push((
            Tone::Note,
            format!("never installed by itself: {}", pinned.join(", ")),
        ));
    }
    lines
}

/// The report as plain lines.
pub fn status_lines(data: &Path, auto: Option<bool>) -> Vec<String> {
    report(data, auto, units_installed())
        .into_iter()
        .map(|(_, line)| line)
        .collect()
}
