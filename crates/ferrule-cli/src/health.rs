//! M19b: the CLI's side of the gateway's health (docs/m19b-reliability.md)
//! — the `tracing` layer behind `/status`'s recent warnings, the redactor
//! built from the config, the report's spend and schedule sections, and
//! `ferrule status`.

use crate::config::{self, Config};
use anyhow::Result;
use ferrule_gateway::health::{human, restart_notice, stamp, STATUS_FILE};
use ferrule_gateway::{
    Health, HealthSettings, Heartbeat, Leftover, Notice, RecentLog, Redactor, TaskStore,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// Feeds warnings and errors to a [`RecentLog`].
pub struct RingLayer(pub Arc<RecentLog>);

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = *event.metadata().level();
        if level > Level::WARN {
            return;
        }
        let mut text = Fields(String::new());
        event.record(&mut text);
        self.0.push(level.as_str(), text.0.trim());
    }
}

struct Fields(String);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.insert_str(0, value);
        } else {
            self.0.push_str(&format!(" {}={value}", field.name()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.insert_str(0, &format!("{value:?}"));
        } else {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }
}

/// Hides the values of every secret the config names: `[secrets]`, the
/// Telegram token and the providers' API keys.
pub fn redactor(cfg: &Config) -> Redactor {
    let mut names: Vec<String> = cfg.secrets.keys().cloned().collect();
    names.extend(cfg.gateway.telegram_token_env.clone());
    names.extend(cfg.providers.values().map(|p| p.api_key_env.clone()));
    Redactor::new(names.iter().filter_map(|n| std::env::var(n).ok()))
}

/// `<data>/gateway`: the status file and the running marker.
pub fn dir() -> Result<PathBuf> {
    Ok(config::data_dir()?.join("gateway"))
}

/// The gateway's health, with the spend and schedule sections.
pub fn build(
    cfg: &Config,
    hub: Arc<ferrule_trust::Hub>,
    store: Option<Arc<TaskStore>>,
) -> Result<Health> {
    let settings = HealthSettings {
        dir: Some(dir()?),
        poll_stale: Duration::from_secs(cfg.health.poll_stale_secs.max(1)),
        watchdog_after: (cfg.health.watchdog_after_secs > 0)
            .then(|| Duration::from_secs(cfg.health.watchdog_after_secs)),
        owner: owner(cfg),
        heartbeat: (!cfg.health.heartbeat_url.trim().is_empty()).then(|| Heartbeat {
            url: cfg.health.heartbeat_url.trim().to_string(),
            every: Duration::from_secs(cfg.health.heartbeat_secs.max(1)),
        }),
        ..HealthSettings::default()
    };
    let kill = hub.clone();
    let mut health = Health::new(env!("CARGO_PKG_VERSION"), settings)
        .with_redactor(Arc::new(redactor(cfg)))
        .with_section(
            "spend and caps",
            Arc::new(move || crate::trust::status_lines(&hub)),
        )
        .with_probe(Arc::new(move || {
            kill.stopped().map(|_| "the kill switch is on".to_string())
        }));
    if let Some(store) = store {
        health = health.with_section("schedule", Arc::new(move || schedule_lines(&store)));
    }
    if let Ok(models) = crate::models::shared() {
        health = health.with_section(
            "models",
            Arc::new(move || crate::models::status_lines(&models)),
        );
    }
    let notice = startup_notice(cfg, &health);
    Ok(health
        .with_startup_notice(notice)
        .with_systemd(ferrule_gateway::sdnotify::SystemdWatchdog::from_env()))
}

/// What the owner hears when this gateway starts: the restart notice if
/// the last one didn't shut down cleanly, "back up" if they asked for it.
fn startup_notice(cfg: &Config, health: &Health) -> Option<Notice> {
    let now = SystemTime::now();
    match health.leftover(pid_alive) {
        Some(Leftover::Unclean(marker)) => {
            tracing::warn!(
                pid = marker.pid,
                interrupted = marker.turns.len(),
                "the last gateway exited without a clean shutdown"
            );
            Some(restart_notice(&marker, now))
        }
        Some(Leftover::Running(marker)) => {
            tracing::warn!(
                pid = marker.pid,
                "another gateway seems to be running on this data directory"
            );
            None
        }
        None => cfg.health.notify_on_start.then(|| Notice {
            text: format!(
                "Back up: ferrule {} started at {}.",
                env!("CARGO_PKG_VERSION"),
                stamp(now)
            ),
            fallback: None,
        }),
    }
}

/// Where the gateway's own warnings go: the owner's Telegram chat, when
/// the gateway runs Telegram.
fn owner(cfg: &Config) -> Option<(String, String)> {
    cfg.gateway.telegram_token_env.as_ref()?;
    crate::trust::owner_chat(cfg).map(|c| ("telegram".to_string(), c.to_string()))
}

/// `max_turn_minutes`, `None` when off.
pub fn max_turn(cfg: &Config) -> Option<Duration> {
    (cfg.health.max_turn_minutes > 0).then(|| Duration::from_secs(cfg.health.max_turn_minutes * 60))
}

/// The next three runs and the last failed one.
fn schedule_lines(store: &TaskStore) -> Vec<String> {
    let tasks = match store.list() {
        Ok(t) => t,
        Err(e) => return vec![format!("unknown — {e}")],
    };
    let at = |ts: i64| stamp(SystemTime::UNIX_EPOCH + Duration::from_secs(ts.max(0) as u64));
    let mut next: Vec<_> = tasks
        .iter()
        .filter(|t| t.enabled)
        .filter_map(|t| t.next_run_at.map(|n| (n, &t.name)))
        .collect();
    next.sort();
    let mut out: Vec<String> = next
        .iter()
        .take(3)
        .map(|(n, name)| format!("next: {name} at {}", at(*n)))
        .collect();
    if out.is_empty() {
        out.push("nothing scheduled".into());
    }
    match store.last_failed_run() {
        Ok(Some(run)) => {
            let name = tasks
                .iter()
                .find(|t| t.id == run.task_id)
                .map(|t| t.name.clone())
                .unwrap_or(run.task_id.clone());
            let detail = run
                .detail
                .map(|d| format!(" — {}", ferrule_gateway::health::clip(&d, 120)))
                .unwrap_or_default();
            out.push(format!(
                "last failed: {name} at {}{detail}",
                at(run.started_at)
            ));
        }
        Ok(None) => out.push("no failed run".into()),
        Err(e) => out.push(format!("last failed: unknown — {e}")),
    }
    out
}

/// A status file older than this means the daemon stopped writing it.
const STALE_STATUS: Duration = Duration::from_secs(30);

/// `ferrule status`: the running gateway's report, from the file it
/// keeps. False when no gateway is running.
pub fn status_cmd() -> Result<bool> {
    let path = dir()?.join(STATUS_FILE);
    let Ok(report) = std::fs::read_to_string(&path) else {
        println!(
            "no ferrule gateway is running (no status file at {})",
            path.display()
        );
        return Ok(false);
    };
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .unwrap_or_default();
    let pid = report_pid(&report);
    if age > STALE_STATUS {
        match pid.and_then(pid_alive) {
            Some(false) => {
                println!(
                    "no ferrule gateway is running: pid {} exited without a clean shutdown (a crash or a kill). Its last report, {} old:\n",
                    pid.unwrap_or_default(),
                    human(age)
                );
                println!("{report}");
                return Ok(false);
            }
            _ => println!(
                "warning: the gateway{} hasn't updated its status for {}; it may be wedged. Its last report:\n",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default(),
                human(age)
            ),
        }
    }
    println!("{report}");
    Ok(true)
}

/// The pid on the report's first line ("…, pid 1234").
fn report_pid(report: &str) -> Option<u32> {
    report
        .lines()
        .next()?
        .rsplit_once("pid ")?
        .1
        .trim()
        .parse()
        .ok()
}

/// Whether a process with this pid exists; `None` where we can't tell.
pub fn pid_alive(pid: u32) -> Option<bool> {
    #[cfg(unix)]
    {
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        Some(r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    #[test]
    fn the_ring_keeps_warnings_and_errors_only() {
        let ring = Arc::new(RecentLog::new(10));
        let sub = tracing_subscriber::registry().with(RingLayer(ring.clone()));
        tracing::subscriber::with_default(sub, || {
            tracing::info!("hello");
            tracing::warn!(chat = 42, "slow poll");
            tracing::error!(error = "boom", "send failed");
        });
        let lines = ring.last(5);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].ends_with("WARN slow poll chat=42"), "{lines:?}");
        assert!(
            lines[1].ends_with("ERROR send failed error=boom"),
            "{lines:?}"
        );
    }

    #[test]
    fn the_pid_is_read_from_the_first_line() {
        assert_eq!(
            report_pid("ferrule 0.1.0 — up 5 s (started x), pid 4242\n\nturns:"),
            Some(4242)
        );
        assert_eq!(pid_alive(std::process::id()), Some(true));
    }
}
