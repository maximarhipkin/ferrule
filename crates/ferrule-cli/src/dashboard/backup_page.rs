//! Backups from the page (M47 §5c): make one, list the last few, download
//! or delete one. It is `ferrule backup` without `--include-secrets`, so a
//! file that leaves through the page never holds a key. The files live in
//! `<data>/backups/`, which a backup itself leaves out.

use super::api::{bad, confirmed, need, ok, Answer, BY};
use super::http::{Request, Response};
use super::Ctx;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// The page keeps this many of its own backups.
pub const KEEP: usize = 3;
const DIR: &str = "backups";
const EXT: &str = ".tar.gz";

/// The backup running now, if any, and how the last one ended.
#[derive(Default)]
pub struct BackupJob {
    running: bool,
    error: Option<String>,
}

pub type Job = Arc<Mutex<BackupJob>>;

fn dir_of(ctx: &Ctx) -> Option<PathBuf> {
    ctx.data.as_ref().map(|d| d.join(DIR))
}

/// Starts one in the background: `202`, or `409` while one is running.
pub fn start(ctx: &Ctx) -> Answer {
    let Some(dir) = dir_of(ctx) else {
        return bad(503, "the data folder isn't available in this process");
    };
    {
        let mut job = ctx.backups.lock().unwrap();
        if job.running {
            return bad(409, "A backup is already running.");
        }
        job.running = true;
        job.error = None;
    }
    let name = format!(
        "{}-backup-{}{EXT}",
        crate::instance::dir_name(crate::instance::current().as_deref()),
        crate::backup::stamp_now()
    );
    let job = ctx.backups.clone();
    let hub = ctx.hub.clone();
    tokio::spawn(async move {
        let out = dir.join(&name);
        let done = tokio::task::spawn_blocking({
            let dir = dir.clone();
            move || {
                std::fs::create_dir_all(&dir)?;
                crate::backup::backup(Some(out), false)?;
                prune(&dir, KEEP);
                anyhow::Ok(())
            }
        })
        .await;
        let error = match done {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(e) => Some(e.to_string()),
        };
        if let Some(hub) = hub {
            hub.audit().record(
                chrono::Utc::now(),
                "backup.made",
                None,
                None,
                json!({ "file": name, "by": BY, "ok": error.is_none() }),
            );
        }
        let mut job = job.lock().unwrap();
        job.running = false;
        job.error = error;
    });
    Some((
        202,
        json!({ "ok": true, "said": "Backing up… this takes a few seconds." }),
    ))
}

/// The backups there are, newest first: `(name, bytes, modified)`.
fn files(dir: &Path) -> Vec<(String, u64, i64)> {
    let mut out: Vec<(String, u64, i64)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let meta = std::fs::symlink_metadata(e.path()).ok()?;
            (meta.file_type().is_file() && is_name(&name)).then(|| {
                let at = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                (name, meta.len(), at)
            })
        })
        .collect();
    out.sort_by(|a, b| b.2.cmp(&a.2).then(b.0.cmp(&a.0)));
    out
}

/// A name the page made itself: one path part of plain characters that
/// ends in `.tar.gz`. No separator, no dot first, nothing odd.
fn is_name(name: &str) -> bool {
    name.len() <= 200
        && name.ends_with(EXT)
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn prune(dir: &Path, keep: usize) {
    for (name, _, _) in files(dir).into_iter().skip(keep) {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

pub fn list(ctx: &Ctx) -> Answer {
    let Some(dir) = dir_of(ctx) else {
        return bad(503, "the data folder isn't available in this process");
    };
    let (running, error) = {
        let job = ctx.backups.lock().unwrap();
        (job.running, job.error.clone())
    };
    ok(json!({
        "running": running,
        "error": error,
        "files": files(&dir).into_iter().map(|(name, bytes, at)| json!({
            "name": name, "bytes": bytes, "at": at,
        })).collect::<Vec<_>>(),
        "secrets": false,
    }))
}

/// A backup as a download, streamed from disk.
pub fn download(ctx: &Ctx, req: &Request) -> Response {
    let refuse = |status, why: String| Response::json(status, &json!({ "error": why }));
    let Some(dir) = dir_of(ctx) else {
        return refuse(
            503,
            "the data folder isn't available in this process".into(),
        );
    };
    let name = req.query.get("name").map(String::as_str).unwrap_or("");
    if !is_name(name) {
        return refuse(400, "That isn't a backup name.".into());
    }
    let path = dir.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_file() => Response::file(path, "application/gzip").with_header(
            "Content-Disposition",
            format!("attachment; filename=\"{name}\""),
        ),
        _ => refuse(404, format!("There's no backup called {name}.")),
    }
}

pub fn delete(ctx: &Ctx, body: &Value) -> Answer {
    let Some(dir) = dir_of(ctx) else {
        return bad(503, "the data folder isn't available in this process");
    };
    let name = need!(super::api::arg(body, "name"));
    if !is_name(name) {
        return bad(400, "That isn't a backup name.");
    }
    need!(confirmed(body, "Delete this backup file?".into()));
    let path = dir.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_file() => {}
        _ => return bad(404, format!("There's no backup called {name}.")),
    }
    if let Err(e) = std::fs::remove_file(&path) {
        return bad(500, format!("it couldn't be deleted: {e}"));
    }
    if let Some(hub) = &ctx.hub {
        hub.audit().record(
            chrono::Utc::now(),
            "backup.deleted",
            None,
            None,
            json!({ "file": name, "by": BY }),
        );
    }
    ok(json!({ "ok": true, "said": "Deleted the backup." }))
}
