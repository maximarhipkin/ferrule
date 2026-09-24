//! M19b: the gateway is never silently deaf. What the owner can see from a
//! phone (a receipt, `/status`, a watchdog message, a restart notice) and
//! what watches the process from outside (systemd's watchdog, a heartbeat).
//! See docs/m19b-reliability.md.

use crate::channel::Channel;
use crate::router::LaneSnapshot;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
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
}

impl Default for HealthSettings {
    fn default() -> Self {
        Self {
            dir: None,
            status_every: Duration::from_secs(5),
            poll_stale: Duration::from_secs(300),
            watchdog_after: Some(Duration::from_secs(600)),
            owner: None,
        }
    }
}

/// Lines for a `/status` section, computed when asked.
pub type Section = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

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
}

pub const STATUS_FILE: &str = "status.txt";

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
        }
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
        if let Err(e) = write_atomic(&dir.join(STATUS_FILE), report.as_bytes()) {
            tracing::debug!(error = %e, "couldn't write the status file");
        }
    }

    /// A clean shutdown: the files that say a gateway is running go.
    pub fn shutdown(&self) {
        if let Some(dir) = &self.settings.dir {
            let _ = std::fs::remove_file(dir.join(STATUS_FILE));
        }
    }
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
}
