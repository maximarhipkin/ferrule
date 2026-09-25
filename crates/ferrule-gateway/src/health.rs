//! M19b: the gateway is never silently deaf. What the owner can see from a
//! phone (a receipt, `/status`, a watchdog message, a restart notice) and
//! what watches the process from outside (systemd's watchdog, a heartbeat).
//! See docs/m19b-reliability.md.

use crate::channel::Channel;
use crate::router::LaneSnapshot;
use crate::sdnotify::SystemdWatchdog;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// Replaces secrets with `[redacted]` in anything the owner is shown:
/// `/status`, the status file, the heartbeat, the running marker. Knows
/// the configured secret values, and the shape of a Telegram bot token.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    /// `secrets`: values to hide. Short ones (under 6 characters) would
    /// hide ordinary words, so they're skipped.
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<String> = secrets.into_iter().filter(|s| s.len() >= 6).collect();
        // Longest first, so a secret containing another goes whole.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self { secrets }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in &self.secrets {
            if out.contains(s.as_str()) {
                out = out.replace(s.as_str(), "[redacted]");
            }
        }
        redact_bot_tokens(&out)
    }
}

/// `123456789:AAH…` (a bot id, a colon, 30+ token characters).
fn redact_bot_tokens(text: &str) -> String {
    let b = text.as_bytes();
    let token_char = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_digit()) {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j - i >= 6 && j < b.len() && b[j] == b':' {
                let mut k = j + 1;
                while k < b.len() && token_char(b[k]) {
                    k += 1;
                }
                if k - j > 30 {
                    out.push_str(&text[copied..i]);
                    out.push_str("[redacted]");
                    copied = k;
                    i = k;
                    continue;
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    out
}

/// The first `max` characters of `text`, with "…" when cut.
pub fn clip(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// "45 s", "12 min", "3 h 5 min".
pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else {
        format!("{} h {} min", s / 3600, (s % 3600) / 60)
    }
}

/// The last warnings and errors the process logged, for `/status`. The
/// CLI's `tracing` layer fills the global one; the entries are redacted
/// when shown, not when stored.
pub struct RecentLog {
    cap: usize,
    entries: Mutex<VecDeque<(SystemTime, String, String)>>,
}

impl RecentLog {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            entries: Mutex::new(VecDeque::with_capacity(cap)),
        }
    }

    /// The process-wide one.
    pub fn global() -> Arc<RecentLog> {
        static LOG: OnceLock<Arc<RecentLog>> = OnceLock::new();
        LOG.get_or_init(|| Arc::new(RecentLog::new(50))).clone()
    }

    /// `level`: "WARN" or "ERROR". Long messages are clipped.
    pub fn push(&self, level: &str, message: &str) {
        let mut e = self.entries.lock().unwrap();
        if e.len() == self.cap {
            e.pop_front();
        }
        e.push_back((SystemTime::now(), level.to_string(), clip(message, 300)));
    }

    /// The last `n`, oldest first: "10:02:03 WARN …".
    pub fn last(&self, n: usize) -> Vec<String> {
        let e = self.entries.lock().unwrap();
        e.iter()
            .skip(e.len().saturating_sub(n))
            .map(|(at, level, msg)| format!("{} {level} {msg}", clock(*at)))
            .collect()
    }
}

/// `[health]`, as the gateway uses it (docs/m19b-reliability.md).
#[derive(Debug, Clone)]
pub struct HealthSettings {
    /// Where `status.txt` and `running.json` go (`<data>/gateway`); `None`
    /// writes nothing.
    pub dir: Option<PathBuf>,
    /// How often the status file is rewritten.
    pub status_every: Duration,
    /// A polling channel with no ok poll for this long is stale.
    pub poll_stale: Duration,
    /// A turn with no progress for this long gets one message to the
    /// owner; `None` turns the watchdog off.
    pub watchdog_after: Option<Duration>,
    /// Where the gateway's own warnings go: a channel name and a chat id.
    /// `None` sends each to the chat it's about.
    pub owner: Option<(String, String)>,
    /// Where the heartbeat goes, and how often; `None` sends none.
    pub heartbeat: Option<Heartbeat>,
}

/// `[health] heartbeat_url`: a URL (a dead man's switch such as
/// healthchecks.io, or the owner's own) that gets a POST every `every`.
#[derive(Debug, Clone)]
pub struct Heartbeat {
    pub url: String,
    pub every: Duration,
}

