//! The audit log: one JSON line per hook run, in
//! `<data dir>/hooks/runs.jsonl`.

use ferrule_core::lifecycle::{HookAudit, HookRecord};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct JsonlAudit {
    path: PathBuf,
    lock: Mutex<()>,
}

impl JsonlAudit {
    pub fn in_data_dir(data_dir: &Path) -> JsonlAudit {
        JsonlAudit::at(data_dir.join("hooks").join("runs.jsonl"))
    }

    pub fn at(path: PathBuf) -> JsonlAudit {
        JsonlAudit {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The last `n` records, oldest first, as raw JSON.
    pub fn recent(&self, n: usize) -> Vec<serde_json::Value> {
        recent_runs(&self.path, n)
    }
}

impl HookAudit for JsonlAudit {
    fn record(&self, record: &HookRecord) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(mut line) = serde_json::to_string(record) else {
            return;
        };
        line.push('\n');
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = written {
            tracing::warn!(
                "can't write the hooks audit log {}: {e}",
                self.path.display()
            );
        }
    }
}

/// The last `n` lines of the log that parse, oldest first.
pub fn recent_runs(path: &Path, n: usize) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut runs: Vec<serde_json::Value> = text
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str(l).ok())
        .take(n)
        .collect();
    runs.reverse();
    runs
}
