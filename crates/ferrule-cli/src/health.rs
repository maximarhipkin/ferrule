//! M19b: the CLI's side of the gateway's health (docs/m19b-reliability.md)
//! — the `tracing` layer behind `/status`'s recent warnings, the redactor
//! built from the config, the report's spend and schedule sections, and
//! `ferrule status`.

use crate::config::{self, Config};
use anyhow::Result;
use ferrule_gateway::health::{
    human, restart_notice, stamp, RunningMarker, MARKER_STALE, RUNNING_FILE, STATUS_FILE,
};
use ferrule_gateway::{
    Health, HealthSettings, Heartbeat, Leftover, Notice, RecentLog, Redactor, TaskStore,
};
use std::path::{Path, PathBuf};
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
/// chat channels' tokens and the providers' API keys.
pub fn redactor(cfg: &Config) -> Redactor {
    let mut names: Vec<String> = cfg.secrets.keys().cloned().collect();
    let g = &cfg.gateway;
    names.extend(
        [
            &g.telegram_token_env,
            &g.discord_token_env,
            &g.slack_bot_token_env,
            &g.slack_app_token_env,
        ]
        .into_iter()
        .flatten()
        .cloned(),
    );
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
        let stalls = models.clone();
        health = health
            .with_section(
                "models",
                Arc::new(move || crate::models::status_lines(&models)),
            )
            .with_stall_hook(Arc::new(move |session: &str| stalls.stalled(session)));
    }
    if let Some(conns) = crate::connections::shared(cfg) {
        health = health.with_section(
            "connections",
            Arc::new(move || crate::connections::status_lines(&conns)),
        );
    }
    // M34: the remote workspace's link, from its last state (no round trip).
    if crate::remote::current().is_some() {
        health = health
            .with_section("workspace", Arc::new(crate::remote::status_lines))
            .with_probe(Arc::new(crate::remote::probe));
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

/// Where the gateway's own warnings go: the owner's primary chat on a
/// channel the gateway runs.
fn owner(cfg: &Config) -> Option<(String, String)> {
    let g = &cfg.gateway;
    crate::trust::owners(cfg)
        .into_iter()
        .find(|o| match o.channel.as_str() {
            "telegram" => g.telegram_token_env.is_some(),
            "discord" => g.discord_token_env.is_some(),
            "slack" => g.slack_bot_token_env.is_some() && g.slack_app_token_env.is_some(),
            _ => false,
        })
        .map(|o| (o.channel, o.chat))
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
    let running = status_report()?;
    // M19c: where to look next, for an installed service.
    if let crate::service::Status::Installed { .. } = crate::service::status() {
        println!("\nthe service's logs: {}", crate::service::logs_hint());
    }
    Ok(running)
}

fn status_report() -> Result<bool> {
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

/// The gateways running on this machine, by pid, this process left out
/// (M19c): the one whose running marker on this data directory is fresh,
/// and every `ferrule gateway` in the process list (Linux and macOS).
pub fn running_gateways() -> Vec<u32> {
    let mut pids = gateway_processes();
    pids.extend(marker_pid());
    pids.retain(|p| *p != std::process::id());
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// The pid in a fresh running marker, if it's alive.
fn marker_pid() -> Option<u32> {
    let path = dir().ok()?.join(RUNNING_FILE);
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .unwrap_or_default();
    let marker: RunningMarker = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    (age < MARKER_STALE && pid_alive(marker.pid) != Some(false)).then_some(marker.pid)
}

fn gateway_processes() -> Vec<u32> {
    #[allow(unused_mut)]
    let mut out = Vec::new();
    #[cfg(target_os = "linux")]
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        if is_gateway(&args) {
            out.push(pid);
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(ps) = std::process::Command::new("ps")
        .args(["-axo", "pid=,args="])
        .output()
    {
        for line in String::from_utf8_lossy(&ps.stdout).lines() {
            let mut words = line.split_whitespace();
            let Some(pid) = words.next().and_then(|p| p.parse().ok()) else {
                continue;
            };
            if is_gateway(&words.map(str::to_string).collect::<Vec<_>>()) {
                out.push(pid);
            }
        }
    }
    out
}

/// A command line running `ferrule gateway`.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn is_gateway(args: &[String]) -> bool {
    let Some((program, rest)) = args.split_first() else {
        return false;
    };
    let name = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if !matches!(name, "ferrule" | "ferrule.exe") {
        return false;
    }
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            // The one global flag with a value: `ferrule --config x gateway`.
            "--config" => {
                rest.next();
            }
            a if a.starts_with('-') => {}
            a => return a == "gateway",
        }
    }
    false
}

/// Whether a process with this pid exists; `None` where we can't tell.
pub fn pid_alive(pid: u32) -> Option<bool> {
    #[cfg(unix)]
    {
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        Some(r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE,
        };
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: plain Win32 calls; the handle is ours and closed before returning.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                // Denied means it exists but isn't ours (like EPERM); anything
                // else (ERROR_INVALID_PARAMETER) means no such process.
                return Some(GetLastError() == ERROR_ACCESS_DENIED);
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            // An exited process can still be opened while a handle to it is
            // held; its exit code tells. (One that exited with 259 reads as alive.)
            (ok != 0).then_some(code == STILL_ACTIVE as u32)
        }
    }
    #[cfg(not(any(unix, windows)))]
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
    fn a_gateway_is_told_by_its_command_line() {
        let args = |line: &str| line.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert!(is_gateway(&args("/home/max/.local/bin/ferrule gateway")));
        assert!(is_gateway(&args("ferrule gateway --provider openrouter")));
        assert!(is_gateway(&args("ferrule --config my.toml gateway")));
        assert!(is_gateway(&args("ferrule --config=my.toml gateway")));
        assert!(!is_gateway(&args("ferrule --config gateway status")));
        assert!(!is_gateway(&args("ferrule doctor")));
        assert!(!is_gateway(&args("ferrule status gateway")));
        assert!(!is_gateway(&args("vim ferrule gateway")));
        assert!(!is_gateway(&[]));
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
