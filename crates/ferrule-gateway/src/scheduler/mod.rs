//! The task scheduler: SQLite-persisted cron/one-shot tasks that trigger an
//! agent turn on schedule, with truthful run-status logging, an optional
//! gate script, no-overlap protection, and a "run at most once" missed-run
//! policy for backlog after downtime.
//!
//! ## Design: how a task reaches the agent and comes back out
//!
//! Each task gets its own dedicated, resumable session — the *same*
//! `Router`/`Transcript` machinery a chat channel uses — keyed by a
//! reserved pseudo-channel name (`"scheduler"`, never registered as a real
//! `Channel`) and the task's own id as the chat id. That means:
//!
//! - A task's conversation history persists across runs, exactly like a
//!   chat session persists across messages.
//! - The lane's own "reply to the inbound message's channel" behavior in
//!   `run_lane` naturally no-ops for these sessions, since `"scheduler"`
//!   isn't in the router's channel map — so there's no risk of a
//!   double-delivery race between the lane's own reply path and the
//!   scheduler's explicit delivery below.
//! - The scheduler delivers the *real* result to the task's actual
//!   destination (`task.channel` / `task.chat_id`) itself, once
//!   `Router::dispatch_and_wait` returns the true `Result` of the agent
//!   turn — not a fire-and-forget guess.
//!
//! ## Design: the missed-run / backlog policy
//!
//! `next_run_at` is always recomputed from *now* (the instant the previous
//! run finished), never chained forward from the old `next_run_at`. A cron
//! task that missed N occurrences during an outage still gets recomputed as
//! "next occurrence after now" — i.e. exactly one catch-up run, then normal
//! cadence resumes. A one-shot task always gets exactly one run (whenever
//! it's next observed due, even late) and then never fires again
//! (`next_run_at` is cleared). See `advance_next_run_at` below.

pub mod builtin;
pub mod error;
pub mod gate;
pub mod store;
pub mod timing;

pub use builtin::{ensure_builtin, BuiltinJob, BuiltinSpec, Ensured, JobReport, BUILTIN_CHANNEL};
pub use error::SchedulerError;
pub use store::{NewTask, Run, RunStatus, Task, TaskKind, TaskStore};

use crate::channel::Channel;
use crate::message::{InboundMessage, OutboundMessage};
use crate::router::Router;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Reserved channel name for scheduler-triggered sessions. Never register a
/// real `Channel` under this name.
pub const SCHEDULER_PSEUDO_CHANNEL: &str = "scheduler";

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Succeeded {
        answer: String,
    },
    /// The agent stopped before finishing (the step limit, a loop, a check
    /// that kept failing) and answered with a status, delivered like any
    /// answer: the person still hears where things stand.
    Incomplete {
        answer: String,
        reason: String,
    },
    Skipped {
        reason: Option<String>,
    },
    /// The no-overlap guard refused to start this run because a previous
    /// run of the same task was still in flight. Not an error — the task
    /// stays due and will be retried on the next tick.
    AlreadyRunning,
}

enum InnerOutcome {
    /// The answer, and a detail for the run log (built-in jobs only).
    Succeeded(String, Option<String>),
    Incomplete {
        answer: String,
        reason: String,
    },
    Skipped(Option<String>),
}

pub struct Scheduler {
    store: TaskStore,
    router: Arc<Router>,
    channels: HashMap<String, Arc<dyn Channel>>,
    tick_interval: Duration,
    gate_timeout: Duration,
    /// Working directory gate scripts run in. Defaults to the current
    /// directory; a fixed value keeps behavior identical between the daemon
    /// and a one-off `run-now` invocation.
    gate_workspace: PathBuf,
    /// Built-in jobs by task name, run instead of an agent turn for tasks
    /// stored under [`BUILTIN_CHANNEL`].
    builtins: HashMap<String, Arc<dyn BuiltinJob>>,
}

