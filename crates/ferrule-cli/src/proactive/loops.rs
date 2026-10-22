//! M64: open loops from the scheduler (docs/m64-proactive.md §1): one of
//! the owner's own tasks whose last run failed or stopped before finishing
//! is something left unfinished. A later run that succeeds closes it.
//! Other people's tasks are never read here.

use super::store::{NewCommitment, Store};
use crate::config;
use anyhow::Result;
use ferrule_gateway::{RunStatus, TaskStore, BUILTIN_CHANNEL};
use ferrule_trust::{ChatRef, Hub};

/// The source a task's loop is kept under.
fn source(task: &str) -> String {
    format!("task:{task}")
}

/// Brings the loops up to date with the owner's tasks' last runs: a new
/// loop for a task whose last run failed or was cut short (one open loop
/// per task), and its loops closed once a run succeeds. How many it added.
pub fn sync(hub: &Hub, store: &Store) -> Result<usize> {
    let db = config::data_dir()?.join("tasks.db");
    if !db.exists() {
        return Ok(0);
    }
    let tasks = TaskStore::open(&db)?;
    let owners = hub.owners();
    let mut added = 0;
    for t in tasks.list()? {
        let theirs = owners.contains(&ChatRef::new(&t.channel, &t.chat_id))
            || (t.channel == crate::dashboard::chat::CHANNEL
                && t.chat_id == crate::dashboard::chat::CHAT);
        if !theirs || t.channel == BUILTIN_CHANNEL {
            continue;
        }
        let Some(last) = tasks.runs_for(&t.id, 1)?.into_iter().next() else {
            continue;
        };
        let open: Vec<i64> = store
            .commitments_from(&source(&t.id))?
            .into_iter()
            .filter(|c| c.status == "open" || c.status == "snoozed")
            .map(|c| c.id)
            .collect();
        match last.status {
            RunStatus::Succeeded => {
                for id in open {
                    store.set_status(id, "done", None)?;
                }
            }
            RunStatus::Failed | RunStatus::Incomplete if open.is_empty() => {
                let how = if last.status == RunStatus::Failed {
                    "failed"
                } else {
                    "stopped before finishing"
                };
                let detail = last
                    .detail
                    .as_deref()
                    .map(|d| format!(": {}", ferrule_gateway::health::clip(d, 160)))
                    .unwrap_or_default();
                store.add_commitment(&NewCommitment {
                    kind: "loop".into(),
                    text: format!("The scheduled task \"{}\" {how}{detail}", t.name),
                    due: None,
                    due_day: false,
                    source: source(&t.id),
                })?;
                added += 1;
            }
            _ => {}
        }
    }
    Ok(added)
}
