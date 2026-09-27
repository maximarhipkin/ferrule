//! The plans' usage windows: `<data>/plans/usage.json`, one reading per
//! plan, rewritten whole (tmp and rename) on each update, so status,
//! doctor and the dashboard in other processes see what the gateway saw.
//! Nothing secret goes in it.

use ferrule_connections::seal::write_private;
use ferrule_providers::codex::RateLimits;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// `5h`, `weekly`, or the window's length.
    pub name: String,
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    /// When it was read (unix seconds).
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default)]
    pub windows: Vec<Window>,
    /// The plan refuses turns until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limited_until: Option<u64>,
    /// The provider's own word for the state (`allowed`,
    /// `allowed_warning`, `rejected`), when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// `300` → `5h`, `10080` → `weekly`.
pub fn window_name(minutes: Option<u64>) -> String {
    match minutes {
        Some(300) => "5h".into(),
        Some(10080) => "weekly".into(),
        Some(m) if m % 1440 == 0 => format!("{}d", m / 1440),
        Some(m) if m % 60 == 0 => format!("{}h", m / 60),
        Some(m) => format!("{m}min"),
        None => "window".into(),
    }
}

impl Reading {
    /// What the Codex backend's headers and 429 body said.
    pub fn from_codex(limits: &RateLimits, now: u64) -> Self {
        Self {
            at: now,
            plan: limits.plan.clone(),
            windows: limits
                .windows
                .iter()
                .map(|w| Window {
                    name: window_name(w.minutes),
                    used_percent: w.used_percent,
                    resets_at: w.resets_at,
                })
                .collect(),
            limited_until: limits.limited_until,
            status: limits.limited_until.map(|_| "rejected".into()),
        }
    }

    /// One line for status and doctor: `5h 12% (resets in 2 h 4 min) ·
    /// weekly 40%`, with the reading's age when it's old.
    pub fn line(&self, now: u64) -> String {
        let mut parts: Vec<String> = self
            .windows
            .iter()
            .map(|w| {
                let reset = w
                    .resets_at
                    .filter(|at| *at > now)
                    .map(|at| format!(" (resets {})", short_reset(at, now)))
                    .unwrap_or_default();
                format!("{} {:.0}%{reset}", w.name, w.used_percent)
            })
            .collect();
        if let Some(until) = self.limited_until.filter(|u| *u > now) {
            parts.insert(
                0,
                format!(
                    "limit reached, resets {}",
                    ferrule_providers::codex::describe_reset(until, now)
                ),
            );
        }
        if parts.is_empty() {
            parts.push("no usage reported yet".into());
        }
        let age = now.saturating_sub(self.at);
        if age > 6 * 3600 {
            parts.push(format!("read {} h ago", age / 3600));
        }
        parts.join(" · ")
    }
}

fn short_reset(at: u64, now: u64) -> String {
    let full = ferrule_providers::codex::describe_reset(at, now);
    full.split(" (").next().unwrap_or(&full).to_string()
}

pub struct UsageFile {
    path: PathBuf,
}

impl UsageFile {
    /// The file under ferrule's data directory.
    pub fn new(data: &Path) -> Self {
        Self {
            path: data.join("plans").join("usage.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every plan's last reading; a missing or broken file is empty.
    pub fn read(&self) -> BTreeMap<String, Reading> {
        std::fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn get(&self, plan: &str) -> Option<Reading> {
        self.read().remove(plan)
    }

    /// Replace `plan`'s reading. A reading without a plan name keeps the
    /// last one's. Two writers racing lose one reading, never the file.
    pub fn record(&self, plan: &str, mut reading: Reading) {
        let mut all = self.read();
        if reading.plan.is_none() {
            reading.plan = all.get(plan).and_then(|r| r.plan.clone());
        }
        all.insert(plan.to_string(), reading);
        match serde_json::to_vec_pretty(&all) {
            Ok(bytes) => {
                if let Err(e) = write_private(&self.path, &bytes) {
                    tracing::debug!("writing {}: {e:#}", self.path.display());
                }
            }
            Err(e) => tracing::debug!("usage reading: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_providers::codex::LimitWindow;

    #[test]
    fn a_codex_reading_names_its_windows_and_reads_as_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = UsageFile::new(dir.path());
        assert!(file.get("chatgpt").is_none());
        let limits = RateLimits {
            windows: vec![
                LimitWindow {
                    minutes: Some(300),
                    used_percent: 12.5,
                    resets_at: Some(1000 + 7440),
                },
                LimitWindow {
                    minutes: Some(10080),
                    used_percent: 40.0,
                    resets_at: None,
                },
            ],
            limited_until: None,
            plan: Some("pro".into()),
        };
        file.record("chatgpt", Reading::from_codex(&limits, 1000));
        // A later reading without the plan keeps it.
        let mut again = Reading::from_codex(&limits, 1000);
        again.plan = None;
        file.record("chatgpt", again);
        let r = file.get("chatgpt").unwrap();
        assert_eq!(r.plan.as_deref(), Some("pro"));
        assert_eq!(r.windows[0].name, "5h");
        assert_eq!(r.windows[1].name, "weekly");
        assert_eq!(r.line(1000), "5h 12% (resets in 2 h 4 min) · weekly 40%");
        assert!(r.line(1000 + 7 * 3600).ends_with("read 7 h ago"));

        let mut limited = r.clone();
        limited.limited_until = Some(1000 + 600);
        assert!(limited
            .line(1000)
            .starts_with("limit reached, resets in 10 min"));
        assert_eq!(window_name(Some(1440)), "1d");
    }
}
