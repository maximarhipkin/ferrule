//! `<data>/trust/audit.jsonl`: what the owner's guard did, one line each.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// RFC 3339, UTC.
    pub at: String,
    /// `cap_stop`, `cap_warning`, `stop_engaged`, `stop_cleared`,
    /// `approval_asked`, `approval_answered`, `plan_proposed`,
    /// `plan_approved`, `plan_rejected`.
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default)]
    pub detail: Value,
}

pub struct Audit {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Audit {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Appends one event. A write that fails is logged, never raised: the
    /// guard's decision stands without its record.
    pub fn record(
        &self,
        at: DateTime<Utc>,
        event: &str,
        tree: Option<&str>,
        run: Option<&str>,
        detail: Value,
    ) {
        let ev = AuditEvent {
            at: at.to_rfc3339(),
            event: event.into(),
            tree: tree.map(str::to_string),
            run: run.map(str::to_string),
            detail,
        };
        let _held = self.lock.lock().unwrap();
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut line = serde_json::to_string(&ev).map_err(std::io::Error::other)?;
            line.push('\n');
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?
                .write_all(line.as_bytes())
        })();
        if let Err(e) = result {
            tracing::warn!("trust audit: write to {} failed ({e})", self.path.display());
        }
    }

    /// Every event since `since` (all when `None`); no file is no events.
    pub fn read(&self, since: Option<DateTime<Utc>>) -> std::io::Result<Vec<AuditEvent>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(text
            .lines()
            .filter_map(|l| serde_json::from_str::<AuditEvent>(l).ok())
            .filter(|e| match since {
                None => true,
                Some(s) => DateTime::parse_from_rfc3339(&e.at).is_ok_and(|t| t >= s),
            })
            .collect())
    }
}