/// A turn this quiet counts as stuck in the heartbeat when the watchdog
/// is off.
pub const HEARTBEAT_STUCK: Duration = Duration::from_secs(600);

impl Default for HealthSettings {
    fn default() -> Self {
        Self {
            dir: None,
            status_every: Duration::from_secs(5),
            poll_stale: Duration::from_secs(300),
            watchdog_after: Some(Duration::from_secs(600)),
            owner: None,
            heartbeat: None,
        }
    }
}

/// Lines for a `/status` section, computed when asked.
pub type Section = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// Another reason the heartbeat says `degraded` (the kill switch, from the
/// CLI). It must return a fixed phrase: the heartbeat leaves the machine.
pub type Probe = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// The gateway's health: what `/status` and `ferrule status` report, and
/// (with [`crate::Gateway::with_health`]) the tasks that keep the status
/// file current.
pub struct Health {
    settings: HealthSettings,
    version: String,
    started: SystemTime,
    started_at: Instant,
    redactor: Arc<Redactor>,
    recent: Arc<RecentLog>,
    sections: Vec<(String, Section)>,
    /// When the dispatcher started on the message it's handling now.
    dispatching: Mutex<Option<Instant>>,
    /// Set by a clean shutdown: nothing writes the files again.
    closed: AtomicBool,
    /// Sent once when the gateway runs: the restart notice or "back up".
    startup: Mutex<Option<Notice>>,
    /// systemd's watchdog, when the unit asks for it.
    systemd: Option<SystemdWatchdog>,
    /// More reasons for a degraded heartbeat.
    probes: Vec<Probe>,
}

pub const STATUS_FILE: &str = "status.txt";
/// The dispatcher only acknowledges and queues; on one message this long,
/// it's wedged.
pub const DISPATCH_STUCK: Duration = Duration::from_secs(60);
/// Says a gateway is running and which turns it's in the middle of; left
/// behind only by an unclean exit.
pub const RUNNING_FILE: &str = "running.json";

/// What `running.json` holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunningMarker {
    pub pid: u32,
    pub version: String,
    /// Unix seconds.
    pub started: u64,
    /// The turns in progress, their messages redacted and cut to 80
    /// characters.
    pub turns: Vec<MarkedTurn>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MarkedTurn {
    pub place: String,
    pub channel: String,
    pub chat_id: String,
    pub text: String,
}

/// A message the gateway sends on its own: to `settings.owner`, else to
/// `fallback`.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub text: String,
    pub fallback: Option<(String, String)>,
}

/// A marker another process left: the unclean exit it proves, or a
/// gateway still running.
#[derive(Debug, Clone, PartialEq)]
pub enum Leftover {
    /// The process that wrote it is gone without cleaning up.
    Unclean(RunningMarker),
    /// The marker is fresh and its pid is alive: a second gateway on the
    /// same data directory.
    Running(RunningMarker),
}

/// A marker not rewritten for this long belongs to a dead process, even
/// if its pid now belongs to another one (a reboot reuses pids).
pub const MARKER_STALE: Duration = Duration::from_secs(30);

impl Health {
    pub fn new(version: impl Into<String>, settings: HealthSettings) -> Self {
        Self {
            settings,
            version: version.into(),
            started: SystemTime::now(),
            started_at: Instant::now(),
            redactor: Arc::new(Redactor::default()),
            recent: RecentLog::global(),
            sections: Vec::new(),
            dispatching: Mutex::new(None),
            closed: AtomicBool::new(false),
            startup: Mutex::new(None),
            systemd: None,
            probes: Vec::new(),
        }
    }

    /// Adds a reason the heartbeat can give for `degraded`.
    pub fn with_probe(mut self, probe: Probe) -> Self {
        self.probes.push(probe);
        self
    }

    /// The heartbeat's body: `ok`, or `degraded` with why — a stale
    /// channel, a stuck dispatcher, a turn with no progress, a probe (the
    /// kill switch). Built from fixed phrases, channel names, chat ids and
    /// durations only: never a message, a tool call's arguments or a
    /// secret.
    pub fn heartbeat(
        &self,
        lanes: &[LaneSnapshot],
        channels: &[Arc<dyn Channel>],
    ) -> serde_json::Value {
        let mut reasons = Vec::new();
        if let Err(why) = self.watchdog_ok(channels) {
            reasons.push(why);
        }
        let stuck = self.settings.watchdog_after.unwrap_or(HEARTBEAT_STUCK);
        for lane in lanes.iter().filter(|l| l.busy_for.is_some()) {
            if let Some(quiet) = lane.since_progress.filter(|d| *d >= stuck) {
                reasons.push(format!(
                    "a turn in {} has made no progress for {}",
                    lane.place(),
                    human(quiet)
                ));
            }
        }
        reasons.extend(channels.iter().filter_map(|c| c.problem()));
        reasons.extend(self.probes.iter().filter_map(|p| p()));
        let reason = self.redactor.redact(&reasons.join("; "));
        serde_json::json!({
            "status": if reasons.is_empty() { "ok" } else { "degraded" },
            "reason": reason,
            "version": self.version,
            "uptime_secs": self.uptime().as_secs(),
        })
    }

