//! Scheduled tasks as the owner changes them: pause, resume, delete and
//! run now, each audited in the trust audit log. `ferrule tasks` and the
//! dashboard both go through here (docs/m22-dashboard.md).

use anyhow::{anyhow, bail, Result};
use ferrule_gateway::{Task, TaskKind, TaskStore, BUILTIN_CHANNEL};
use ferrule_trust::Hub;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;

/// One task as the page shows it; never its prompt or gate command.
#[derive(Debug, Clone, Serialize)]
pub struct TaskRow {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub schedule: String,
    pub timezone: String,
    pub enabled: bool,
    pub builtin: bool,
    /// Where its answer goes: "telegram chat 42".
    pub destination: String,
    /// `None`: the default.
    pub model: Option<String>,
    /// Unix seconds.
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub runs: Vec<RunRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunRow {
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: &'static str,
    /// The error or skip reason, cut short.
    pub detail: Option<String>,
}

pub struct TasksAdmin {
    path: PathBuf,
    hub: Option<Arc<Hub>>,
}

impl TasksAdmin {
    /// `path`: `<data>/tasks.db`; `hub`: where changes are audited.
    pub fn new(path: PathBuf, hub: Option<Arc<Hub>>) -> Self {
        Self { path, hub }
    }

    /// This machine's tasks, audited through the process's hub.
    pub fn open() -> Result<Self> {
        let (cfg, _) = crate::config::Config::load()?;
        Ok(Self::new(
            crate::config::data_dir()?.join("tasks.db"),
            crate::trust::hub(&cfg).ok(),
        ))
    }

    fn store(&self) -> Result<TaskStore> {
        Ok(TaskStore::open(&self.path)?)
    }

    fn task(&self, store: &TaskStore, id: &str) -> Result<Task> {
        store.get(id)?.ok_or_else(|| anyhow!("no task {id}"))
    }

    /// Every task with its last `runs` runs.
    pub fn view(&self, runs: usize) -> Result<Vec<TaskRow>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let store = self.store()?;
        let mut out = Vec::new();
        for t in store.list()? {
            let runs = store
                .runs_for(&t.id, runs)?
                .into_iter()
                .map(|r| RunRow {
                    started_at: r.started_at,
                    finished_at: r.finished_at,
                    status: r.status.as_str(),
                    detail: r.detail.map(|d| ferrule_gateway::health::clip(&d, 200)),
                })
                .collect();
            out.push(TaskRow {
                builtin: t.channel == BUILTIN_CHANNEL,
                destination: format!("{} chat {}", t.channel, t.chat_id),
                kind: match t.kind {
                    TaskKind::Cron => "cron",
                    TaskKind::Once => "once",
                },
                id: t.id,
                name: t.name,
                schedule: t.schedule,
                timezone: t.timezone,
                enabled: t.enabled,
                model: t.model,
                next_run_at: t.next_run_at,
                last_run_at: t.last_run_at,
                runs,
            });
        }
        Ok(out)
    }

    fn audit(&self, event: &str, t: &Task, by: &str) {
        if let Some(hub) = &self.hub {
            hub.audit().record(
                chrono::Utc::now(),
                event,
                None,
                None,
                serde_json::json!({ "task": t.id, "name": t.name, "by": by }),
            );
        }
    }

    /// It stays configured but never fires until resumed.
    pub fn pause(&self, id: &str, by: &str) -> Result<String> {
        let store = self.store()?;
        let t = self.task(&store, id)?;
        store.set_enabled(id, false)?;
        self.audit("task_paused", &t, by);
        Ok(format!("Paused {} ({id}).", t.name))
    }

    pub fn resume(&self, id: &str, by: &str) -> Result<String> {
        let store = self.store()?;
        let t = self.task(&store, id)?;
        store.set_enabled(id, true)?;
        self.audit("task_resumed", &t, by);
        Ok(format!("Resumed {} ({id}).", t.name))
    }

    /// The task and its run history. A built-in task would come back at
    /// the next start, so it's paused instead.
    pub fn delete(&self, id: &str, by: &str) -> Result<String> {
        let store = self.store()?;
        let t = self.task(&store, id)?;
        if t.channel == BUILTIN_CHANNEL {
            bail!(
                "{} is built in and comes back at the next start; pause it instead",
                t.name
            );
        }
        store.delete(id)?;
        self.audit("task_deleted", &t, by);
        Ok(format!("Deleted {} ({id}).", t.name))
    }

    /// Due now: the running gateway's scheduler runs it at its next tick
    /// (with its no-overlap guard and gate), then moves it on to its next
    /// time as usual.
    pub fn run_now(&self, id: &str, by: &str) -> Result<String> {
        let store = self.store()?;
        let t = self.task(&store, id)?;
        if !t.enabled {
            bail!("{} is paused; resume it first", t.name);
        }
        let now = chrono::Utc::now().timestamp();
        store.update_schedule(id, &t.schedule, &t.timezone, Some(now))?;
        self.audit("task_run_now", &t, by);
        Ok(format!(
            "{} runs at the scheduler's next tick; its result goes to {} chat {}.",
            t.name, t.channel, t.chat_id
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_gateway::NewTask;

    fn setup() -> (tempfile::TempDir, TasksAdmin, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");
        let store = TaskStore::open(&path).unwrap();
        let t = store
            .add(
                NewTask {
                    name: "digest".into(),
                    kind: TaskKind::Cron,
                    schedule: "0 9 * * *".into(),
                    timezone: "UTC".into(),
                    channel: "telegram".into(),
                    chat_id: "42".into(),
                    prompt: "the secret prompt".into(),
                    gate: None,
                    model: None,
                },
                "t-1".into(),
                0,
                Some(4_000_000_000),
            )
            .unwrap();
        (dir, TasksAdmin::new(path, None), t.id)
    }

    #[test]
    fn pause_resume_run_now_and_delete() {
        let (_d, admin, id) = setup();
        let v = admin.view(5).unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].enabled);
        assert!(!serde_json::to_string(&v).unwrap().contains("secret prompt"));
        admin.pause(&id, "test").unwrap();
        assert!(!admin.view(5).unwrap()[0].enabled);
        assert!(admin.run_now(&id, "test").is_err(), "paused");
        admin.resume(&id, "test").unwrap();
        admin.run_now(&id, "test").unwrap();
        let next = admin.view(5).unwrap()[0].next_run_at.unwrap();
        assert!(next <= chrono::Utc::now().timestamp());
        admin.delete(&id, "test").unwrap();
        assert!(admin.view(5).unwrap().is_empty());
        assert!(admin.pause(&id, "test").is_err(), "gone");
    }
}
