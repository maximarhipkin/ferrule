//! SQLite-backed task + run-history store. Mirrors `ferrule-memory`'s
//! rusqlite + WAL pattern: one file, `PRAGMA journal_mode = WAL`,
//! `CREATE TABLE IF NOT EXISTS` on open.
//!
//! Every method that needs "the current time" takes it as an explicit `now`
//! parameter rather than calling `Utc::now()` internally. That's what makes
//! the no-overlap guard, the interrupted-run recovery, and the missed-run
//! policy testable without any real sleeping.
//!
//! WAL mode also gives this store its cross-process safety for free: the
//! long-running `ferrule gateway` daemon and a one-off `ferrule tasks
//! run-now` invocation are two separate OS processes that can both have
//! this file open at once, and SQLite's own file locking serializes their
//! writes — which is exactly what the no-overlap guard in `start_run`
//! depends on (see its doc comment).

use super::error::SchedulerError;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Cron,
    Once,
}

impl TaskKind {
    fn as_str(self) -> &'static str {
        match self {
            TaskKind::Cron => "cron",
            TaskKind::Once => "once",
        }
    }

    fn parse(s: &str) -> Result<Self, SchedulerError> {
        match s {
            "cron" => Ok(TaskKind::Cron),
            "once" => Ok(TaskKind::Once),
            other => Err(SchedulerError::InvalidKind(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    Skipped,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Skipped => "skipped",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "running" => RunStatus::Running,
            "succeeded" => RunStatus::Succeeded,
            "skipped" => RunStatus::Skipped,
            // Anything unrecognized is treated as `failed` rather than
            // panicking — a corrupt/foreign value in this column should
            // never be mistaken for success.
            _ => RunStatus::Failed,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub kind: TaskKind,
    /// Cron: a 5-field expression. Once: an RFC 3339 timestamp.
    pub schedule: String,
    /// IANA timezone name, consulted for `Cron` tasks only.
    pub timezone: String,
    pub channel: String,
    pub chat_id: String,
    pub prompt: String,
    pub gate: Option<String>,
    pub enabled: bool,
    pub created_at: i64,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewTask {
    pub name: String,
    pub kind: TaskKind,
    pub schedule: String,
    pub timezone: String,
    pub channel: String,
    pub chat_id: String,
    pub prompt: String,
    pub gate: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub task_id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: RunStatus,
    pub detail: Option<String>,
}

/// `rusqlite::Connection` is intentionally `!Sync` (SQLite connections
/// aren't safe for concurrent access without serialization). The scheduler
/// needs `TaskStore` to be `Sync` so `Arc<Scheduler>` can be spawned onto
/// its own tokio task alongside the gateway's channel adapters — a
/// `std::sync::Mutex` is the standard fix, and safe to hold across these
/// calls since every rusqlite operation here is synchronous and the lock is
/// never held across an `.await`.
pub struct TaskStore {
    conn: Mutex<Connection>,
}

impl TaskStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SchedulerError> {
        Self::init(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self, SchedulerError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, SchedulerError> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS tasks (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 schedule TEXT NOT NULL,
                 timezone TEXT NOT NULL,
                 channel TEXT NOT NULL,
                 chat_id TEXT NOT NULL,
                 prompt TEXT NOT NULL,
                 gate TEXT,
                 enabled INTEGER NOT NULL DEFAULT 1,
                 created_at INTEGER NOT NULL,
                 next_run_at INTEGER,
                 last_run_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS runs (
                 id TEXT PRIMARY KEY,
                 task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                 started_at INTEGER NOT NULL,
                 finished_at INTEGER,
                 status TEXT NOT NULL,
                 detail TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_runs_task_status ON runs(task_id, status);
             CREATE INDEX IF NOT EXISTS idx_tasks_due ON tasks(enabled, next_run_at);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn add(
        &self,
        task: NewTask,
        id: String,
        created_at: i64,
        next_run_at: Option<i64>,
    ) -> Result<Task, SchedulerError> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO tasks (id, name, kind, schedule, timezone, channel, chat_id, prompt, gate, enabled, created_at, next_run_at, last_run_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?11, NULL)",
            params![
                id,
                task.name,
                task.kind.as_str(),
                task.schedule,
                task.timezone,
                task.channel,
                task.chat_id,
                task.prompt,
                task.gate,
                created_at,
                next_run_at,
            ],
        )?;
        Ok(Task {
            id,
            name: task.name,
            kind: task.kind,
            schedule: task.schedule,
            timezone: task.timezone,
            channel: task.channel,
            chat_id: task.chat_id,
            prompt: task.prompt,
            gate: task.gate,
            enabled: true,
            created_at,
            next_run_at,
            last_run_at: None,
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<Task>, SchedulerError> {
        self.conn.lock().unwrap()
            .query_row(
                "SELECT id, name, kind, schedule, timezone, channel, chat_id, prompt, gate, enabled, created_at, next_run_at, last_run_at
                 FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()
            .map_err(SchedulerError::from)
    }

    pub fn list(&self) -> Result<Vec<Task>, SchedulerError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, schedule, timezone, channel, chat_id, prompt, gate, enabled, created_at, next_run_at, last_run_at
             FROM tasks ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], row_to_task)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Tasks due to fire: enabled, scheduled, and their time has come.
    pub fn due_tasks(&self, now: i64) -> Result<Vec<Task>, SchedulerError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, schedule, timezone, channel, chat_id, prompt, gate, enabled, created_at, next_run_at, last_run_at
             FROM tasks WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ?1
             ORDER BY next_run_at ASC",
        )?;
        let rows = stmt.query_map(params![now], row_to_task)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Returns `true` if the row existed (regardless of whether `enabled`
    /// actually changed).
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, SchedulerError> {
        let n = self.conn.lock().unwrap().execute(
            "UPDATE tasks SET enabled = ?2 WHERE id = ?1",
            params![id, enabled as i64],
        )?;
        Ok(n > 0)
    }

    pub fn delete(&self, id: &str) -> Result<bool, SchedulerError> {
        let n = self
            .conn
            .lock()
            .unwrap()
            .execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Records that a task was executed: advances (or clears) its
    /// `next_run_at` and stamps `last_run_at`. Called once per `execute()`
    /// regardless of whether the run succeeded, failed, or was skipped by
    /// its gate — a broken task must keep getting a chance to run again,
    /// not go silently unscheduled.
    pub fn record_execution(
        &self,
        task_id: &str,
        next_run_at: Option<i64>,
        now: i64,
    ) -> Result<(), SchedulerError> {
        self.conn.lock().unwrap().execute(
            "UPDATE tasks SET next_run_at = ?2, last_run_at = ?3 WHERE id = ?1",
            params![task_id, next_run_at, now],
        )?;
        Ok(())
    }

    /// The no-overlap guard: atomically inserts a `running` row for
    /// `task_id` *only if* no other row for that task is already
    /// `running`. Returns `true` if the insert happened (caller may
    /// proceed), `false` if another run is already in flight (caller must
    /// skip). Being a single SQL statement — rather than a
    /// check-then-insert done as two round trips — is what makes this safe
    /// across concurrent callers, including two separate OS processes
    /// sharing this database file (the daemon's own tick loop and a
    /// manually invoked `ferrule tasks run-now`).
    pub fn start_run(&self, task_id: &str, run_id: &str, now: i64) -> Result<bool, SchedulerError> {
        let n = self.conn.lock().unwrap().execute(
            "INSERT INTO runs (id, task_id, started_at, status, detail)
             SELECT ?1, ?2, ?3, 'running', NULL
             WHERE NOT EXISTS (SELECT 1 FROM runs WHERE task_id = ?2 AND status = 'running')",
            params![run_id, task_id, now],
        )?;
        Ok(n > 0)
    }

    pub fn finish_run(
        &self,
        run_id: &str,
        status: RunStatus,
        detail: Option<&str>,
        finished_at: i64,
    ) -> Result<(), SchedulerError> {
        self.conn.lock().unwrap().execute(
            "UPDATE runs SET status = ?2, detail = ?3, finished_at = ?4 WHERE id = ?1",
            params![run_id, status.as_str(), detail, finished_at],
        )?;
        Ok(())
    }

    pub fn runs_for(&self, task_id: &str, limit: usize) -> Result<Vec<Run>, SchedulerError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, task_id, started_at, finished_at, status, detail FROM runs
             WHERE task_id = ?1 ORDER BY started_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![task_id, limit as i64], row_to_run)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Any run still `status = 'running'` when this is called was left that
    /// way by a process that died mid-run (crash, kill, host restart) — it
    /// did **not** actually finish successfully, truthful-status-logging
    /// requires marking it `failed` rather than leaving it stuck `running`
    /// forever (which would also permanently wedge that task's no-overlap
    /// guard, since `start_run` would forever see a phantom in-flight run).
    /// Must be called once, before the scheduler starts accepting new work.
    pub fn recover_interrupted_runs(&self, now: i64) -> Result<usize, SchedulerError> {
        let n = self.conn.lock().unwrap().execute(
            "UPDATE runs SET status = 'failed', detail = 'interrupted: process restarted before this run finished', finished_at = ?1
             WHERE status = 'running'",
            params![now],
        )?;
        Ok(n)
    }
}

fn row_to_task(row: &rusqlite::Row) -> rusqlite::Result<Task> {
    let kind_str: String = row.get(2)?;
    let kind = TaskKind::parse(&kind_str).unwrap_or(TaskKind::Cron); // column is our own enum; only reachable on external DB tampering
    Ok(Task {
        id: row.get(0)?,
        name: row.get(1)?,
        kind,
        schedule: row.get(3)?,
        timezone: row.get(4)?,
        channel: row.get(5)?,
        chat_id: row.get(6)?,
        prompt: row.get(7)?,
        gate: row.get(8)?,
        enabled: row.get::<_, i64>(9)? != 0,
        created_at: row.get(10)?,
        next_run_at: row.get(11)?,
        last_run_at: row.get(12)?,
    })
}

fn row_to_run(row: &rusqlite::Row) -> rusqlite::Result<Run> {
    let status_str: String = row.get(4)?;
    Ok(Run {
        id: row.get(0)?,
        task_id: row.get(1)?,
        started_at: row.get(2)?,
        finished_at: row.get(3)?,
        status: RunStatus::parse(&status_str),
        detail: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str) -> NewTask {
        NewTask {
            name: name.into(),
            kind: TaskKind::Cron,
            schedule: "*/5 * * * *".into(),
            timezone: "UTC".into(),
            channel: "local".into(),
            chat_id: "c1".into(),
            prompt: "say hi".into(),
            gate: None,
        }
    }

    #[test]
    fn add_and_get_roundtrip() {
        let store = TaskStore::in_memory().unwrap();
        let t = store
            .add(sample("t1"), "id-1".into(), 1000, Some(1300))
            .unwrap();
        assert_eq!(t.id, "id-1");
        let got = store.get("id-1").unwrap().unwrap();
        assert_eq!(got.name, "t1");
        assert_eq!(got.next_run_at, Some(1300));
        assert!(got.enabled);
        assert!(store.get("missing").unwrap().is_none());
    }

    #[test]
    fn due_tasks_respects_enabled_and_next_run_at() {
        let store = TaskStore::in_memory().unwrap();
        store
            .add(sample("due"), "id-due".into(), 0, Some(100))
            .unwrap();
        store
            .add(sample("future"), "id-future".into(), 0, Some(9_999))
            .unwrap();
        store
            .add(sample("paused"), "id-paused".into(), 0, Some(50))
            .unwrap();
        store.set_enabled("id-paused", false).unwrap();

        let due = store.due_tasks(500).unwrap();
        let ids: Vec<_> = due.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["id-due"]);
    }

    #[test]
    fn start_run_blocks_a_second_concurrent_run_for_the_same_task() {
        let store = TaskStore::in_memory().unwrap();
        store
            .add(sample("t1"), "id-1".into(), 0, Some(100))
            .unwrap();

        assert!(store.start_run("id-1", "run-a", 100).unwrap());
        // A second attempt while run-a is still `running` must be refused.
        assert!(!store.start_run("id-1", "run-b", 101).unwrap());

        store
            .finish_run("run-a", RunStatus::Succeeded, None, 110)
            .unwrap();
        // Once the first run is finished, a new run is allowed again.
        assert!(store.start_run("id-1", "run-c", 120).unwrap());
    }

    #[test]
    fn recover_interrupted_runs_marks_running_rows_failed() {
        let store = TaskStore::in_memory().unwrap();
        store
            .add(sample("t1"), "id-1".into(), 0, Some(100))
            .unwrap();
        store.start_run("id-1", "run-a", 100).unwrap();

        let recovered = store.recover_interrupted_runs(500).unwrap();
        assert_eq!(recovered, 1);

        let runs = store.runs_for("id-1", 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Failed);
        assert_eq!(runs[0].finished_at, Some(500));
        assert!(runs[0].detail.as_deref().unwrap().contains("interrupted"));

        // The no-overlap guard must be released by the recovery, not left
        // permanently wedged.
        assert!(store.start_run("id-1", "run-b", 501).unwrap());
    }

    #[test]
    fn delete_removes_the_task() {
        let store = TaskStore::in_memory().unwrap();
        store
            .add(sample("t1"), "id-1".into(), 0, Some(100))
            .unwrap();
        assert!(store.delete("id-1").unwrap());
        assert!(store.get("id-1").unwrap().is_none());
        assert!(!store.delete("id-1").unwrap()); // already gone
    }

    #[test]
    fn runs_for_orders_newest_first_and_respects_limit() {
        let store = TaskStore::in_memory().unwrap();
        store
            .add(sample("t1"), "id-1".into(), 0, Some(100))
            .unwrap();
        for (i, run_id) in ["r1", "r2", "r3"].iter().enumerate() {
            store.start_run("id-1", run_id, 100 + i as i64).unwrap();
            store
                .finish_run(run_id, RunStatus::Succeeded, None, 105 + i as i64)
                .unwrap();
        }
        let runs = store.runs_for("id-1", 2).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, "r3");
        assert_eq!(runs[1].id, "r2");
    }
}
