//! M36 §7: the repair log, `<data>/repairs.jsonl`. One line per repair
//! attempt, automatic or from the self-check; doctor and the dashboard show
//! the last ten.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

pub const FILE: &str = "repairs.jsonl";
/// The file is cut to this many lines.
pub const KEEP: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repair {
    /// Unix seconds.
    pub at: u64,
    /// The failure kind (`failure::Kind::name`) or the self-check's key.
    pub kind: String,
    pub action: String,
    pub ok: bool,
    pub detail: String,
}

/// Appenders in one process don't interleave their cuts.
static LOCK: Mutex<()> = Mutex::new(());

/// Append one line; never fails the caller (a full disk is its own problem).
pub fn log(data: &Path, kind: &str, action: &str, ok: bool, detail: &str) {
    let entry = Repair {
        at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        kind: kind.to_string(),
        action: action.to_string(),
        ok,
        detail: detail.chars().take(500).collect(),
    };
    if let Err(e) = append(data, &entry) {
        tracing::debug!("repair log not written: {e}");
    }
}

fn append(data: &Path, entry: &Repair) -> std::io::Result<()> {
    let _one = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(data)?;
    let path = data.join(FILE);
    let line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(format!("{line}\n").as_bytes())?;
    // Cut in batches: the file grows to KEEP + 50 before it's rewritten.
    let text = std::fs::read_to_string(&path)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > KEEP + 50 {
        let kept = lines[lines.len() - KEEP..].join("\n") + "\n";
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, kept)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// The last `n` repairs, oldest first.
pub fn recent(data: &Path, n: usize) -> Vec<Repair> {
    let text = std::fs::read_to_string(data.join(FILE)).unwrap_or_default();
    let all: Vec<Repair> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    all[all.len().saturating_sub(n)..].to_vec()
}

/// "2026-09-27 14:03 claude_too_old: updated claude (ok) — 2.1.0 → 2.2.0".
pub fn line(r: &Repair) -> String {
    let at = chrono::DateTime::from_timestamp(r.at as i64, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default();
    let ok = if r.ok { "ok" } else { "failed" };
    let detail = if r.detail.is_empty() {
        String::new()
    } else {
        format!(" — {}", r.detail)
    };
    format!("{at} {}: {} ({ok}){detail}", r.kind, r.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_its_last_lines_and_reads_back_the_newest() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..(KEEP + 60) {
            log(
                tmp.path(),
                "claude_too_old",
                "updated claude",
                i % 2 == 0,
                &format!("#{i}"),
            );
        }
        let text = std::fs::read_to_string(tmp.path().join(FILE)).unwrap();
        assert!(text.lines().count() <= KEEP + 50);
        let last = recent(tmp.path(), 10);
        assert_eq!(last.len(), 10);
        assert_eq!(last[9].detail, format!("#{}", KEEP + 59));
        assert!(line(&last[9]).contains("claude_too_old: updated claude (failed) — #559"));
        assert!(recent(&tmp.path().join("none"), 10).is_empty());
    }
}
