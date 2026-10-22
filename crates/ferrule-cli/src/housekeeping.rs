//! Wave A: the daily pass that prunes what an always-on bot keeps making
//! (docs/housekeeping.md). Files under the workspace's `.ferrule/spill/`
//! (long tool output and background-job logs), `.ferrule/mcp-content/`
//! (MCP servers' images and files), `.ferrule/thumbs/` (M65's video
//! posters) and `media/screen/` (M61's captures), the data dir's
//! `learn/reactions/` (M53.5's 👎 transcript cuts), by age and then by
//! size; `.ferrule/trash/` (what the Files page deleted) after 7 days; and
//! the trust audit log's lines by age. Nothing else
//! is looked at: never a transcript, memory, the ledger, the config or a
//! key.

use crate::config;
use anyhow::Result;
use clap::Subcommand;
use ferrule_gateway::scheduler::{
    ensure_builtin, BuiltinJob, BuiltinSpec, Ensured, JobReport, Task,
};
use ferrule_gateway::Scheduler;
use ferrule_trust::audit::{Audit, AuditEvent};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// `[housekeeping]`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HousekeepingConfig {
    /// The daily pass in the gateway (`ferrule housekeeping run` works
    /// either way).
    pub enabled: bool,
    /// Days the files and the audit log's egress lines are kept; 0 keeps
    /// them all.
    pub keep_days: u64,
    /// Days the audit log's other events (approvals, caps, grants) are
    /// kept; 0 keeps them all.
    pub audit_keep_days: u64,
    /// The most each directory may hold after the age rule, in MB; the
    /// oldest files go first. 0: no size cap.
    pub max_mb: u64,
}

impl Default for HousekeepingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_days: 14,
            audit_keep_days: 90,
            max_mb: 500,
        }
    }
}

/// The built-in task's name in tasks.db.
pub const TASK_NAME: &str = "ferrule-housekeeping";

/// A directory the pass prunes: `base/rel`.
pub struct Target {
    pub name: &'static str,
    pub rel: &'static str,
    base: PathBuf,
}

/// Every directory the pass prunes, and nothing else.
pub fn targets(workspace: &Path, data: &Path) -> Vec<Target> {
    let at = |name, base: &Path, rel| Target {
        name,
        rel,
        base: base.to_path_buf(),
    };
    vec![
        at("spill", workspace, ferrule_core::tool::SPILL_DIR),
        at("mcp-content", workspace, ferrule_mcp::client::MEDIA_DIR),
        at("screen", workspace, crate::screen::DIR),
        at("reactions", data, "learn/reactions"),
        // M65 §5: the Files page's video posters (made again on demand).
        at("thumbs", workspace, crate::dashboard::files_page::THUMBS),
    ]
}

/// M65 §5: what the Files page deleted waits here this many days, then
/// goes, whatever `keep_days` says.
pub const TRASH_DAYS: u64 = 7;

/// The trust audit log (`<data>/trust/audit.jsonl`).
fn audit_path(data: &Path) -> PathBuf {
    data.join("trust").join("audit.jsonl")
}

/// `base/rel` when every part of `rel` is a real directory, with no link
/// on the way: the pass never follows one out of where it may delete.
fn real_dir(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut cur = base.to_path_buf();
    for part in Path::new(rel).components() {
        cur.push(part);
        let m = std::fs::symlink_metadata(&cur).ok()?;
        if m.file_type().is_symlink() || !m.is_dir() {
            return None;
        }
    }
    Some(cur)
}

struct Found {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

/// The plain files under `dir` (four levels down at most), and the
/// directories they're in. A link is never followed, nor removed.
fn walk(dir: &Path, depth: usize, files: &mut Vec<Found>, dirs: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(m) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let kind = m.file_type();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if depth < 4 {
                walk(&path, depth + 1, files, dirs);
                dirs.push(path);
            }
        } else if kind.is_file() {
            files.push(Found {
                path,
                modified: m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                len: m.len(),
            });
        }
    }
}

/// What the pass did to one directory (or would, on a dry run).
#[derive(Debug, Default, Clone, Copy)]
pub struct Pruned {
    pub files: usize,
    pub bytes: u64,
    /// What's left there.
    pub left: u64,
}