    /// Pings systemd's watchdog while [`Health::watchdog_ok`] holds.
    pub fn with_systemd(mut self, watchdog: Option<SystemdWatchdog>) -> Self {
        self.systemd = watchdog;
        self
    }

    pub fn systemd(&self) -> Option<&SystemdWatchdog> {
        self.systemd.as_ref()
    }

    /// Whether the gateway can hear the owner: the dispatcher isn't stuck
    /// on one message and every polling channel has polled lately (the
    /// first `poll_stale` after start count as fine). `Err` says why not.
    pub fn watchdog_ok(&self, channels: &[Arc<dyn Channel>]) -> Result<(), String> {
        if let Some(d) = self.dispatch_busy_for().filter(|d| *d > DISPATCH_STUCK) {
            return Err(format!(
                "the dispatcher has been on one message for {}",
                human(d)
            ));
        }
        let stale = self.stale_channels(channels);
        if !stale.is_empty() {
            return Err(format!(
                "no successful poll from {} for over {}",
                stale.join(", "),
                human(self.settings.poll_stale)
            ));
        }
        Ok(())
    }

    /// Sets the message the gateway sends once it runs.
    pub fn with_startup_notice(self, notice: Option<Notice>) -> Self {
        *self.startup.lock().unwrap() = notice;
        self
    }

    pub(crate) fn take_startup_notice(&self) -> Option<Notice> {
        self.startup.lock().unwrap().take()
    }

    /// The marker a previous run left in `dir`, if any. `alive` says
    /// whether a pid exists (`None` when the platform can't tell). Call it
    /// before the gateway runs: from then on the marker is ours.
    pub fn leftover(&self, alive: impl Fn(u32) -> Option<bool>) -> Option<Leftover> {
        let path = self.settings.dir.as_ref()?.join(RUNNING_FILE);
        let bytes = std::fs::read(&path).ok()?;
        let Ok(marker) = serde_json::from_slice::<RunningMarker>(&bytes) else {
            tracing::warn!(path = %path.display(), "an unreadable running marker; treating it as an unclean exit");
            return Some(Leftover::Unclean(RunningMarker {
                pid: 0,
                version: String::new(),
                started: 0,
                turns: Vec::new(),
            }));
        };
        if marker.pid == std::process::id() {
            return None;
        }
        let age = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .unwrap_or_default();
        if age < MARKER_STALE && alive(marker.pid) != Some(false) {
            return Some(Leftover::Running(marker));
        }
        Some(Leftover::Unclean(marker))
    }

    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> Self {
        self.redactor = redactor;
        self
    }

    pub fn with_recent(mut self, recent: Arc<RecentLog>) -> Self {
        self.recent = recent;
        self
    }

    /// A titled section of the report (spend, the schedule), from the CLI.
    pub fn with_section(mut self, title: impl Into<String>, lines: Section) -> Self {
        self.sections.push((title.into(), lines));
        self
    }

    pub fn settings(&self) -> &HealthSettings {
        &self.settings
    }

    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// The dispatcher took a message / is done with it.
    pub(crate) fn dispatching(&self, busy: bool) {
        *self.dispatching.lock().unwrap() = busy.then(Instant::now);
    }

    /// How long the dispatcher has been on its current message.
    pub fn dispatch_busy_for(&self) -> Option<Duration> {
        self.dispatching.lock().unwrap().map(|at| at.elapsed())
    }

    /// A polling channel is stale when its last ok poll (or, before the
    /// first, the start) is older than `poll_stale`.
    pub fn stale_channels(&self, channels: &[Arc<dyn Channel>]) -> Vec<String> {
        let now = SystemTime::now();
        channels
            .iter()
            .filter(|c| c.polls())
            .filter(|c| {
                let since = c.last_ok_poll().unwrap_or(self.started);
                now.duration_since(since).unwrap_or_default() > self.settings.poll_stale
            })
            .map(|c| c.name().to_string())
            .collect()
    }

