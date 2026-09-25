//! Scheduled tasks as the owner changes them: pause, resume, delete and
//! run now, each audited in the trust audit log. `ferrule tasks` and the
//! dashboard both go through here (docs/m22-dashboard.md).

use crate::setup::{put, table, Target};
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
    /// The config a built-in task's schedule is kept in (M24).
    config: Option<PathBuf>,
}

impl TasksAdmin {
    /// `path`: `<data>/tasks.db`; `hub`: where changes are audited.
    pub fn new(path: PathBuf, hub: Option<Arc<Hub>>) -> Self {
        Self {
            path,
            hub,
            config: None,
        }
    }

    /// Where a built-in task's schedule is written, so it survives the
    /// next start (`[learning] schedule` and `timezone`).
    pub fn with_config(mut self, config: Option<PathBuf>) -> Self {
        self.config = config;
        self
    }

    /// This machine's tasks, audited through the process's hub.
    pub fn open() -> Result<Self> {
        let (cfg, path) = crate::config::Config::load()?;
        Ok(Self::new(
            crate::config::data_dir()?.join("tasks.db"),
            crate::trust::hub(&cfg).ok(),
        )
        .with_config(Some(path)))
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
        self.record(
            event,
            serde_json::json!({ "task": t.id, "name": t.name, "by": by }),
        );
    }

    fn record(&self, event: &str, detail: serde_json::Value) {
        if let Some(hub) = &self.hub {
            hub.audit()
                .record(chrono::Utc::now(), event, None, None, detail);
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

impl TasksAdmin {
    /// A new schedule (a cron line for a cron task, a time for a one-off)
    /// and, with `timezone`, a new zone. Checked by the parser `ferrule
    /// tasks add` uses; the next run is recomputed. A built-in task's
    /// schedule is also written to `[learning]`, which it's reset from at
    /// every start.
    pub fn set_schedule(
        &self,
        id: &str,
        schedule: &str,
        timezone: Option<&str>,
        by: &str,
    ) -> Result<String> {
        let store = self.store()?;
        let t = self.task(&store, id)?;
        let schedule = schedule.trim();
        let tz = timezone.map(str::trim).filter(|z| !z.is_empty());
        let tz = tz.unwrap_or(&t.timezone).to_string();
        let next = ferrule_gateway::initial_next_run_at(t.kind, schedule, &tz, chrono::Utc::now())
            .map_err(|e| anyhow!("{schedule} ({tz}): {e}"))?;
        if t.channel == BUILTIN_CHANNEL {
            if t.name != crate::learn::TASK_NAME {
                bail!("{} is built in and its schedule can't be changed", t.name);
            }
            let Some(path) = &self.config else {
                bail!(
                    "{} is built in; change `[learning] schedule` in the config",
                    t.name
                );
            };
            edit_config(path, |t| {
                let learning = table(t.root(), &["learning"])?;
                put(learning, "schedule", schedule);
                put(learning, "timezone", tz.as_str());
                Ok(())
            })?;
        }
        store.update_schedule(id, schedule, &tz, next)?;
        self.record(
            "task.schedule",
            serde_json::json!({
                "task": t.id, "name": t.name,
                "from": t.schedule, "to": schedule,
                "from_timezone": t.timezone, "timezone": tz,
                "by": by,
            }),
        );
        let when = next
            .and_then(|n| chrono::DateTime::from_timestamp(n, 0))
            .map(|n| n.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "never".into());
        Ok(format!(
            "{} now runs on `{schedule}` ({tz}); next run {when}.",
            t.name
        ))
    }

    /// The model a task runs on; `None` for the default. The caller has
    /// checked that the model is connected.
    pub fn set_model(&self, id: &str, model: Option<&str>, by: &str) -> Result<String> {
        let store = self.store()?;
        let t = self.task(&store, id)?;
        store.set_model(id, model)?;
        self.record(
            "model.task",
            serde_json::json!({ "task": id, "from": t.model, "to": model, "by": by }),
        );
        Ok(match model {
            Some(m) => format!("{} now runs on {m}.", t.name),
            None => format!("{} now runs on the default.", t.name),
        })
    }
}

/// Read-modify-write the config under its lock; refuse (writing nothing)
/// an edit that wouldn't parse as a config.
pub(crate) fn edit_config<T>(
    path: &std::path::Path,
    edit: impl FnOnce(&mut Target) -> Result<T>,
) -> Result<T> {
    let _lock = crate::filewrite::Lock::take(path)?;
    let mut t = Target::load(path.to_path_buf())?;
    let out = edit(&mut t)?;
    t.config()
        .map_err(|e| anyhow!("that change would break the config, so it wasn't saved: {e}"))?;
    t.save()?;
    Ok(out)
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

    #[test]
    fn a_schedule_edit_is_checked_and_moves_the_next_run() {
        let (_d, admin, id) = setup();
        assert!(admin.set_schedule(&id, "not a cron", None, "t").is_err());
        assert!(admin
            .set_schedule(&id, "0 8 * * *", Some("Mars/Base"), "t")
            .is_err());
        let v = admin.view(5).unwrap();
        assert_eq!(v[0].schedule, "0 9 * * *", "a refused edit writes nothing");
        let said = admin
            .set_schedule(&id, "30 7 * * 1", Some("Asia/Jerusalem"), "t")
            .unwrap();
        assert!(said.contains("30 7 * * 1"), "{said}");
        let v = admin.view(5).unwrap();
        assert_eq!(v[0].schedule, "30 7 * * 1");
        assert_eq!(v[0].timezone, "Asia/Jerusalem");
        assert_ne!(v[0].next_run_at, Some(4_000_000_000));
        admin.set_model(&id, Some("fast"), "t").unwrap();
        assert_eq!(admin.view(5).unwrap()[0].model.as_deref(), Some("fast"));
        admin.set_model(&id, None, "t").unwrap();
        assert_eq!(admin.view(5).unwrap()[0].model, None);
        assert!(admin.set_model("nope", None, "t").is_err());
    }

    #[test]
    fn the_learning_task_s_schedule_goes_to_the_config_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");
        let config = dir.path().join("ferrule.toml");
        std::fs::write(&config, "# mine\n[learning]\nenabled = true\n").unwrap();
        let store = TaskStore::open(&path).unwrap();
        let t = store
            .add(
                NewTask {
                    name: crate::learn::TASK_NAME.into(),
                    kind: TaskKind::Cron,
                    schedule: "0 3 * * *".into(),
                    timezone: "UTC".into(),
                    channel: BUILTIN_CHANNEL.into(),
                    chat_id: String::new(),
                    prompt: String::new(),
                    gate: None,
                    model: None,
                },
                "t-1".into(),
                0,
                None,
            )
            .unwrap();
        let bare = TasksAdmin::new(path.clone(), None);
        assert!(bare.set_schedule(&t.id, "0 4 * * *", None, "t").is_err());
        let admin = TasksAdmin::new(path, None).with_config(Some(config.clone()));
        admin.set_schedule(&t.id, "0 4 * * *", None, "t").unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.contains("# mine"), "{text}");
        let cfg: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.learning.schedule, "0 4 * * *");
        assert_eq!(admin.view(5).unwrap()[0].schedule, "0 4 * * *");
    }
}