fn prune_dir(dir: &Path, cutoff: Option<SystemTime>, max: Option<u64>, dry: bool) -> Pruned {
    let (mut found, mut dirs) = (Vec::new(), Vec::new());
    walk(dir, 0, &mut found, &mut dirs);
    let mut p = Pruned::default();
    let gone = |f: &Found, p: &mut Pruned| {
        let removed = dry || std::fs::remove_file(&f.path).is_ok();
        if removed {
            p.files += 1;
            p.bytes += f.len;
        }
        removed
    };
    let mut kept = Vec::new();
    for f in found {
        if cutoff.is_some_and(|c| f.modified < c) && gone(&f, &mut p) {
            continue;
        }
        kept.push(f);
    }
    if let Some(max) = max {
        kept.sort_by_key(|f| f.modified);
        let mut total: u64 = kept.iter().map(|f| f.len).sum();
        let mut rest = Vec::new();
        for f in kept {
            if total > max && gone(&f, &mut p) {
                total -= f.len;
            } else {
                rest.push(f);
            }
        }
        kept = rest;
    }
    p.left = kept.iter().map(|f| f.len).sum();
    if !dry {
        // Deepest first; only an empty one goes, never the target itself.
        for d in &dirs {
            let _ = std::fs::remove_dir(d);
        }
    }
    p
}

/// What one pass did.
pub struct Report {
    pub dirs: Vec<(&'static str, &'static str, Option<Pruned>)>,
    /// (kept, dropped) audit lines, or why it couldn't be pruned.
    pub audit: Result<(usize, usize), String>,
    pub dry: bool,
}

impl Report {
    /// One line for the task's run detail.
    pub fn summary(&self) -> String {
        let (files, bytes) = self
            .dirs
            .iter()
            .filter_map(|(_, _, p)| *p)
            .fold((0, 0), |(f, b), p| (f + p.files, b + p.bytes));
        let audit = match &self.audit {
            Ok((_, dropped)) => format!("{dropped} old audit lines"),
            Err(e) => format!("the audit log not pruned ({e})"),
        };
        format!(
            "housekeeping: {} {files} files ({}), {audit}",
            if self.dry { "would remove" } else { "removed" },
            size(bytes)
        )
    }
}

fn days_ago(days: u64) -> Option<SystemTime> {
    (days > 0).then(|| SystemTime::now() - Duration::from_secs(days * 86_400))
}

/// One pass over [`targets`] and the audit log; `dry` removes nothing.
pub fn run(hk: &HousekeepingConfig, workspace: &Path, data: &Path, dry: bool) -> Report {
    let cutoff = days_ago(hk.keep_days);
    let max = (hk.max_mb > 0).then(|| hk.max_mb * 1024 * 1024);
    let mut dirs: Vec<_> = targets(workspace, data)
        .into_iter()
        .map(|t| {
            let pruned = real_dir(&t.base, t.rel).map(|dir| prune_dir(&dir, cutoff, max, dry));
            (t.name, t.rel, pruned)
        })
        .collect();
    let trash = crate::dashboard::files_page::TRASH;
    dirs.push((
        "trash",
        trash,
        real_dir(workspace, trash).map(|dir| prune_dir(&dir, days_ago(TRASH_DAYS), None, dry)),
    ));
    Report {
        dirs,
        audit: prune_audit(hk, data, dry),
        dry,
    }
}

/// The audit log's `egress` and `egress_denied` lines older than
/// `keep_days`, and its other events older than `audit_keep_days`.
fn prune_audit(hk: &HousekeepingConfig, data: &Path, dry: bool) -> Result<(usize, usize), String> {
    let now = chrono::Utc::now();
    let cut = |days: u64| (days > 0).then(|| now - chrono::Duration::days(days as i64));
    let (egress, other) = (cut(hk.keep_days), cut(hk.audit_keep_days));
    let keep = |e: &AuditEvent| {
        let Ok(at) = chrono::DateTime::parse_from_rfc3339(&e.at) else {
            return true;
        };
        let cut = if e.event.starts_with("egress") {
            egress
        } else {
            other
        };
        cut.is_none_or(|c| at >= c)
    };
    let audit = Audit::new(audit_path(data));
    if dry {
        let events = audit.read(None).map_err(|e| e.to_string())?;
        let dropped = events.iter().filter(|e| !keep(e)).count();
        return Ok((events.len() - dropped, dropped));
    }
    audit.prune(keep).map_err(|e| e.to_string())
}

/// `12.3 MB`.
pub fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1024 * 1024 * 1024 => format!("{:.1} GB", b as f64 / 1073741824.0),
        b if b >= 1024 * 1024 => format!("{:.1} MB", b as f64 / 1048576.0),
        b if b >= 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// `ferrule doctor`'s line: what each pruned place holds now, and the