    /// What `/status` answers and `status.txt` holds, redacted.
    /// The watchdog's one message about a turn that stopped moving.
    pub fn stall_notice(&self, lane: &LaneSnapshot) -> String {
        let quiet = lane.since_progress.unwrap_or_default();
        self.redactor.redact(&format!(
            "Stuck on {} for {} in {}, handling: '{}' — /stop to cancel it.",
            lane.activity,
            human(quiet),
            lane.place(),
            clip(&lane.text, 80)
        ))
    }

    pub fn report(&self, lanes: &[LaneSnapshot], channels: &[Arc<dyn Channel>]) -> String {
        let mut out = vec![
            format!(
                "ferrule {} — up {} (started {}), pid {}",
                self.version,
                human(self.uptime()),
                stamp(self.started),
                std::process::id()
            ),
            String::new(),
        ];
        out.push("turns:".into());
        if lanes.is_empty() {
            out.push("  none: every chat is idle".into());
        }
        for l in lanes {
            let mut line = format!("  {}: ", l.place());
            match l.busy_for {
                Some(busy) => {
                    line.push_str(&format!("{} for {}", l.activity, human(busy)));
                    if let Some(p) = l.since_progress {
                        line.push_str(&format!(", last progress {} ago", human(p)));
                    }
                }
                None => line.push_str("starting"),
            }
            if l.queued > 0 {
                line.push_str(&format!(", {} queued", l.queued));
            }
            out.push(line);
        }
        if let Some(d) = self
            .dispatch_busy_for()
            .filter(|d| *d > Duration::from_secs(5))
        {
            out.push(format!(
                "  the dispatcher has been on one message for {}",
                human(d)
            ));
        }
        for (title, lines) in &self.sections {
            out.push(String::new());
            out.push(format!("{title}:"));
            out.extend(lines().into_iter().map(|l| format!("  {l}")));
        }
        out.push(String::new());
        out.push("channels:".into());
        let stale = self.stale_channels(channels);
        for c in channels {
            let line = if !c.polls() {
                "doesn't poll".to_string()
            } else {
                let flag = if stale.iter().any(|s| s == c.name()) {
                    " — STALE"
                } else {
                    ""
                };
                match c.last_ok_poll() {
                    Some(at) => format!(
                        "last ok poll {} ago{flag}",
                        human(SystemTime::now().duration_since(at).unwrap_or_default())
                    ),
                    None => format!("no ok poll yet{flag}"),
                }
            };
            out.push(format!("  {}: {line}", c.name()));
            if let Some(problem) = c.problem() {
                out.push(format!("    {problem}"));
            }
        }
        out.push(String::new());
        out.push("recent warnings and errors:".into());
        let recent = self.recent.last(5);
        if recent.is_empty() {
            out.push("  none".into());
        }
        out.extend(recent.into_iter().map(|l| format!("  {}", clip(&l, 200))));
        // Telegram's limit is 4096 characters.
        clip(&self.redactor.redact(&out.join("\n")), 3800)
    }

    /// Writes the report to `<dir>/status.txt` (atomically), for
    /// `ferrule status`.
    pub fn write_status(&self, report: &str) {
        let Some(dir) = &self.settings.dir else {
            return;
        };
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        if let Err(e) = write_atomic(&dir.join(STATUS_FILE), report.as_bytes()) {
            tracing::debug!(error = %e, "couldn't write the status file");
        }
    }

