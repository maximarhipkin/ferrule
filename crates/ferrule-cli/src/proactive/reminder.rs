//! M64: reminders through the scheduler (docs/m64-proactive.md §6). A
//! reminder is a built-in one-off task named [`TASK`]: at its time the
//! scheduler hands its text to the owner's chat as is, with no model call
//! and no agent turn, so text that came from a calendar or a mail can't
//! become a prompt the owner's task runs. It shows in `ferrule tasks` and
//! on the Tasks page like any task, and can be paused or deleted there.

use crate::config;
use anyhow::{anyhow, bail, Result};
use ferrule_gateway::scheduler::{BuiltinJob, JobReport, Task};
use ferrule_gateway::{NewTask, Scheduler, TaskKind, TaskStore, BUILTIN_CHANNEL};
use ferrule_trust::Hub;
use serde_json::json;
use std::sync::Arc;

/// The built-in task name every reminder has.
pub const TASK: &str = "ferrule-reminder";

/// Schedules `text` for `at` (unix seconds); the new task's id.
pub fn create(hub: &Hub, at: i64, text: &str, by: &str) -> Result<String> {
    let now = chrono::Utc::now();
    if at <= now.timestamp() {
        bail!("that time has passed");
    }
    let when = chrono::DateTime::from_timestamp(at, 0).ok_or_else(|| anyhow!("no such time"))?;
    let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
    let id = uuid::Uuid::new_v4().to_string();
    let task = store.add(
        NewTask {
            name: TASK.into(),
            kind: TaskKind::Once,
            schedule: when.to_rfc3339(),
            timezone: "UTC".into(),
            channel: BUILTIN_CHANNEL.into(),
            chat_id: "owner".into(),
            prompt: text.to_string(),
            gate: None,
            model: None,
        },
        id,
        now.timestamp(),
        Some(at),
    )?;
    hub.audit().record(
        now,
        "task.add",
        None,
        None,
        json!({"task": task.id, "name": TASK, "kind": "once", "schedule": task.schedule, "by": by}),
    );
    Ok(task.id)
}

/// Deletes the reminder `id`; `false` when it's gone already. Refuses a
/// task that isn't a reminder.
pub fn delete(hub: &Hub, id: &str, by: &str) -> Result<bool> {
    let store = TaskStore::open(config::data_dir()?.join("tasks.db"))?;
    let Some(t) = store.get(id)? else {
        return Ok(false);
    };
    if !is_reminder(&t) {
        bail!("{} isn't a reminder", t.name);
    }
    store.delete(id)?;
    hub.audit().record(
        chrono::Utc::now(),
        "task_deleted",
        None,
        None,
        json!({"task": id, "name": t.name, "by": by}),
    );
    Ok(true)
}

/// Whether `t` is one of the proactive agent's reminders.
pub fn is_reminder(t: &Task) -> bool {
    t.channel == BUILTIN_CHANNEL && t.name == TASK
}

/// The job reminders run: registered whether proactive is on or not, so a
/// reminder the owner accepted still comes after proactive is turned off.
pub fn register(scheduler: Scheduler, hub: Arc<Hub>) -> Scheduler {
    scheduler.with_builtin(TASK, Arc::new(Job { hub }))
}

struct Job {
    hub: Arc<Hub>,
}

#[async_trait::async_trait]
impl BuiltinJob for Job {
    async fn run(&self, task: &Task) -> Result<JobReport, String> {
        let text = format!("Reminder: {}", task.prompt);
        let (chat, _) = self.hub.post_in(None, &text, &[]).await?;
        Ok(JobReport {
            summary: format!("reminded the owner in {}", chat.channel_title()),
            incomplete: None,
        })
    }
}