/// rules.
pub fn doctor_line(hk: &HousekeepingConfig, workspace: &Path, data: &Path) -> String {
    let mut parts: Vec<String> = targets(workspace, data)
        .into_iter()
        .map(|t| {
            let held = real_dir(&t.base, t.rel).map(|dir| {
                let (mut files, mut dirs) = (Vec::new(), Vec::new());
                walk(&dir, 0, &mut files, &mut dirs);
                files.iter().map(|f| f.len).sum::<u64>()
            });
            format!("{} {}", t.name, size(held.unwrap_or(0)))
        })
        .collect();
    let audit = std::fs::metadata(audit_path(data)).map_or(0, |m| m.len());
    parts.push(format!("audit log {}", size(audit)));
    let keep = |d: u64| match d {
        0 => "all kept".to_string(),
        d => format!("{d} days"),
    };
    let rule = if hk.enabled {
        format!(
            "pruned daily at 04:20 UTC: files and egress lines {}, other audit events {}{}",
            keep(hk.keep_days),
            keep(hk.audit_keep_days),
            match hk.max_mb {
                0 => String::new(),
                mb => format!(", at most {mb} MB a directory"),
            }
        )
    } else {
        "the daily pass is off ([housekeeping] enabled = false)".into()
    };
    format!("{} · {rule}", parts.join(" · "))
}

/// The built-in task's spec.
pub fn spec() -> BuiltinSpec {
    BuiltinSpec {
        name: TASK_NAME.into(),
        schedule: "20 4 * * *".into(),
        timezone: "UTC".into(),
        description: "[built-in] prune old spill files, MCP output, screen captures, reaction cuts and audit lines (docs/housekeeping.md)".into(),
    }
}

/// The pass as a built-in task, like M16's learning pass: `ensure` first
/// makes tasks.db match `[housekeeping] enabled`, then the job goes on the
/// scheduler.
pub fn register(
    cfg: &config::Config,
    scheduler: Scheduler,
    workspace: &Path,
    ensure: bool,
) -> Scheduler {
    let on = cfg.housekeeping.enabled;
    if ensure {
        let want = on.then(spec);
        match ensure_builtin(
            scheduler.store(),
            TASK_NAME,
            want.as_ref(),
            chrono::Utc::now(),
        ) {
            Ok(Ensured::Added(id)) => tracing::info!(task = %id, "housekeeping scheduled"),
            Ok(_) => {}
            Err(e) => {
                eprintln!("ferrule: [housekeeping] schedule: {e}; the daily pass is off");
                return scheduler;
            }
        }
    }
    if !on {
        return scheduler;
    }
    let workspace = dunce::canonicalize(workspace).unwrap_or(workspace.to_path_buf());
    scheduler.with_builtin(TASK_NAME, Arc::new(Job { workspace }))
}

struct Job {
    workspace: PathBuf,
}

#[async_trait::async_trait]
impl BuiltinJob for Job {
    async fn run(&self, _task: &Task) -> Result<JobReport, String> {
        // The settings as they are now, like the learning pass.
        let (cfg, _) = config::Config::load().map_err(|e| format!("{e:#}"))?;
        let data = config::data_dir().map_err(|e| format!("{e:#}"))?;
        let workspace = self.workspace.clone();
        let report =
            tokio::task::spawn_blocking(move || run(&cfg.housekeeping, &workspace, &data, false))
                .await
                .map_err(|e| e.to_string())?;
        let summary = report.summary();
        tracing::info!("{summary}");
        Ok(JobReport {
            summary,
            incomplete: report.audit.err(),
        })
    }
}

#[derive(Subcommand)]
pub enum HousekeepingCmd {
    /// Prune now what the daily pass prunes (docs/housekeeping.md)
    Run {
        /// The workspace whose `.ferrule/` and `media/screen/` are pruned
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// Say what would go; remove nothing
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn cmd(op: HousekeepingCmd) -> Result<()> {
    match op {
        HousekeepingCmd::Run { workspace, dry_run } => {
            let (cfg, _) = config::Config::load()?;
            let data = config::data_dir()?;
            let workspace = dunce::canonicalize(&workspace).unwrap_or(workspace);
            let report = run(&cfg.housekeeping, &workspace, &data, dry_run);
            let verb = if dry_run { "would remove" } else { "removed" };
            for (name, rel, pruned) in &report.dirs {
                match pruned {
                    Some(p) => println!(
                        "{name:<12} {rel:<22} {verb} {} files ({}), {} left",
                        p.files,
                        size(p.bytes),
                        size(p.left)
                    ),
                    None => println!("{name:<12} {rel:<22} nothing there"),
                }
            }
            match &report.audit {
                Ok((kept, dropped)) => println!(
                    "{:<12} {:<22} {verb} {dropped} lines, {kept} kept",
                    "audit", "trust/audit.jsonl"
                ),
                Err(e) => println!(
                    "{:<12} {:<22} not pruned: {e}",
                    "audit", "trust/audit.jsonl"
                ),
            }
            Ok(())
        }
    }
}