    /// Writes `<dir>/running.json`: this process, and the turns it's in
    /// the middle of.
    pub fn write_running(&self, lanes: &[LaneSnapshot]) {
        let Some(dir) = &self.settings.dir else {
            return;
        };
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let marker = RunningMarker {
            pid: std::process::id(),
            version: self.version.clone(),
            started: unix(self.started),
            turns: lanes
                .iter()
                .filter(|l| l.busy_for.is_some())
                .map(|l| MarkedTurn {
                    place: l.place(),
                    channel: l.channel.clone(),
                    chat_id: l.chat_id.clone(),
                    text: clip(&self.redactor.redact(&l.text), 80),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&marker).unwrap_or_default();
        if let Err(e) = write_atomic(&dir.join(RUNNING_FILE), &bytes) {
            tracing::debug!(error = %e, "couldn't write the running marker");
        }
    }

    /// A clean shutdown: the files that say a gateway is running go, and
    /// nothing writes them again.
    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(dir) = &self.settings.dir {
            let _ = std::fs::remove_file(dir.join(STATUS_FILE));
            let _ = std::fs::remove_file(dir.join(RUNNING_FILE));
        }
    }
}

/// The owner's one message after an unclean exit. The interrupted turns
/// are named, never run again.
pub fn restart_notice(marker: &RunningMarker, now: SystemTime) -> Notice {
    let at = stamp(now);
    let text = match marker.turns.as_slice() {
        [] => format!(
            "I restarted at {at} after an unclean exit (a crash or a kill); no turn was running."
        ),
        [t] => format!(
            "I restarted at {at}; the turn for {} was interrupted while handling: '{}'. It won't be re-run — send it again if it's still needed.",
            t.place, t.text
        ),
        turns => {
            let mut text = format!(
                "I restarted at {at}; these turns were interrupted and won't be re-run — send them again if they're still needed:"
            );
            for t in turns {
                text.push_str(&format!("\n- {}: '{}'", t.place, t.text));
            }
            text
        }
    };
    Notice {
        text,
        fallback: marker
            .turns
            .first()
            .map(|t| (t.channel.clone(), t.chat_id.clone())),
    }
}

fn unix(at: SystemTime) -> u64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Is `text` the command `cmd` (`/status`, `/status@bot`)?
pub fn is_command(text: &str, cmd: &str) -> bool {
    let first = text.split_whitespace().next().unwrap_or("");
    first
        .split('@')
        .next()
        .unwrap_or("")
        .eq_ignore_ascii_case(cmd)
}

/// "2026-09-25 10:02:03 UTC".
pub fn stamp(at: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(at)
        .format("%Y-%m-%d %H:%M:%S UTC")
        .to_string()
}

fn clock(at: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(at)
        .format("%H:%M:%S")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_hides_configured_values_and_bot_tokens() {
        let r = Redactor::new(["sk-live-abcdef".to_string(), "abc".to_string()]);
        let text = "key sk-live-abcdef, url https://api.telegram.org/bot123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsawQ/getUpdates, abc stays";
        let out = r.redact(text);
        assert_eq!(
            out,
            "key [redacted], url https://api.telegram.org/bot[redacted]/getUpdates, abc stays"
        );
        // Times and ids aren't tokens.
        assert_eq!(
            r.redact("at 12:30, chat -100123456:x"),
            "at 12:30, chat -100123456:x"
        );
    }

    #[test]
    fn clip_and_human() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("héllo world", 5), "héllo…");
        assert_eq!(human(Duration::from_secs(5)), "5 s");
        assert_eq!(human(Duration::from_secs(600)), "10 min");
        assert_eq!(human(Duration::from_secs(3900)), "1 h 5 min");
    }

    fn busy(chat: &str, text: &str) -> LaneSnapshot {
        LaneSnapshot {
            session_id: format!("telegram-{chat}"),
            channel: "telegram".into(),
            chat_id: chat.into(),
            busy_for: Some(Duration::from_secs(3)),
            started_at: Some(SystemTime::now()),
            activity: "a model call".into(),
            since_progress: Some(Duration::from_secs(1)),
            queued: 0,
            text: text.into(),
        }
    }

    fn marker_health(dir: &Path) -> Health {
        Health::new(
            "9.9.9",
            HealthSettings {
                dir: Some(dir.to_path_buf()),
                ..Default::default()
            },
        )
        .with_redactor(Arc::new(Redactor::new(["hunter2".to_string()])))
    }

    #[test]
    fn the_heartbeat_says_why_it_is_degraded_without_messages_or_secrets() {
        let health = marker_health(Path::new("/nonexistent"));
        let ok = health.heartbeat(&[busy("42", "hi")], &[]);
        assert_eq!(ok["status"], "ok");
        assert_eq!(ok["reason"], "");
        assert_eq!(ok["version"], "9.9.9");
        assert!(ok["uptime_secs"].is_u64());

        let mut stuck = busy("42", "my password is hunter2, deploy it");
        stuck.activity = "tool `shell` (curl -H 'token: hunter2' x)".into();
        stuck.since_progress = Some(Duration::from_secs(700));
        let mut idle = busy("7", "old");
        idle.busy_for = None;
        idle.since_progress = Some(Duration::from_secs(9999));
        let health = health.with_probe(Arc::new(|| Some("the kill switch is on".into())));
        let beat = health.heartbeat(&[stuck, idle], &[]);
        assert_eq!(beat["status"], "degraded");
        assert_eq!(
            beat["reason"],
            "a turn in telegram chat 42 has made no progress for 11 min; the kill switch is on"
        );
        let body = beat.to_string();
        for leak in ["hunter2", "password", "deploy", "curl", "shell", "redacted"] {
            assert!(!body.contains(leak), "{leak} in {body}");
        }
    }

    #[test]
    fn the_marker_names_the_turns_in_progress_and_a_clean_shutdown_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let health = marker_health(dir.path());
        let mut idle = busy("7", "done");
        idle.busy_for = None;
        let long = format!("my password is hunter2 {}", "x".repeat(200));
        health.write_running(&[busy("-100", &long), idle]);
        let marker: RunningMarker =
            serde_json::from_slice(&std::fs::read(dir.path().join(RUNNING_FILE)).unwrap()).unwrap();
        assert_eq!(marker.pid, std::process::id());
        assert_eq!(marker.turns.len(), 1);
        assert_eq!(marker.turns[0].place, "telegram chat -100");
        assert!(marker.turns[0]
            .text
            .starts_with("my password is [redacted] xx"));
        // The first 80 characters, and a mark that there was more.
        assert_eq!(marker.turns[0].text.chars().count(), 81);
        assert!(marker.turns[0].text.ends_with('…'));
        // Our own marker isn't a leftover.
        assert_eq!(health.leftover(|_| Some(true)), None);
        health.write_status("report");
        health.shutdown();
        assert!(!dir.path().join(RUNNING_FILE).exists());
        assert!(!dir.path().join(STATUS_FILE).exists());
        // Nothing writes them after a clean shutdown.
        health.write_running(&[]);
        health.write_status("late");
        assert!(!dir.path().join(RUNNING_FILE).exists());
        assert!(!dir.path().join(STATUS_FILE).exists());
    }

    #[test]
    fn a_leftover_marker_is_an_unclean_exit_unless_its_gateway_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let health = marker_health(dir.path());
        let path = dir.path().join(RUNNING_FILE);
        let marker = RunningMarker {
            pid: 999_999,
            version: "9.9.8".into(),
            started: 1,
            turns: vec![MarkedTurn {
                place: "telegram chat -100".into(),
                channel: "telegram".into(),
                chat_id: "-100".into(),
                text: "deploy the site".into(),
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&marker).unwrap()).unwrap();
        assert_eq!(
            health.leftover(|_| Some(false)),
            Some(Leftover::Unclean(marker.clone()))
        );
        // Fresh and alive: another gateway, not a crash.
        assert_eq!(
            health.leftover(|_| Some(true)),
            Some(Leftover::Running(marker.clone()))
        );
        // Stale: its pid belongs to someone else now.
        let old = SystemTime::now() - Duration::from_secs(120);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(
            health.leftover(|_| Some(true)),
            Some(Leftover::Unclean(marker.clone()))
        );
        std::fs::write(&path, "{not json").unwrap();
        assert!(matches!(
            health.leftover(|_| Some(true)),
            Some(Leftover::Unclean(_))
        ));
    }

    #[test]
    fn the_restart_notice_names_what_was_interrupted() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        let turn = |chat: &str, text: &str| MarkedTurn {
            place: format!("telegram chat {chat}"),
            channel: "telegram".into(),
            chat_id: chat.into(),
            text: text.into(),
        };
        let mut marker = RunningMarker {
            pid: 1,
            version: "1".into(),
            started: 0,
            turns: vec![],
        };
        let none = restart_notice(&marker, at);
        assert_eq!(
            none.text,
            "I restarted at 2026-09-21 14:13:20 UTC after an unclean exit (a crash or a kill); no turn was running."
        );
        assert_eq!(none.fallback, None);
        marker.turns.push(turn("-100", "deploy the site"));
        let one = restart_notice(&marker, at);
        assert_eq!(
            one.text,
            "I restarted at 2026-09-21 14:13:20 UTC; the turn for telegram chat -100 was interrupted while handling: 'deploy the site'. It won't be re-run — send it again if it's still needed."
        );
        assert_eq!(one.fallback, Some(("telegram".into(), "-100".into())));
        marker.turns.push(turn("42", "hi"));
        let two = restart_notice(&marker, at).text;
        assert!(
            two.ends_with(":\n- telegram chat -100: 'deploy the site'\n- telegram chat 42: 'hi'"),
            "{two}"
        );
    }
}