impl Scheduler {
    /// Builds a scheduler and immediately recovers any run left `running`
    /// by a previous process that died mid-run (crash, kill, restart) —
    /// this must happen before the scheduler (or a `run-now` caller
    /// sharing this store) accepts any new work, otherwise a genuinely
    /// interrupted run stays wedged in `running` forever and permanently
    /// blocks that task's no-overlap guard.
    pub fn new(
        store: TaskStore,
        router: Arc<Router>,
        channels: HashMap<String, Arc<dyn Channel>>,
        tick_interval: Duration,
        gate_timeout: Duration,
        gate_workspace: PathBuf,
    ) -> Result<Self, SchedulerError> {
        let recovered = store.recover_interrupted_runs(now_unix())?;
        if recovered > 0 {
            tracing::warn!(count = recovered, "recovered run(s) left `running` by a previous process; marked failed (interrupted)");
        }
        Ok(Self {
            store,
            router,
            channels,
            tick_interval,
            gate_timeout,
            gate_workspace,
            builtins: HashMap::new(),
        })
    }

    /// Registers the job a built-in task named `name` runs.
    pub fn with_builtin(mut self, name: &str, job: Arc<dyn BuiltinJob>) -> Self {
        self.builtins.insert(name.to_string(), job);
        self
    }

    /// Runs forever, checking for due tasks every `tick_interval`.
    pub async fn run(&self) -> Result<(), SchedulerError> {
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(e) = self.tick().await {
                tracing::error!(error = %e, "scheduler tick failed");
            }
        }
    }

    async fn tick(&self) -> Result<(), SchedulerError> {
        let now = now_unix();
        for task in self.store.due_tasks(now)? {
            if let Err(e) = self.execute(&task).await {
                tracing::error!(task = %task.id, task_name = %task.name, error = %e, "task execution failed");
            }
        }
        Ok(())
    }

    /// Executes one task end to end: no-overlap guard, gate, agent turn,
    /// destination delivery, run bookkeeping, and rescheduling. Shared by
    /// the tick loop and `ferrule tasks run-now` (which builds its own
    /// `Scheduler` pointed at the same store/db file — see `ferrule-cli`).
    ///
    /// Returns `Ok` for every *observed* outcome including a failed agent
    /// run (the failure is the outcome, not a scheduler malfunction);
    /// `Err` is reserved for the scheduler's own bookkeeping failing (e.g.
    /// the sqlite write itself erroring).
    pub async fn execute(&self, task: &Task) -> Result<RunOutcome, SchedulerError> {
        let run_id = uuid::Uuid::new_v4().to_string();
        let started_at = now_unix();

        if !self.store.start_run(&task.id, &run_id, started_at)? {
            tracing::warn!(task = %task.id, task_name = %task.name, "skipping: a previous run of this task is still in progress");
            return Ok(RunOutcome::AlreadyRunning);
        }

        let inner = self.execute_inner(task).await;
        let finished_at = now_unix();

        let outcome = match inner {
            Ok(InnerOutcome::Succeeded(answer, detail)) => {
                self.store.finish_run(
                    &run_id,
                    RunStatus::Succeeded,
                    detail.as_deref(),
                    finished_at,
                )?;
                RunOutcome::Succeeded { answer }
            }
            Ok(InnerOutcome::Incomplete { answer, reason }) => {
                self.store.finish_run(
                    &run_id,
                    RunStatus::Incomplete,
                    Some(&reason),
                    finished_at,
                )?;
                RunOutcome::Incomplete { answer, reason }
            }
            Ok(InnerOutcome::Skipped(reason)) => {
                self.store.finish_run(
                    &run_id,
                    RunStatus::Skipped,
                    reason.as_deref(),
                    finished_at,
                )?;
                RunOutcome::Skipped { reason }
            }
            Err(e) => {
                let detail = e.to_string();
                self.store
                    .finish_run(&run_id, RunStatus::Failed, Some(&detail), finished_at)?;
                self.notify_failure(task, &detail).await;
                // Reschedule before surfacing the error — a broken task
                // must still get a next chance, not silently fall off the
                // due-tasks list forever.
                let next = advance_next_run_at(task, finished_at)?;
                self.store.record_execution(&task.id, next, finished_at)?;
                return Err(e);
            }
        };

        let next = advance_next_run_at(task, finished_at)?;
        self.store.record_execution(&task.id, next, finished_at)?;
        Ok(outcome)
    }

    async fn execute_inner(&self, task: &Task) -> Result<InnerOutcome, SchedulerError> {
        if task.channel == BUILTIN_CHANNEL {
            return self.execute_builtin(task).await;
        }
        let mut gate_context = None;
        if let Some(gate_cmd) = &task.gate {
            match gate::run_gate(gate_cmd, &self.gate_workspace, self.gate_timeout).await {
                Ok(gate::GateOutcome::Skip { reason }) => return Ok(InnerOutcome::Skipped(reason)),
                Ok(gate::GateOutcome::Proceed { context }) => gate_context = context,
                Err(e) => return Err(e),
            }
        }

        let prompt = match &gate_context {
            Some(ctx) if !ctx.trim().is_empty() => {
                format!("{}\n\n[gate context]\n{}", task.prompt, ctx.trim())
            }
            _ => task.prompt.clone(),
        };

        let inbound = InboundMessage {
            channel: SCHEDULER_PSEUDO_CHANNEL.into(),
            chat_id: task.id.clone(),
            sender: "scheduler".into(),
            message_id: uuid::Uuid::new_v4().to_string(),
            text: prompt,
            attachments: vec![],
            reply_to: None,
            ts: now_unix(),
        };

        let reply = self.router.dispatch_and_wait(inbound).await?;
        let answer = reply.text;

        if let Some(channel) = self.channels.get(&task.channel) {
            let out = OutboundMessage {
                channel: task.channel.clone(),
                chat_id: task.chat_id.clone(),
                text: answer.clone(),
                reply_to: None,
                attachments: vec![],
            };
            if let Err(e) = channel.send(out).await {
                tracing::error!(task = %task.id, error = %e, "task succeeded but delivery to its destination channel failed");
            }
        } else {
            tracing::warn!(task = %task.id, channel = %task.channel, "task succeeded but its destination channel is not registered in this process; result not delivered");
        }

        Ok(match reply.incomplete {
            Some(reason) => InnerOutcome::Incomplete { answer, reason },
            None => InnerOutcome::Succeeded(answer, None),
        })
    }

    async fn execute_builtin(&self, task: &Task) -> Result<InnerOutcome, SchedulerError> {
        let Some(job) = self.builtins.get(&task.name) else {
            return Ok(InnerOutcome::Skipped(Some(format!(
                "built-in job `{}` isn't enabled in this process",
                task.name
            ))));
        };
        let report = job.run(task).await.map_err(SchedulerError::Builtin)?;
        Ok(match report.incomplete {
            Some(reason) => InnerOutcome::Incomplete {
                answer: report.summary,
                reason,
            },
            None => InnerOutcome::Succeeded(report.summary.clone(), Some(report.summary)),
        })
    }

    /// Best-effort: let the destination chat know a task failed, rather
    /// than only logging it (an error that's only logged and never
    /// surfaced is worse than a visible one — same reasoning `run_lane`
    /// already applies to normal chat turns).
    async fn notify_failure(&self, task: &Task, detail: &str) {
        if let Some(channel) = self.channels.get(&task.channel) {
            let out = OutboundMessage {
                channel: task.channel.clone(),
                chat_id: task.chat_id.clone(),
                text: format!("[scheduled task \"{}\" failed] {detail}", task.name),
                reply_to: None,
                attachments: vec![],
            };
            let _ = channel.send(out).await;
        }
    }

    pub fn store(&self) -> &TaskStore {
        &self.store
    }
}

