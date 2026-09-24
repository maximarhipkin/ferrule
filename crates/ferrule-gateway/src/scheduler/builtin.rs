//! Built-in jobs: tasks ferrule registers itself (M16's `ferrule-learn`),
//! run in-process in place of an agent turn. A built-in task is an ordinary
//! row in `tasks` — listed, paused, run-now'd and logged like any other —
//! whose channel is [`BUILTIN_CHANNEL`] and whose name picks the job
//! registered with [`Scheduler::with_builtin`](super::Scheduler::with_builtin).

use super::{initial_next_run_at, NewTask, SchedulerError, Task, TaskKind, TaskStore};

/// The channel a built-in task is stored under. Never a real channel, so a
/// user task can't be mistaken for a built-in one by name alone.
pub const BUILTIN_CHANNEL: &str = "builtin";

/// What a built-in job reports back for the run log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobReport {
    /// One line kept as the run's detail.
    pub summary: String,
    /// Set when the job stopped early (a cap, repeated errors): the run is
    /// logged `incomplete` with this reason.
    pub incomplete: Option<String>,
}

#[async_trait::async_trait]
pub trait BuiltinJob: Send + Sync {
    /// Runs the job once. `Err` is logged as a failed run.
    async fn run(&self, task: &Task) -> Result<JobReport, String>;
}

/// What a built-in task should look like; `None` means "not wanted".
#[derive(Debug, Clone)]
pub struct BuiltinSpec {
    pub name: String,
    pub schedule: String,
    pub timezone: String,
    /// Shown as the task's prompt in `ferrule tasks list`.
    pub description: String,
}

/// What [`ensure_builtin`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensured {
    Added(String),
    Updated(String),
    Unchanged(String),
    Removed(usize),
    Absent,
}

/// Makes the store match the config: adds the built-in task named `name`
/// when `want` is set and it's missing, moves it when the schedule or
/// timezone changed (its paused/enabled state is left alone), and deletes
/// it when `want` is `None`.
pub fn ensure_builtin(
    store: &TaskStore,
    name: &str,
    want: Option<&BuiltinSpec>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Ensured, SchedulerError> {
    let existing: Vec<Task> = store
        .list()?
        .into_iter()
        .filter(|t| t.channel == BUILTIN_CHANNEL && t.name == name)
        .collect();
    let Some(spec) = want else {
        for t in &existing {
            store.delete(&t.id)?;
        }
        return Ok(if existing.is_empty() {
            Ensured::Absent
        } else {
            Ensured::Removed(existing.len())
        });
    };
    let next = initial_next_run_at(TaskKind::Cron, &spec.schedule, &spec.timezone, now)?;
    let Some((first, extra)) = existing.split_first() else {
        let id = uuid::Uuid::new_v4().to_string();
        let task = NewTask {
            name: name.to_string(),
            kind: TaskKind::Cron,
            schedule: spec.schedule.clone(),
            timezone: spec.timezone.clone(),
            channel: BUILTIN_CHANNEL.into(),
            chat_id: name.to_string(),
            prompt: spec.description.clone(),
            gate: None,
        };
        store.add(task, id.clone(), now.timestamp(), next)?;
        return Ok(Ensured::Added(id));
    };
    for t in extra {
        store.delete(&t.id)?;
    }
    if first.schedule == spec.schedule && first.timezone == spec.timezone {
        return Ok(Ensured::Unchanged(first.id.clone()));
    }
    store.update_schedule(&first.id, &spec.schedule, &spec.timezone, next)?;
    Ok(Ensured::Updated(first.id.clone()))
}
