//! The kill switch: `<data>/trust/stop`. There, every run stops; gone,
//! runs go on. It fails closed: a file that can't be read is "stopped".

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopInfo {
    /// RFC 3339, UTC.
    pub at: String,
    /// `ferrule stop`, or `telegram chat <id>`.
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub struct KillSwitch {
    path: PathBuf,
}

impl KillSwitch {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn engage(&self, info: &StopInfo) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(info)?)?;
        std::fs::rename(&tmp, &self.path)
    }

    /// `true` when it was on.
    pub fn clear(&self) -> std::io::Result<bool> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// `Some` while engaged. A file that can't be read or parsed counts as
    /// engaged, with what went wrong as the reason.
    pub fn status(&self) -> Option<StopInfo> {
        match std::fs::read(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => Some(StopInfo {
                at: String::new(),
                by: "an unreadable stop file".into(),
                reason: Some(format!("{} can't be read: {e}", self.path.display())),
            }),
            Ok(bytes) => Some(serde_json::from_slice(&bytes).unwrap_or(StopInfo {
                at: String::new(),
                by: "a stop file ferrule didn't write".into(),
                reason: None,
            })),
        }
    }
}

/// What a run answers while the switch is on.
pub fn stop_message(info: &StopInfo) -> String {
    let when = if info.at.is_empty() {
        String::new()
    } else {
        format!(" at {}", info.at)
    };
    let why = info
        .reason
        .as_deref()
        .map(|r| format!(", reason: {r}"))
        .unwrap_or_default();
    format!(
        "Ferrule is stopped (by {}{when}{why}). Nothing was run. \
         `ferrule stop --clear` or /resume turns it back on.",
        info.by
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engage_status_clear_and_failing_closed() {
        let dir = tempfile::tempdir().unwrap();
        let k = KillSwitch::new(dir.path().join("trust/stop"));
        assert_eq!(k.status(), None);
        assert!(!k.clear().unwrap());
        let info = StopInfo {
            at: "2026-09-25T10:00:00+00:00".into(),
            by: "ferrule stop".into(),
            reason: Some("runaway".into()),
        };
        k.engage(&info).unwrap();
        assert_eq!(k.status(), Some(info.clone()));
        assert!(stop_message(&info).contains("reason: runaway"));
        assert!(k.clear().unwrap());
        assert_eq!(k.status(), None);

        std::fs::write(k.path(), "not json").unwrap();
        assert!(k.status().is_some(), "garbage is engaged");
        std::fs::remove_file(k.path()).unwrap();
        std::fs::create_dir(k.path()).unwrap();
        assert!(k.status().is_some(), "unreadable is engaged");
    }
}