/// The initial `next_run_at` for a brand-new task, computed relative to
/// `now` (task-creation time): for `Cron`, the next occurrence strictly
/// after now; for `Once`, the parsed timestamp itself — even if that
/// timestamp is already in the past, so a one-shot task created with a
/// past time still fires (once, on the next tick) rather than silently
/// never running.
pub fn initial_next_run_at(
    kind: TaskKind,
    schedule: &str,
    timezone: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<i64>, SchedulerError> {
    match kind {
        TaskKind::Cron => {
            let cron = timing::parse_cron(schedule)?;
            Ok(Some(
                timing::next_cron_occurrence(&cron, timezone, now)?.timestamp(),
            ))
        }
        TaskKind::Once => Ok(Some(timing::parse_once(schedule)?.timestamp())),
    }
}

/// The next `next_run_at` after a task has just executed. See the module
/// doc comment for the backlog-collapsing rationale: this is deliberately
/// computed from `finished_at` (now), not from the task's old
/// `next_run_at`.
fn advance_next_run_at(task: &Task, finished_at: i64) -> Result<Option<i64>, SchedulerError> {
    match task.kind {
        TaskKind::Once => Ok(None),
        TaskKind::Cron => {
            let cron = timing::parse_cron(&task.schedule)?;
            let now = timing::from_unix(finished_at);
            Ok(Some(
                timing::next_cron_occurrence(&cron, &task.timezone, now)?.timestamp(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::TimeZone;
    use ferrule_core::provider::{CompletionRequest, CompletionResponse};
    use ferrule_core::tool::ToolContext;
    use ferrule_core::{
        Agent, AgentConfig, CoreError, HarnessProfile, Message, Provider, ToolRegistry, Usage,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    // -- Pure-function tests for the missed-run / backlog policy -----------
    //
    // These test `advance_next_run_at` directly rather than through a full
    // `Scheduler::execute`, because `execute` stamps `started_at`/
    // `finished_at` from the real wall clock (`now_unix()`) with no clock
    // injection seam — exactly the kind of untestable-without-sleeping code
    // the store's "take `now` as a parameter" pattern (see store.rs's doc
    // comment) was designed to avoid. `advance_next_run_at` itself already
    // takes `finished_at` as a plain argument, so it doesn't need that seam.

    fn cron_task(next_run_at: Option<i64>) -> Task {
        Task {
            id: "id".into(),
            name: "n".into(),
            kind: TaskKind::Cron,
            schedule: "0 9 * * *".into(),
            timezone: "UTC".into(),
            channel: "c".into(),
            chat_id: "chat".into(),
            prompt: "p".into(),
            gate: None,
            enabled: true,
            created_at: 0,
            next_run_at,
            last_run_at: None,
        }
    }

    #[test]
    fn once_task_never_reschedules() {
        let mut task = cron_task(Some(500));
        task.kind = TaskKind::Once;
        task.schedule = "2020-01-01T00:00:00Z".into();
        assert_eq!(advance_next_run_at(&task, 999_999).unwrap(), None);
    }

    #[test]
    fn missed_cron_backlog_collapses_to_one_catch_up_run() {
        // Deliberately deep in the past, as if a daemon outage left this
        // task's `next_run_at` stale by weeks.
        let task = cron_task(Some(1_000));
        let finished_at = chrono::Utc
            .with_ymd_and_hms(2026, 3, 1, 12, 0, 0)
            .unwrap()
            .timestamp();

        let next = advance_next_run_at(&task, finished_at).unwrap().unwrap();

        let expected = timing::next_cron_occurrence(
            &timing::parse_cron("0 9 * * *").unwrap(),
            "UTC",
            timing::from_unix(finished_at),
        )
        .unwrap()
        .timestamp();
        assert_eq!(next, expected, "next_run_at must be recomputed from `finished_at` (now), not chained from the stale value");
        assert!(next > finished_at);
        // The bug this guards against: chaining forward from the stale
        // `next_run_at` (near the Unix epoch) would land `next` back near
        // the epoch too, not after `finished_at` — i.e. a burst of
        // backlogged runs instead of a single catch-up run.
        assert!(next > task.next_run_at.unwrap() + 1_000_000_000);
    }

    // -- Scheduler integration tests ----------------------------------------

    #[derive(Clone)]
    enum Reply {
        Ok(String),
        Err(String),
        /// Calls the same missing tool until told to stop, then answers
        /// with this status.
        Loop(String),
    }

    /// A `Provider` that counts every call (so tests can assert the agent
    /// was, or was not, ever invoked — e.g. a skipped/already-running task
    /// must never reach the provider) and returns a scripted result.
    struct ScriptedProvider {
        calls: Arc<AtomicUsize>,
        reply: Reply,
    }
    #[async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.reply {
                Reply::Ok(s) => Ok(CompletionResponse {
                    message: Message::assistant(Some(s.clone()), vec![], None),
                    usage: Usage::default(),
                }),
                Reply::Err(e) => Err(CoreError::Provider(e.clone())),
                Reply::Loop(status) => {
                    let stopping = req
                        .messages
                        .last()
                        .and_then(|m| m.content.as_deref())
                        .is_some_and(|c| c.starts_with("[ferrule] Stopping here"));
                    let message = if stopping {
                        Message::assistant(Some(status.clone()), vec![], None)
                    } else {
                        let call = ferrule_core::ToolCall {
                            id: "c".into(),
                            name: "missing".into(),
                            arguments: serde_json::json!({}),
                        };
                        Message::assistant(None, vec![call], None)
                    };
                    Ok(CompletionResponse {
                        message,
                        usage: Usage::default(),
                    })
                }
            }
        }
    }

    struct RecordingChannel {
        sent: std::sync::Mutex<Vec<OutboundMessage>>,
    }
    impl RecordingChannel {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: std::sync::Mutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            "test"
        }
        async fn run(
            &self,
            _tx: mpsc::Sender<InboundMessage>,
        ) -> Result<(), crate::error::GatewayError> {
            Ok(())
        }
        async fn send(&self, msg: OutboundMessage) -> Result<(), crate::error::GatewayError> {
            self.sent.lock().unwrap().push(msg);
            Ok(())
        }
    }

    /// Builds a `Scheduler` wired to a scripted provider (so tests can
    /// assert both the observable outcome *and* whether the agent was ever
    /// woken) and a recording destination channel, entirely in-memory/temp —
    /// no network, no real sleeping. The returned `TempDir`s must be kept
    /// alive for the duration of the test (they delete on drop).
    fn test_scheduler(
        reply: Reply,
        gate_timeout: Duration,
    ) -> (
        Scheduler,
        Arc<AtomicUsize>,
        Arc<RecordingChannel>,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_factory = calls.clone();
        let sessions_dir = tempfile::tempdir().unwrap();
        let gate_ws = tempfile::tempdir().unwrap();

        let agent_factory: crate::router::AgentFactory = Arc::new(move |_sid, transcript| {
            let provider = Arc::new(ScriptedProvider {
                calls: calls_for_factory.clone(),
                reply: reply.clone(),
            });
            // A looping agent gets one step, so the run hits the limit.
            let config = match reply {
                Reply::Loop(_) => AgentConfig {
                    max_iterations: 1,
                    ..AgentConfig::default()
                },
                _ => AgentConfig::default(),
            };
            Ok(Agent::new(
                provider,
                ToolRegistry::new(),
                HarnessProfile::generic(),
                config,
                ToolContext::default(),
                Some(transcript),
            )
            .with_system_prompt("test"))
        });

        let recorder = RecordingChannel::new();
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        channels.insert("test".into(), recorder.clone());

        let router = Arc::new(Router::new(
            sessions_dir.path(),
            agent_factory,
            channels.clone(),
        ));
        let store = TaskStore::in_memory().unwrap();
        let scheduler = Scheduler::new(
            store,
            router,
            channels,
            Duration::from_secs(30),
            gate_timeout,
            gate_ws.path().to_path_buf(),
        )
        .unwrap();
        (scheduler, calls, recorder, sessions_dir, gate_ws)
    }

    fn add_task(scheduler: &Scheduler, gate: Option<&str>) -> Task {
        let new_task = NewTask {
            name: "t".into(),
            kind: TaskKind::Cron,
            schedule: "*/5 * * * *".into(),
            timezone: "UTC".into(),
            channel: "test".into(),
            chat_id: "chat-1".into(),
            prompt: "do the thing".into(),
            gate: gate.map(str::to_string),
        };
        scheduler
            .store()
            .add(new_task, uuid::Uuid::new_v4().to_string(), 0, Some(0))
            .unwrap()
    }

    /// The headline requirement: a provider error must be logged as a
    /// truthful `failed` run with the error text attached, and must never
    /// be observable as `succeeded` — the exact NanoClaw bug (an errored
    /// run logged as `completed`) this scheduler is meant not to reproduce.
    #[tokio::test]
    async fn provider_error_produces_failed_run_never_succeeded() {
        let (scheduler, calls, _recorder, _d1, _d2) = test_scheduler(
            Reply::Err("simulated provider outage".into()),
            Duration::from_secs(5),
        );
        let task = add_task(&scheduler, None);

        let err = scheduler.execute(&task).await.unwrap_err();
        assert!(err.to_string().contains("simulated provider outage"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let runs = scheduler.store().runs_for(&task.id, 1).unwrap();
        assert_eq!(runs.len(), 1);
        assert_ne!(runs[0].status, RunStatus::Succeeded);
        assert_eq!(runs[0].status, RunStatus::Failed);
        assert!(runs[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("simulated provider outage"));
    }

    /// A run left `running` by a previous process that crashed/was killed
    /// mid-run must be recovered as `failed` ("interrupted") the moment a
    /// new `Scheduler` is constructed on that store — before it, or any
    /// concurrent `run-now`, accepts new work.
    #[tokio::test]
    async fn interrupted_running_run_recovered_as_failed_at_startup() {
        let sessions_dir = tempfile::tempdir().unwrap();
        let gate_ws = tempfile::tempdir().unwrap();
        let store = TaskStore::in_memory().unwrap();
        let new_task = NewTask {
            name: "t".into(),
            kind: TaskKind::Cron,
            schedule: "*/5 * * * *".into(),
            timezone: "UTC".into(),
            channel: "test".into(),
            chat_id: "chat-1".into(),
            prompt: "p".into(),
            gate: None,
        };
        let task = store.add(new_task, "id-1".into(), 0, Some(0)).unwrap();
        // Simulate a previous process dying mid-run: a `running` row with no
        // matching `finish_run` call.
        store.start_run(&task.id, "stale-run", 100).unwrap();

        let agent_factory: crate::router::AgentFactory = Arc::new(|_sid, transcript| {
            Ok(Agent::new(
                Arc::new(ScriptedProvider {
                    calls: Arc::new(AtomicUsize::new(0)),
                    reply: Reply::Ok("ok".into()),
                }),
                ToolRegistry::new(),
                HarnessProfile::generic(),
                AgentConfig::default(),
                ToolContext::default(),
                Some(transcript),
            )
            .with_system_prompt("test"))
        });
        let channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        let router = Arc::new(Router::new(
            sessions_dir.path(),
            agent_factory,
            channels.clone(),
        ));

        // Constructing the Scheduler must recover the stale `running` row.
        let scheduler = Scheduler::new(
            store,
            router,
            channels,
            Duration::from_secs(30),
            Duration::from_secs(5),
            gate_ws.path().to_path_buf(),
        )
        .unwrap();

        let runs = scheduler.store().runs_for(&task.id, 1).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Failed);
        assert!(runs[0].detail.as_deref().unwrap().contains("interrupted"));
    }

    /// No-overlap: a task whose previous run is still `running` must not
    /// start a second one, and — unlike the lower-level `store` test — this
    /// proves the *scheduler* never even wakes the agent in that case.
    #[tokio::test]
    async fn no_overlap_skips_when_previous_run_still_running() {
        let (scheduler, calls, _recorder, _d1, _d2) =
            test_scheduler(Reply::Ok("ok".into()), Duration::from_secs(5));
        let task = add_task(&scheduler, None);
        scheduler
            .store()
            .start_run(&task.id, "in-flight", 0)
            .unwrap();

        let outcome = scheduler.execute(&task).await.unwrap();
        assert_eq!(outcome, RunOutcome::AlreadyRunning);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "agent must not be invoked when a run is already in flight"
        );

        let runs = scheduler.store().runs_for(&task.id, 10).unwrap();
        assert_eq!(runs.len(), 1, "no second run row should have been created");
    }

    /// Gate outcome: `{"wakeAgent": false}` skips the run entirely — no
    /// agent turn — and is recorded as `skipped`, not `failed` or
    /// `succeeded`.
    #[tokio::test]
    async fn gate_skip_produces_skipped_run_with_no_agent_turn() {
        let (scheduler, calls, _recorder, _d1, _d2) = test_scheduler(
            Reply::Ok("should never be called".into()),
            Duration::from_secs(5),
        );
        let task = add_task(
            &scheduler,
            Some(r#"echo '{"wakeAgent": false, "reason": "nothing new"}'"#),
        );

        let outcome = scheduler.execute(&task).await.unwrap();
        assert_eq!(
            outcome,
            RunOutcome::Skipped {
                reason: Some("nothing new".into())
            }
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a gate that skips must never wake the agent"
        );

        let runs = scheduler.store().runs_for(&task.id, 1).unwrap();
        assert_eq!(runs[0].status, RunStatus::Skipped);
    }

    /// Gate outcome: a non-zero gate exit is a `failed` run, and the agent
    /// is never woken.
    #[tokio::test]
    async fn gate_nonzero_exit_produces_failed_run() {
        let (scheduler, calls, _recorder, _d1, _d2) =
            test_scheduler(Reply::Ok("x".into()), Duration::from_secs(5));
        let task = add_task(&scheduler, Some("exit 3"));

        let err = scheduler.execute(&task).await.unwrap_err();
        assert!(err.to_string().contains("exited with"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let runs = scheduler.store().runs_for(&task.id, 1).unwrap();
        assert_eq!(runs[0].status, RunStatus::Failed);
    }

    /// Gate outcome: a gate that runs past its timeout is killed (proved by
    /// `execute` returning promptly rather than waiting the full sleep out)
    /// and recorded as a `failed` run.
    #[tokio::test]
    async fn gate_timeout_produces_failed_run_and_kills_the_gate() {
        let (scheduler, calls, _recorder, _d1, _d2) =
            test_scheduler(Reply::Ok("x".into()), Duration::from_millis(150));
        let task = add_task(&scheduler, Some("sleep 5"));

        let started = std::time::Instant::now();
        let err = scheduler.execute(&task).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2), "execute() must return promptly, proving the gate was killed rather than awaited to completion");
        assert!(err.to_string().contains("timed out"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let runs = scheduler.store().runs_for(&task.id, 1).unwrap();
        assert_eq!(runs[0].status, RunStatus::Failed);
    }

    /// A one-shot task fires exactly once: after `execute()`, its
    /// `next_run_at` is cleared, and it never appears in `due_tasks` again —
    /// no matter how far into the future `now` moves.
    #[tokio::test]
    async fn one_shot_task_fires_at_most_once() {
        let (scheduler, calls, _recorder, _d1, _d2) =
            test_scheduler(Reply::Ok("done".into()), Duration::from_secs(5));
        let new_task = NewTask {
            name: "once".into(),
            kind: TaskKind::Once,
            schedule: "2020-01-01T00:00:00Z".into(),
            timezone: "UTC".into(),
            channel: "test".into(),
            chat_id: "chat-1".into(),
            prompt: "p".into(),
            gate: None,
        };
        let task = scheduler
            .store()
            .add(new_task, "once-1".into(), 0, Some(100))
            .unwrap();

        let outcome = scheduler.execute(&task).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Succeeded { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let refetched = scheduler.store().get(&task.id).unwrap().unwrap();
        assert_eq!(
            refetched.next_run_at, None,
            "a one-shot task must never be rescheduled after it fires"
        );

        let due = scheduler.store().due_tasks(9_999_999_999).unwrap();
        assert!(
            due.iter().all(|t| t.id != task.id),
            "a fired one-shot task must never be due again"
        );
    }

    /// A run that hits the step limit delivers the agent's status like any
    /// answer, and is recorded as incomplete, not succeeded or failed.
    #[tokio::test]
    async fn a_run_that_stops_early_is_incomplete() {
        let (scheduler, _calls, recorder, _d1, _d2) =
            test_scheduler(Reply::Loop("got halfway".into()), Duration::from_secs(5));
        let task = add_task(&scheduler, None);

        let outcome = scheduler.execute(&task).await.unwrap();
        let RunOutcome::Incomplete { answer, reason } = outcome else {
            panic!("expected Incomplete, got {outcome:?}");
        };
        assert_eq!(answer, "got halfway");
        assert_eq!(reason, "it reached the limit of 1 step");

        let sent = recorder.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "got halfway");

        let runs = scheduler.store().runs_for(&task.id, 5).unwrap();
        assert_eq!(runs[0].status, RunStatus::Incomplete);
        assert_eq!(runs[0].detail.as_deref(), Some(reason.as_str()));
    }

    // -- Built-in jobs ------------------------------------------------------

    struct CountingJob {
        runs: Arc<AtomicUsize>,
        report: Result<JobReport, String>,
    }
    #[async_trait]
    impl BuiltinJob for CountingJob {
        async fn run(&self, _task: &Task) -> Result<JobReport, String> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            self.report.clone()
        }
    }

    fn spec(schedule: &str) -> BuiltinSpec {
        BuiltinSpec {
            name: "ferrule-learn".into(),
            schedule: schedule.into(),
            timezone: "UTC".into(),
            description: "learning pass".into(),
        }
    }

    #[tokio::test]
    async fn a_builtin_job_runs_instead_of_an_agent_turn() {
        let (scheduler, calls, recorder, _d1, _d2) =
            test_scheduler(Reply::Ok("agent".into()), Duration::from_secs(5));
        let runs = Arc::new(AtomicUsize::new(0));
        let scheduler = scheduler.with_builtin(
            "ferrule-learn",
            Arc::new(CountingJob {
                runs: runs.clone(),
                report: Ok(JobReport {
                    summary: "pass 1: 1 kept".into(),
                    incomplete: None,
                }),
            }),
        );
        let now = chrono::Utc::now();
        let Ensured::Added(id) = ensure_builtin(
            scheduler.store(),
            "ferrule-learn",
            Some(&spec("0 3 * * *")),
            now,
        )
        .unwrap() else {
            panic!("not added")
        };
        let task = scheduler.store().get(&id).unwrap().unwrap();
        let out = scheduler.execute(&task).await.unwrap();
        assert!(matches!(out, RunOutcome::Succeeded { .. }));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no agent turn");
        assert!(recorder.sent.lock().unwrap().is_empty());
        let run = &scheduler.store().runs_for(&id, 1).unwrap()[0];
        assert_eq!(run.status, RunStatus::Succeeded);
        assert_eq!(run.detail.as_deref(), Some("pass 1: 1 kept"));

        // A user task with the same name is still an agent turn.
        let mut user = add_task(&scheduler, None);
        user.name = "ferrule-learn".into();
        scheduler.execute(&user).await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_builtin_job_that_errs_or_stops_early_is_logged_truthfully() {
        for (report, want) in [
            (Err("ledger unreadable".to_string()), RunStatus::Failed),
            (
                Ok(JobReport {
                    summary: "pass 2".into(),
                    incomplete: Some("stopped-budget".into()),
                }),
                RunStatus::Incomplete,
            ),
        ] {
            let (scheduler, _calls, _r, _d1, _d2) =
                test_scheduler(Reply::Ok("agent".into()), Duration::from_secs(5));
            let scheduler = scheduler.with_builtin(
                "ferrule-learn",
                Arc::new(CountingJob {
                    runs: Arc::default(),
                    report,
                }),
            );
            let now = chrono::Utc::now();
            ensure_builtin(
                scheduler.store(),
                "ferrule-learn",
                Some(&spec("0 3 * * *")),
                now,
            )
            .unwrap();
            let task = scheduler.store().list().unwrap().remove(0);
            let _ = scheduler.execute(&task).await;
            let run = &scheduler.store().runs_for(&task.id, 1).unwrap()[0];
            assert_eq!(run.status, want);
        }
    }

    #[tokio::test]
    async fn an_unregistered_builtin_is_skipped() {
        let (scheduler, calls, _r, _d1, _d2) =
            test_scheduler(Reply::Ok("agent".into()), Duration::from_secs(5));
        let now = chrono::Utc::now();
        ensure_builtin(
            scheduler.store(),
            "ferrule-learn",
            Some(&spec("0 3 * * *")),
            now,
        )
        .unwrap();
        let task = scheduler.store().list().unwrap().remove(0);
        let out = scheduler.execute(&task).await.unwrap();
        assert!(matches!(out, RunOutcome::Skipped { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ensure_builtin_adds_updates_and_removes() {
        let store = TaskStore::in_memory().unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap();
        let want = spec("0 3 * * *");
        let Ensured::Added(id) = ensure_builtin(&store, "ferrule-learn", Some(&want), now).unwrap()
        else {
            panic!("not added")
        };
        let t = store.get(&id).unwrap().unwrap();
        assert_eq!(t.channel, BUILTIN_CHANNEL);
        let three_am = chrono::Utc.with_ymd_and_hms(2026, 3, 2, 3, 0, 0).unwrap();
        assert_eq!(t.next_run_at, Some(three_am.timestamp()));
        assert_eq!(
            ensure_builtin(&store, "ferrule-learn", Some(&want), now).unwrap(),
            Ensured::Unchanged(id.clone())
        );

        store.set_enabled(&id, false).unwrap();
        let moved = spec("30 4 * * *");
        assert_eq!(
            ensure_builtin(&store, "ferrule-learn", Some(&moved), now).unwrap(),
            Ensured::Updated(id.clone())
        );
        let t = store.get(&id).unwrap().unwrap();
        assert_eq!(t.schedule, "30 4 * * *");
        assert!(!t.enabled, "a paused built-in stays paused");
        let four_thirty = chrono::Utc.with_ymd_and_hms(2026, 3, 2, 4, 30, 0).unwrap();
        assert_eq!(t.next_run_at, Some(four_thirty.timestamp()));

        assert!(ensure_builtin(&store, "ferrule-learn", Some(&spec("nope")), now).is_err());
        assert_eq!(
            ensure_builtin(&store, "ferrule-learn", None, now).unwrap(),
            Ensured::Removed(1)
        );
        assert!(store.list().unwrap().is_empty());
        assert_eq!(
            ensure_builtin(&store, "ferrule-learn", None, now).unwrap(),
            Ensured::Absent
        );
    }
}
