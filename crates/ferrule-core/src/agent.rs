use crate::error::CoreError;
use crate::event::AgentEvent;
use crate::guard::{unless_halted, Guard, GuardedCall, Verdict as GuardVerdict};
use crate::history::{result_ref, SEARCH_HISTORY};
use crate::hooks::{Budget, Inbox, RunEnd, RunObserver, SessionRecall, StopFlag, TurnContext};
use crate::ledger::{
    LedgerContext, LedgerRecord, LedgerSink, SpeedStats, ToolBatch, TraceEvent, TraceLevel,
};
use crate::lifecycle::{Fired, Hook, HookEvent, HookInput, HookSet, Verdict};
use crate::message::{Message, ToolCall, Usage};
use crate::profile::{HarnessProfile, COMPACTION_TEMPLATE};
use crate::provider::{CompletionRequest, CompletionResponse, Delta, DeltaSink, Provider};
use crate::routing::Signal;
use crate::stuck::{Step, Stuck};
use crate::tool::{Tool, ToolContext, ToolOutput, ToolRegistry};
use crate::transcript::Transcript;
use crate::triggers::{PromptTriggers, TriggerLoad};
use crate::verify::Verifier;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Wrapper the skills system puts around an activated skill's instructions
/// (`<skill_content name="x">…</skill_content>`). Compaction carries these
/// blocks forward verbatim instead of summarising them: a summary of "how
/// to do X" loses exactly the detail the skill exists to provide.
pub const SKILL_CONTENT_OPEN: &str = "<skill_content name=\"";
pub const SKILL_CONTENT_CLOSE: &str = "</skill_content>";

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_iterations: usize,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Number of trailing messages always kept verbatim through compaction.
    pub compaction_keep_last: usize,
    pub retry: RetryPolicy,
    /// Failed checks a run gets to fix before it stops (see [`Verifier`]).
    pub max_verify_rounds: usize,
    /// What happens when the context passes the profile's trigger.
    pub overflow: ContextOverflow,
    /// Watch for a run going in circles, nudge once, then stop it.
    pub detect_stuck: bool,
    /// At the compaction trigger, tool results older than the verbatim
    /// tail and longer than this (chars) are shortened to a preview and a
    /// `search_history` reference before anything is summarized. Only
    /// when the agent has a transcript and the `search_history` tool.
    pub shorten_tool_results_over: usize,
    /// M27: at most this many read-only tool calls from one response run
    /// at the same time. 1 runs every call one after another, as before.
    pub parallel_tools: usize,
}

/// What the loop does when the context outgrows the profile's trigger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextOverflow {
    /// Fold the older part into a structured summary, the request kept
    /// verbatim: ferrule's way.
    #[default]
    Compact,
    /// Drop the oldest messages until it fits, the system prompt kept: what
    /// a harness without context management does. `ferrule eval` uses it for
    /// its naive variant; nothing else should.
    Truncate,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 60,
            max_output_tokens: None,
            temperature: None,
            compaction_keep_last: 6,
            retry: RetryPolicy::default(),
            max_verify_rounds: 3,
            overflow: ContextOverflow::Compact,
            detect_stuck: true,
            shorten_tool_results_over: 4_000,
            parallel_tools: 4,
        }
    }
}

/// How a provider call that failed transiently (`CoreError::Transient`) is
/// retried: exponential backoff with jitter, or the server's `Retry-After`
/// when it gives one, all within one time budget.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Tries per call, the first included. 1 turns retrying off.
    pub max_attempts: u32,
    pub base_delay: Duration,
    /// Cap on the computed backoff. A server's `Retry-After` is only held
    /// to the budget.
    pub max_delay: Duration,
    /// No retry that would start later than this after the first try.
    pub budget: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(30),
            budget: Duration::from_secs(120),
        }
    }
}

impl RetryPolicy {
    /// The wait before trying again after `attempt` (1-based) failed with
    /// `err`, `elapsed` after the first try started; `None` to give up.
    pub fn delay(&self, err: &CoreError, attempt: u32, elapsed: Duration) -> Option<Duration> {
        let CoreError::Transient { retry_after, .. } = err else {
            return None;
        };
        if attempt >= self.max_attempts {
            return None;
        }
        let delay = match retry_after {
            Some(d) => *d,
            None => jitter(
                self.base_delay
                    .saturating_mul(1 << (attempt - 1).min(16))
                    .min(self.max_delay),
            ),
        };
        (elapsed + delay <= self.budget).then_some(delay)
    }
}

/// Somewhere in the upper half of `d`, so sessions that failed together
/// don't all come back at the same instant.
fn jitter(d: Duration) -> Duration {
    use std::hash::BuildHasher;
    // Each `RandomState` is freshly keyed: a random number without a crate.
    let r = std::collections::hash_map::RandomState::new().hash_one(0u8);
    let half = d / 2;
    half + Duration::from_nanos(r % (half.as_nanos() as u64 + 1))
}

/// Why a run ended before the model said it was done.
#[derive(Debug, Clone)]
enum StopReason {
    MaxIterations(usize),
    Stuck(Stuck),
    VerifyFailing {
        check: String,
        rounds: usize,
    },
    /// A [`Budget`] said nothing more may be spent.
    Budget(String),
    /// Stop hooks kept sending the run back past `max_stop_blocks`.
    StopHook {
        hook: String,
        rounds: usize,
    },
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::MaxIterations(n) => write!(
                f,
                "it reached the limit of {n} step{}",
                if *n == 1 { "" } else { "s" }
            ),
            StopReason::Stuck(stuck) => f.write_str(&stuck.reason()),
            StopReason::VerifyFailing { check, rounds } => {
                let rounds = if *rounds == 1 {
                    "one round".to_string()
                } else {
                    format!("{rounds} rounds")
                };
                write!(f, "`{check}` still fails after {rounds} of fixes")
            }
            StopReason::Budget(why) => f.write_str(why),
            StopReason::StopHook { hook, rounds } => write!(
                f,
                "the Stop hook `{hook}` still blocks after {rounds} tr{}",
                if *rounds == 1 { "y" } else { "ies" }
            ),
        }
    }
}

impl StopReason {
    fn into_error(self) -> CoreError {
        match self {
            StopReason::MaxIterations(n) => CoreError::MaxIterations(n),
            other => CoreError::Stopped(other.to_string()),
        }
    }
}

/// A call past the gate (M27): its hook input, what PreToolUse said, and
/// the answer already settled when the guard refused it or a hook blocked
/// it.
struct Gated {
    input: HookInput,
    pre: Fired,
    settled: Option<String>,
}

/// A call's answer: the text, whether it worked, whether the tool was
/// reached (PostToolUse follows only then), and how long the tool took.
struct Ran {
    raw: String,
    ok: bool,
    reached: bool,
    elapsed: Duration,
    /// When it finished, for the trace (M33).
    finished: SystemTime,
}

impl Ran {
    fn settled(raw: &str) -> Self {
        Self {
            raw: raw.to_string(),
            ok: false,
            reached: false,
            elapsed: Duration::ZERO,
            finished: SystemTime::now(),
        }
    }

    fn of(result: Result<ToolOutput, CoreError>, elapsed: Duration) -> Self {
        let (raw, ok) = match result {
            Ok(out) => (out.content, true),
            Err(e) => (format!("error: {e}"), false),
        };
        Self {
            raw,
            ok,
            reached: true,
            elapsed,
            finished: SystemTime::now(),
        }
    }
}

/// What cut a tool segment short.
enum Interrupt {
    Halted(String),
    Stopped,
}

/// A tool result with the PreToolUse hooks' note after it.
fn with_pre_note(raw: &str, pre: &Fired) -> String {
    match &pre.context {
        Some(note) => format!("{raw}\n\n[hook: PreToolUse] {note}"),
        None => raw.to_string(),
    }
}

/// One serialized agent run loop. Construct one per session; do not drive it
/// concurrently — sessions are serialized by the caller (session lane).
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    profile: HarnessProfile,
    config: AgentConfig,
    tool_ctx: ToolContext,
    transcript: Option<Transcript>,
    ledger: Option<LedgerContext>,
    /// Lifecycle hooks, the built-in check among them (see
    /// [`crate::lifecycle`]).
    hooks: HookSet,
    /// SessionStart fired (it fires once per agent).
    started: bool,
    budget: Option<Arc<dyn Budget>>,
    inbox: Option<Arc<dyn Inbox>>,
    stop: Option<StopFlag>,
    guard: Option<Arc<dyn Guard>>,
    session_recall: Option<Arc<dyn SessionRecall>>,
    /// Session-start recall ran (it runs once per agent).
    recalled: bool,
    /// What it recalled (M27: a user message after the goal, so the
    /// system prompt stays byte-stable for the cache). Compaction carries
    /// it forward verbatim, as it does the goal.
    memory: Option<String>,
    /// The recalled block still has to go in after this run's goal.
    memory_due: bool,
    /// Asked at the start of every run (M29: the repo map), and the last
    /// block it added.
    turn_context: Option<Arc<dyn TurnContext>>,
    turn_context_last: Option<String>,
    run_observers: Vec<Arc<dyn RunObserver>>,
    /// What the current run was asked to do: kept verbatim through
    /// compaction, since it's what says when the work is done.
    goal: Option<String>,
    pub messages: Vec<Message>,
    pub usage: Usage,
    /// Set when the last run stopped before finishing (the step limit, a
    /// loop, a check that kept failing): why. Its answer is then a status,
    /// not a result.
    pub incomplete: Option<String>,
    /// M27 timings waiting for the next ledger row.
    speed: Arc<std::sync::Mutex<SpeedStats>>,
    /// What the ledger sink wants traced this run (M33), asked at its
    /// start.
    trace: TraceLevel,
    /// Where the answer streams while it's written (M27), if anywhere.
    reply_stream: Option<DeltaSink>,
    /// When the current run started, and whether its first visible text
    /// has been timed yet.
    run_started: Instant,
    shown: Arc<std::sync::atomic::AtomicBool>,
    /// M28: keyword-triggered skills, asked only about a person's message.
    triggers: Option<Arc<dyn PromptTriggers>>,
    /// The current run's goal is a person's message ([`Agent::run_user`]).
    from_person: bool,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        profile: HarnessProfile,
        config: AgentConfig,
        tool_ctx: ToolContext,
        transcript: Option<Transcript>,
    ) -> Self {
        Self {
            provider,
            tools,
            profile,
            config,
            tool_ctx,
            transcript,
            ledger: None,
            trace: TraceLevel::Off,
            hooks: HookSet::default(),
            started: false,
            budget: None,
            inbox: None,
            stop: None,
            guard: None,
            session_recall: None,
            recalled: false,
            memory: None,
            memory_due: false,
            turn_context: None,
            turn_context_last: None,
            run_observers: Vec::new(),
            goal: None,
            messages: Vec::new(),
            usage: Usage::default(),
            incomplete: None,
            speed: Default::default(),
            reply_stream: None,
            run_started: Instant::now(),
            shown: Default::default(),
            triggers: None,
            from_person: false,
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.messages.push(Message::system(prompt.into()));
        self
    }

    /// Attach a per-call ledger sink (Phase 0, see `crate::ledger`). Default
    /// is no sink — existing callers/tests are unaffected. `task_shape` and
    /// `origin` identify *why* this session exists (`"run"`, `"chat"`,
    /// `"gateway"` + channel, `"scheduler"` + task id); `model` is supplied
    /// by the caller because `Provider` exposes only `name()`, not a model.
    pub fn with_ledger(
        mut self,
        sink: Arc<dyn LedgerSink>,
        task_shape: impl Into<String>,
        origin: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        self.ledger = Some(LedgerContext {
            sink,
            task_shape: task_shape.into(),
            origin,
            model: model.into(),
        });
        self
    }

    /// Check the work before a run that changed files may finish; see
    /// [`Verifier`]. It's the built-in Stop hook, ahead of every other.
    pub fn with_verifier(mut self, verifier: Arc<dyn Verifier>) -> Self {
        self.hooks.add_check(verifier);
        self
    }

    /// Fire `hooks` at their lifecycle events; see [`crate::lifecycle`].
    pub fn with_hooks(mut self, hooks: HookSet) -> Self {
        self.add_hooks(hooks);
        self
    }

    /// [`Agent::with_hooks`] after construction.
    pub fn add_hooks(&mut self, hooks: HookSet) {
        self.hooks.merge(hooks);
    }

    pub fn hooks(&self) -> &HookSet {
        &self.hooks
    }

    /// Charge every provider call to `budget` and stop, with a status
    /// answer, once it's spent; see [`Budget`].
    pub fn with_budget(mut self, budget: Arc<dyn Budget>) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Deliver messages that arrive mid-run before the next model call; see
    /// [`Inbox`].
    pub fn with_inbox(mut self, inbox: Arc<dyn Inbox>) -> Self {
        self.inbox = Some(inbox);
        self
    }

    /// Stop at the next step once `flag` is set; see [`StopFlag`].
    pub fn with_stop_flag(mut self, flag: StopFlag) -> Self {
        self.stop = Some(flag);
        self
    }

    /// Ask `recall` for long-term memory about the goal at the start of the
    /// first run; see [`SessionRecall`].
    /// The owner's guard (M19): caps, the kill switch, approval gates and
    /// plan mode. Its own slot, so a supervisor's `with_budget` can't
    /// replace it.
    pub fn with_guard(mut self, guard: Arc<dyn Guard>) -> Self {
        self.guard = Some(guard);
        self
    }

    /// The owner's guard, if any.
    pub fn guard(&self) -> Option<Arc<dyn Guard>> {
        self.guard.clone()
    }

    /// Stream the answer into `sink` while the model writes it (M27): the
    /// text of each `turn` and `status` call, a [`Delta::Reset`] before
    /// each call and each retry. Compaction and other side calls never
    /// stream. `None` turns it off.
    pub fn set_reply_stream(&mut self, sink: Option<DeltaSink>) {
        self.reply_stream = sink;
    }

    /// Replaces the guard (the gateway puts a turn deadline in front of
    /// the owner's).
    pub fn set_guard(&mut self, guard: Arc<dyn Guard>) {
        self.guard = Some(guard);
    }

    /// M28: skills whose triggers a person's message names load with it;
    /// see [`PromptTriggers`] and [`Agent::run_user`].
    pub fn with_prompt_triggers(mut self, triggers: Arc<dyn PromptTriggers>) -> Self {
        self.triggers = Some(triggers);
        self
    }

    pub fn with_session_recall(mut self, recall: Arc<dyn SessionRecall>) -> Self {
        self.session_recall = Some(recall);
        self
    }

    /// Ask `context` for a block at the start of every run; see
    /// [`TurnContext`].
    pub fn with_turn_context(mut self, context: Arc<dyn TurnContext>) -> Self {
        self.turn_context = Some(context);
        self
    }

    /// Adds a [`RunObserver`], called around every run.
    pub fn with_run_observer(mut self, observer: Arc<dyn RunObserver>) -> Self {
        self.run_observers.push(observer);
        self
    }

    /// Adds a tool after construction (tools bound to this agent's identity
    /// are made once its id is known).
    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        self.tools.register(tool);
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains(name)
    }

    /// Appends to the system prompt (adding one if there is none). Not
    /// written to the transcript: it's rebuilt with the agent each time.
    pub fn append_system_prompt(&mut self, text: &str) {
        match self.messages.first_mut() {
            Some(m) if m.role == crate::message::Role::System => {
                let body = m.content.get_or_insert_with(String::new);
                if !body.is_empty() {
                    body.push_str("\n\n");
                }
                body.push_str(text);
            }
            _ => self.messages.insert(0, Message::system(text.to_string())),
        }
    }

    fn stopped(&self) -> bool {
        self.stop.as_ref().is_some_and(StopFlag::is_set)
    }

    async fn emit(&self, tx: &mpsc::Sender<AgentEvent>, ev: AgentEvent) {
        let _ = tx.send(ev).await; // receiver may be detached in headless mode
    }

    fn est_context_tokens(&self) -> usize {
        self.messages.iter().map(|m| m.est_tokens()).sum()
    }

    /// The ledger, when its sink wants trace events this run.
    fn traced(&self) -> Option<&LedgerContext> {
        self.ledger
            .as_ref()
            .filter(|_| self.trace != TraceLevel::Off)
    }

    fn trace_tool(
        &self,
        call: &ToolCall,
        ok: bool,
        elapsed: Duration,
        finished: SystemTime,
        raw: &str,
    ) {
        let Some(ledger) = self.traced() else { return };
        let content = self.trace == TraceLevel::Content;
        ledger.sink.trace(TraceEvent::ToolCall {
            session_id: self.session_id(),
            id: call.id.clone(),
            name: call.name.clone(),
            ok,
            started: finished.checked_sub(elapsed).unwrap_or(finished),
            elapsed,
            arguments: content.then(|| call.arguments.to_string()),
            result: content.then(|| raw.to_string()),
        });
    }

    fn session_id(&self) -> String {
        self.transcript
            .as_ref()
            .and_then(|t| {
                t.path()
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "ephemeral".into())
    }

    /// A hook payload's common part for this agent.
    fn hook_input(&self) -> HookInput {
        self.hooks.input(
            &self.session_id(),
            self.transcript.as_ref().map(|t| t.path().to_path_buf()),
            self.tool_ctx.workspace.clone(),
        )
    }

    /// Fires `event`'s hooks (not the built-in check) and shows the owner
    /// each run.
    async fn fire(
        &self,
        tx: &mpsc::Sender<AgentEvent>,
        event: HookEvent,
        input: HookInput,
    ) -> Fired {
        if !self.hooks.has(event) {
            return Fired::default();
        }
        let fired = self.hooks.fire(event, input, &self.tool_ctx).await;
        for r in &fired.reports {
            if let Some(error) = &r.error {
                warn!(event = %r.event, command = %r.command, "hook failed: {error}");
            }
            self.emit(
                tx,
                AgentEvent::HookFinished {
                    event: r.event.name().to_string(),
                    source: r.source.name().to_string(),
                    command: r.command.clone(),
                    blocked: r.blocked,
                    exit_code: r.exit_code,
                    duration_ms: r.duration.as_millis() as u64,
                    error: r.error.clone(),
                },
            )
            .await;
        }
        fired
    }

    /// Runs the built-in checks in order: the first that fails, with its
    /// output. They keep the verify events of before hooks.
    async fn run_checks(&self, tx: &mpsc::Sender<AgentEvent>) -> Option<(String, String)> {
        let checks: Vec<Hook> = self
            .hooks
            .hooks()
            .iter()
            .filter(|h| h.check)
            .cloned()
            .collect();
        let mut input = self.hook_input();
        input.files_changed = Some(true);
        for (i, hook) in checks.iter().enumerate() {
            let check = hook.command();
            self.emit(
                tx,
                AgentEvent::VerifyStarted {
                    check: check.clone(),
                },
            )
            .await;
            let (verdict, _) = self.hooks.run_hook(hook, &input, &self.tool_ctx).await;
            let failed = match verdict {
                Verdict::Block { reason } => Some(reason),
                _ => None,
            };
            self.emit(
                tx,
                AgentEvent::VerifyFinished {
                    check: check.clone(),
                    ok: failed.is_none(),
                },
            )
            .await;
            if let Some(output) = failed {
                self.hooks.skip(&checks[i + 1..], &input);
                return Some((check, output));
            }
        }
        None
    }

    /// Fires SessionEnd: `ferrule run` finished or `ferrule chat` exited.
    pub async fn end_session(&self, reason: &str, tx: &mpsc::Sender<AgentEvent>) {
        let mut input = self.hook_input();
        input.reason = Some(reason.to_string());
        self.fire(tx, HookEvent::SessionEnd, input).await;
    }

    /// Time a `provider.complete()` call and, if a ledger sink is attached,
    /// emit one row for it — success or failure. This is the only place
    /// that writes to the ledger; both the main loop and the compaction
    /// summary call go through it.
    ///
    /// A transient failure is tried again per `config.retry`; every attempt
    /// gets its own row, the ones that led to a retry marked `"retried"`.
    async fn call_provider(
        &self,
        tx: &mpsc::Sender<AgentEvent>,
        req: CompletionRequest,
        iteration: usize,
        call_kind: &str,
    ) -> Result<CompletionResponse, CoreError> {
        let mut first = Instant::now();
        let mut attempt = 1;
        // A provider marks each failed model down, so this ends anyway;
        // the cap guards against one that doesn't.
        let mut fallbacks = 0;
        loop {
            let start = Instant::now();
            let mut req = req.clone();
            if let (Some(sink), "turn" | "status") = (&self.reply_stream, call_kind) {
                sink.send(Delta::Reset);
                req.stream = Some(self.timed(sink.clone(), start));
            }
            let (served, result) = self.provider.complete_routed(req).await;
            let latency_ms = start.elapsed().as_millis() as u64;
            let retry_in = match &result {
                Err(e) => self.config.retry.delay(e, attempt, first.elapsed()),
                Ok(_) => None,
            };
            // Out of retries on an outage: the next model in the owner's
            // fallback list, if the provider has one, from a fresh start.
            let fell_over = match (&result, retry_in) {
                (Err(e @ CoreError::Transient { .. }), None) if fallbacks < 8 => {
                    self.provider.fail_over(served.as_ref(), e)
                }
                _ => None,
            };
            // M25: a failure a stronger model may not repeat moves the turn
            // up a tier, and the same request goes again there.
            let route = self.provider.route_tag();
            let escalated = match (&result, retry_in, &fell_over) {
                (Err(e), None, None) if self.provider.routes() => crate::routing::escalates_on(e)
                    .and_then(|class| self.provider.escalate(&Signal::CallFailed(class))),
                _ => None,
            };
            self.record_completion(
                iteration,
                call_kind,
                latency_ms,
                &result,
                retry_in.is_some() || fell_over.is_some() || escalated.is_some(),
                served.as_ref(),
                route,
            );
            if let (Some(budget), Ok(resp)) = (&self.budget, &result) {
                budget.charge(&resp.usage);
            }
            if let (Some(to), Err(e)) = (fell_over, &result) {
                warn!(from = %to.from, to = %to.to, "model failing, falling back: {e}");
                self.emit(
                    tx,
                    AgentEvent::ModelFallback {
                        from: to.from,
                        to: to.to,
                        error: e.to_string(),
                    },
                )
                .await;
                first = Instant::now();
                attempt = 1;
                fallbacks += 1;
                continue;
            }
            if let Some(up) = escalated {
                self.emit_escalation(tx, up).await;
                first = Instant::now();
                attempt = 1;
                continue;
            }
            let (Some(delay), Err(e)) = (retry_in, &result) else {
                return result.map_err(|e| e.after_attempts(attempt));
            };
            warn!(
                attempt,
                delay_ms = delay.as_millis() as u64,
                "provider call failed, retrying: {e}"
            );
            self.emit(
                tx,
                AgentEvent::ProviderRetry {
                    attempt,
                    max_attempts: self.config.retry.max_attempts,
                    delay_ms: delay.as_millis() as u64,
                    error: e.to_string(),
                },
            )
            .await;
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    /// `sink`, timing the call's first streamed byte and the run's first
    /// visible text into the next ledger row.
    fn timed(&self, sink: DeltaSink, call_start: Instant) -> DeltaSink {
        let speed = self.speed.clone();
        let shown = self.shown.clone();
        let run_start = self.run_started;
        let ms = |since: Instant| since.elapsed().as_millis() as u64;
        DeltaSink::new(move |delta| {
            {
                let mut s = speed.lock().unwrap();
                s.first_token_ms.get_or_insert_with(|| ms(call_start));
                if matches!(&delta, Delta::Text(t) if !t.is_empty())
                    && !shown.swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    s.first_visible_ms = Some(ms(run_start));
                }
            }
            sink.send(delta)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn record_completion(
        &self,
        iteration: usize,
        call_kind: &str,
        latency_ms: u64,
        result: &Result<CompletionResponse, CoreError>,
        retried: bool,
        served: Option<&crate::provider::Served>,
        route: Option<crate::routing::RouteTag>,
    ) {
        let Some(ledger) = &self.ledger else { return };
        if let (TraceLevel::Content, Ok(resp)) = (self.trace, result) {
            let mut text = resp.message.content.clone().unwrap_or_default();
            for call in &resp.message.tool_calls {
                text.push_str(&format!("\n[tool call: {}]", call.name));
            }
            ledger.sink.trace(TraceEvent::CallContent {
                session_id: self.session_id(),
                text,
            });
        }
        let (
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            tool_calls,
            outcome,
            error_kind,
            error_message,
        ) = match result {
            Ok(resp) => (
                resp.usage.input_tokens,
                resp.usage.cached_input_tokens,
                resp.usage.cache_write_input_tokens,
                resp.usage.output_tokens,
                resp.message.tool_calls.len(),
                "ok".to_string(),
                None,
                None,
            ),
            Err(e) => {
                let outcome = if retried { "retried" } else { "error" };
                (
                    0,
                    0,
                    0,
                    0,
                    0,
                    outcome.to_string(),
                    Some(error_kind_of(e)),
                    Some(truncate_error(&e.to_string())),
                )
            }
        };
        let record = LedgerRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            session_id: self.session_id(),
            task_shape: ledger.task_shape.clone(),
            origin: ledger.origin.clone(),
            provider: served
                .map(|s| s.provider.clone())
                .unwrap_or_else(|| self.provider.name().to_string()),
            model: served
                .map(|s| s.model.clone())
                .unwrap_or_else(|| ledger.model.clone()),
            iteration,
            call_kind: call_kind.to_string(),
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            tool_calls,
            latency_ms,
            outcome,
            error_kind,
            error_message,
            cost_usd: None,
            eval: None,
            tree: None,
            route,
            speed: Some(std::mem::take(&mut *self.speed.lock().unwrap())).filter(|s| !s.is_empty()),
        };
        ledger.sink.record(record);
    }

    /// Tell the provider about `signal`; if it moved up a tier, say so.
    /// A no-op unless the provider routes.
    async fn signal(&self, tx: &mpsc::Sender<AgentEvent>, signal: crate::routing::Signal) -> bool {
        if !self.provider.routes() {
            return false;
        }
        let Some(up) = self.provider.escalate(&signal) else {
            return false;
        };
        self.emit_escalation(tx, up).await;
        true
    }

    async fn emit_escalation(&self, tx: &mpsc::Sender<AgentEvent>, up: crate::routing::Escalation) {
        info!(from = %up.from, to = %up.to, reason = %up.reason, "routing: escalating");
        self.emit(
            tx,
            AgentEvent::Escalated {
                from: up.from,
                to: up.to,
                reason: up.reason,
            },
        )
        .await;
    }

    /// The ReAct loop: call → tool calls → observe → repeat until text-only.
    ///
    /// A run that can't finish still answers: at the step limit, in a loop
    /// it was already warned about, or with a check that keeps failing, it
    /// stops with a status of where things stand ([`Agent::incomplete`]
    /// says why) instead of an error.
    pub async fn run(
        &mut self,
        goal: &str,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<String, CoreError> {
        let inbox = self.inbox.clone();
        if let Some(inbox) = &inbox {
            inbox.begin();
        }
        if let Some(guard) = &self.guard {
            guard.begin();
        }
        self.provider.begin_turn();
        self.run_started = Instant::now();
        self.shown
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let observers = self.run_observers.clone();
        let session_id = self.session_id();
        for o in &observers {
            o.begin(&session_id).await;
        }
        self.trace = self
            .ledger
            .as_ref()
            .map_or(TraceLevel::Off, |l| l.sink.trace_level());
        if let Some(ledger) = self.traced() {
            ledger.sink.trace(TraceEvent::TurnStarted {
                session_id: session_id.clone(),
                task_shape: ledger.task_shape.clone(),
                origin: ledger.origin.clone(),
                at: SystemTime::now(),
                goal: (self.trace == TraceLevel::Content).then(|| goal.to_string()),
            });
        }
        let result = self.run_inner(goal, tx.clone()).await;
        if let Some(ledger) = self.traced() {
            ledger.sink.trace(TraceEvent::TurnFinished {
                session_id: session_id.clone(),
                at: SystemTime::now(),
                ok: result.is_ok(),
                incomplete: self.incomplete.clone(),
            });
        }
        if let Some(inbox) = &inbox {
            inbox.end();
        }
        for o in &observers {
            let end = RunEnd {
                session_id: &session_id,
                goal,
                answer: result.as_deref().ok(),
                incomplete: self.incomplete.as_deref(),
            };
            if let Some(text) = o.end(&end).await {
                self.emit(
                    &tx,
                    AgentEvent::Notice {
                        source: o.name().to_string(),
                        text,
                    },
                )
                .await;
            }
        }
        result
    }

    /// [`Agent::run`] for a message a person typed: the one kind of text
    /// that may trigger skills (M28). Everything else — a sub-agent's
    /// news, a scheduled prompt, a plan to carry out — goes through `run`.
    pub async fn run_user(
        &mut self,
        goal: &str,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<String, CoreError> {
        self.from_person = true;
        self.run(goal, tx).await
    }

    async fn run_inner(
        &mut self,
        goal: &str,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<String, CoreError> {
        let from_person = std::mem::take(&mut self.from_person);
        let session_id = self.session_id();
        self.emit(
            &tx,
            AgentEvent::RunStarted {
                session_id,
                goal: goal.into(),
            },
        )
        .await;
        self.recall_for(goal).await;
        let session_note = if self.started {
            None
        } else {
            self.started = true;
            let resumed = self
                .messages
                .iter()
                .any(|m| m.role == crate::message::Role::User);
            let mut input = self.hook_input();
            input.source = Some(if resumed { "resume" } else { "startup" }.into());
            self.fire(&tx, HookEvent::SessionStart, input).await.context
        };
        let mut input = self.hook_input();
        input.prompt = Some(goal.to_string());
        let submitted = self.fire(&tx, HookEvent::UserPromptSubmit, input).await;
        if let Some((_, reason)) = submitted.block {
            // The request never enters the history.
            if let Some(note) = session_note {
                self.push(Message::user(format!("[hook: SessionStart]\n{note}")));
            }
            let why = format!("a UserPromptSubmit hook blocked the request: {reason}");
            warn!(%why, "not running the request");
            self.incomplete = Some(why.clone());
            let answer = format!("Blocked by a hook: {reason}");
            self.emit(
                &tx,
                AgentEvent::AssistantText {
                    text: answer.clone(),
                },
            )
            .await;
            self.emit(
                &tx,
                AgentEvent::RunIncomplete {
                    reason: why,
                    iterations: 0,
                },
            )
            .await;
            return Ok(answer);
        }
        self.goal = Some(goal.to_string());
        self.incomplete = None;
        self.push(Message::user(goal));
        if std::mem::take(&mut self.memory_due) {
            if let Some(memory) = self.memory.clone() {
                self.push(Message::user(memory));
            }
        }
        self.add_turn_context(goal).await;
        for (event, note) in [
            (HookEvent::SessionStart, session_note),
            (HookEvent::UserPromptSubmit, submitted.context),
        ] {
            if let Some(note) = note {
                self.push(Message::user(format!("[hook: {event}]\n{note}")));
            }
        }
        if from_person {
            self.load_triggered(goal, &tx).await;
        }

        let mut steps: Vec<Step> = Vec::new();
        let mut nudged = false;
        // A tool that changes files succeeded, so the check has to pass
        // before the run may finish.
        let mut unverified = false;
        // Files changed since the check last passed (a Stop hook can send
        // the model back after a pass; the check reruns only on new edits).
        let mut needs_check = false;
        let mut failed_checks = 0;
        // Times Stop hooks sent this run back, and whether the last finish
        // was sent back by any (the payload's `stop_hook_active`).
        let mut stop_blocks = 0;
        let mut sent_back = false;
        // M25, kept only when the provider routes: invalid tool calls in a
        // row, and the same call repeated in a row.
        let routes = self.provider.routes();
        let mut misfits = 0;
        let mut repeats: (Option<(String, String)>, u32) = (None, 0);

        for iteration in 0..self.config.max_iterations {
            if self.stopped() {
                return Err(CoreError::Aborted("the agent was stopped".into()));
            }
            if let Some(why) = self.guard.as_ref().and_then(|g| g.before_model_call()) {
                return Ok(self.halt(&tx, iteration, why).await);
            }
            if let Some(why) = self.budget.as_ref().and_then(|b| b.exhausted()) {
                return self.wrap_up(&tx, iteration, StopReason::Budget(why)).await;
            }
            self.deliver_inbox();
            self.maybe_compact(&tx, iteration).await?;
            let guard = self.guard.clone();
            let call = self.call_provider(&tx, self.request(), iteration, "turn");
            let resp = match unless_halted(guard.as_ref(), call).await {
                Ok(resp) => resp?,
                Err(why) => return Ok(self.halt(&tx, iteration, why).await),
            };
            self.add_usage(&tx, &resp.usage).await;

            let msg = resp.message;
            if let Some(text) = &msg.content {
                if !text.is_empty() {
                    self.emit(&tx, AgentEvent::AssistantText { text: text.clone() })
                        .await;
                }
            }
            if let Some(r) = &msg.reasoning {
                if !r.is_empty() {
                    self.emit(&tx, AgentEvent::Reasoning { text: r.clone() })
                        .await;
                }
            }

            let finished = msg.tool_calls.is_empty();
            self.push(msg.clone());

            if finished {
                if needs_check {
                    // The kill switch reaches a hanging check too (M19 §13).
                    let checked = match unless_halted(guard.as_ref(), self.run_checks(&tx)).await {
                        Ok(checked) => checked,
                        Err(why) => return Ok(self.halt(&tx, iteration + 1, why).await),
                    };
                    if let Some((check, output)) = checked {
                        failed_checks += 1;
                        if failed_checks > self.config.max_verify_rounds {
                            let reason = StopReason::VerifyFailing {
                                check,
                                rounds: self.config.max_verify_rounds,
                            };
                            return self.wrap_up(&tx, iteration + 1, reason).await;
                        }
                        sent_back = true;
                        self.push(Message::user(format!(
                            "[ferrule] `{check}` fails, so this isn't done yet. Fix what it reports, then finish again; \
                             it runs again when you do.\n\n{output}"
                        )));
                        self.signal(&tx, Signal::CheckFailed).await;
                        continue;
                    }
                    needs_check = false;
                }
                let mut input = self.hook_input();
                input.stop_hook_active = Some(sent_back);
                input.files_changed = Some(unverified);
                input.last_assistant_message = msg.content.clone();
                let stop = self.fire(&tx, HookEvent::Stop, input);
                let stop = match unless_halted(guard.as_ref(), stop).await {
                    Ok(stop) => stop,
                    Err(why) => return Ok(self.halt(&tx, iteration + 1, why).await),
                };
                // Sent back, the run's next model call is asked about
                // first, like any other: a Stop hook can't outrun the caps.
                if let Some((hook, reason)) = stop.block {
                    stop_blocks += 1;
                    let rounds = self.hooks.limits.max_stop_blocks;
                    if stop_blocks > rounds {
                        let reason = StopReason::StopHook { hook, rounds };
                        return self.wrap_up(&tx, iteration + 1, reason).await;
                    }
                    sent_back = true;
                    self.push(Message::user(format!(
                        "[hook: Stop] This isn't done yet:\n\n{reason}"
                    )));
                    self.signal(&tx, Signal::StopHook).await;
                    continue;
                }
                let answer = msg.content.unwrap_or_default();
                self.emit(
                    &tx,
                    AgentEvent::RunFinished {
                        answer_chars: answer.len(),
                        iterations: iteration + 1,
                    },
                )
                .await;
                return Ok(answer);
            }

            // M27: a run of read-only calls goes through the gate one by
            // one, runs side by side, and is finished one by one in the
            // order asked; every other call is a segment of its own, run as
            // before (docs/m27-speed.md §1).
            let calls = &msg.tool_calls;
            let batch_started = Instant::now();
            let mut batch = ToolBatch {
                calls: calls.len(),
                ..Default::default()
            };
            let g = guard.as_ref();
            let mut at = 0;
            while at < calls.len() {
                let end = self.segment_end(calls, at);
                let seg = &calls[at..end];
                let parallel = seg.len() > 1;

                // The gate: the owner's guard, then the PreToolUse hooks,
                // each raced against a halt (docs/m19-trust-cost.md §13).
                // A hook never sees a call the guard refused, and has
                // nothing to approve with.
                let mut gated: Vec<Gated> = Vec::with_capacity(seg.len());
                for call in seg {
                    if self.stopped() {
                        // Every call needs a result, or the history can't
                        // be sent again when the agent is resumed.
                        self.unrun(&tx, &seg[..gated.len()]).await;
                        for skipped in &calls[at..] {
                            self.push(Message::tool_result(
                                &skipped.id,
                                "not run: the agent was stopped",
                            ));
                        }
                        return Err(CoreError::Aborted("the agent was stopped".into()));
                    }
                    match self.gate(&tx, g, call).await {
                        Ok(passed) => gated.push(passed),
                        Err(why) => {
                            self.unrun(&tx, &seg[..=gated.len()]).await;
                            for skipped in &calls[at..] {
                                self.push(Message::tool_result(
                                    &skipped.id,
                                    "not run: ferrule halted the run",
                                ));
                            }
                            return Ok(self.halt(&tx, iteration + 1, why).await);
                        }
                    }
                }

                let (ran, interrupted) = if parallel {
                    self.run_side_by_side(g, seg, &gated).await
                } else {
                    self.run_one(g, &seg[0], &gated[0]).await
                };
                batch.sum_ms += ran
                    .iter()
                    .flatten()
                    .map(|r| r.elapsed.as_millis() as u64)
                    .sum::<u64>();
                if parallel {
                    batch.parallel += seg.len();
                }

                if let Some(interrupt) = interrupted {
                    // Halted or stopped mid-run: what finished keeps its
                    // result, the rest is marked not run.
                    let note = match &interrupt {
                        Interrupt::Halted(_) => "not run: ferrule halted the run",
                        Interrupt::Stopped => "not run: the agent was stopped",
                    };
                    for ((call, passed), done) in seg.iter().zip(&gated).zip(&ran) {
                        let (content, ok) = match done {
                            Some(done) => (with_pre_note(&done.raw, &passed.pre), done.ok),
                            None => (note.to_string(), false),
                        };
                        self.emit(
                            &tx,
                            AgentEvent::ToolCallFinished {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                ok,
                                output_chars: if done.is_some() { content.len() } else { 0 },
                            },
                        )
                        .await;
                        self.push(Message::tool_result(&call.id, content));
                    }
                    for skipped in &calls[end..] {
                        self.push(Message::tool_result(&skipped.id, note));
                    }
                    return match interrupt {
                        Interrupt::Halted(why) => Ok(self.halt(&tx, iteration + 1, why).await),
                        Interrupt::Stopped => {
                            Err(CoreError::Aborted("the agent was stopped".into()))
                        }
                    };
                }

                for (k, (call, (passed, done))) in
                    seg.iter().zip(gated.into_iter().zip(ran)).enumerate()
                {
                    let Ran {
                        raw,
                        ok,
                        reached,
                        elapsed,
                        finished,
                    } = done.expect("every call has a result when not interrupted");
                    self.trace_tool(call, ok, elapsed, finished, &raw);
                    let Gated { mut input, pre, .. } = passed;
                    if !ok {
                        warn!(tool = %call.name, "tool call failed");
                    }
                    if ok && self.tools.changes_files(&call.name) {
                        unverified = true;
                        needs_check = true;
                    }
                    let mut content = with_pre_note(&raw, &pre);
                    // PostToolUse follows only a call that was dispatched:
                    // not one the guard refused or a hook blocked.
                    if reached {
                        input.tool_response = Some(serde_json::json!({"ok": ok, "content": raw}));
                        let post = self.fire(&tx, HookEvent::PostToolUse, input);
                        let post = match unless_halted(g, post).await {
                            Ok(post) => post,
                            Err(why) => {
                                self.emit(
                                    &tx,
                                    AgentEvent::ToolCallFinished {
                                        id: call.id.clone(),
                                        name: call.name.clone(),
                                        ok,
                                        output_chars: content.len(),
                                    },
                                )
                                .await;
                                self.push(Message::tool_result(&call.id, content));
                                // The rest of the segment ran, but its
                                // PostToolUse hooks didn't: not run, as a
                                // halt mid-call is.
                                self.unrun(&tx, &seg[k + 1..]).await;
                                for skipped in &calls[at + k + 1..] {
                                    self.push(Message::tool_result(
                                        &skipped.id,
                                        "not run: ferrule halted the run",
                                    ));
                                }
                                return Ok(self.halt(&tx, iteration + 1, why).await);
                            }
                        };
                        for note in post.context.iter().chain(post.block.iter().map(|b| &b.1)) {
                            content.push_str(&format!("\n\n[hook: PostToolUse] {note}"));
                        }
                    }
                    self.emit(
                        &tx,
                        AgentEvent::ToolCallFinished {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            ok,
                            output_chars: content.len(),
                        },
                    )
                    .await;

                    steps.push(Step::new(&call.name, &call.arguments, &raw, ok));
                    self.push(Message::tool_result(&call.id, content));
                    if routes {
                        misfits = if self.tools.misfit(&call.name, &call.arguments) {
                            misfits + 1
                        } else {
                            0
                        };
                        let key = (call.name.clone(), call.arguments.to_string());
                        repeats = match repeats {
                            (Some(last), n) if last == key => (Some(last), n + 1),
                            _ => (Some(key), 1),
                        };
                        // A move starts both counts again, so the next tier
                        // gets the same allowance.
                        let moved = (misfits > 0
                            && self.signal(&tx, Signal::ToolErrors(misfits)).await)
                            || self.signal(&tx, Signal::Repeated(repeats.1)).await;
                        if moved {
                            misfits = 0;
                            repeats.1 = 0;
                        }
                    }
                }
                at = end;
            }
            // A lone call's wall time is its own time: only a batch has
            // anything to compare.
            if batch.calls > 1 {
                batch.wall_ms = batch_started.elapsed().as_millis() as u64;
                self.speed.lock().unwrap().tool_batch = Some(batch);
            }

            if let Some(stuck) = Stuck::detect(&steps).filter(|_| self.config.detect_stuck) {
                if nudged {
                    return self
                        .wrap_up(&tx, iteration + 1, StopReason::Stuck(stuck))
                        .await;
                }
                // One warning first: told what it's repeating, a model
                // usually changes course.
                nudged = true;
                steps.clear();
                let note = stuck.nudge();
                warn!(?stuck, "the run is going in circles");
                self.emit(&tx, AgentEvent::Stuck { note: note.clone() })
                    .await;
                self.push(Message::user(note));
                self.signal(&tx, Signal::Stuck).await;
            }
        }
        let limit = self.config.max_iterations;
        self.wrap_up(&tx, limit, StopReason::MaxIterations(limit))
            .await
    }

    /// Ends a run that can't finish with one more call, for a status the
    /// person can act on: what got done, what's blocking, what's next. An
    /// error would throw all of that away.
    async fn wrap_up(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        iterations: usize,
        reason: StopReason,
    ) -> Result<String, CoreError> {
        if let Some(halt) = self.guard.as_ref().and_then(|g| g.before_model_call()) {
            return Ok(self.halt(tx, iterations, halt).await);
        }
        let why = reason.to_string();
        warn!(%why, "stopping the run before it finished");
        self.incomplete = Some(why.clone());
        self.push(Message::user(format!(
            "[ferrule] Stopping here: {why}. Don't call any tools. Write a short status for the person you are \
             working for, in their language: say first that you stopped before finishing and why, then what \
             you got done, what is blocking, and the next step or the question they need to answer."
        )));
        if let Err(e) = self.maybe_compact(tx, iterations).await {
            warn!("compaction before the status answer failed: {e}");
        }
        // The tools stay declared: some APIs refuse a history that holds
        // tool calls when the request has no tools.
        let resp = match self
            .call_provider(tx, self.request(), iterations, "status")
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                warn!("the status answer failed: {e}");
                return Err(reason.into_error());
            }
        };
        self.add_usage(tx, &resp.usage).await;
        let mut msg = resp.message;
        msg.tool_calls.clear();
        let answer = match msg.content.as_deref().map(str::trim) {
            Some(text) if !text.is_empty() => text.to_string(),
            _ => format!("Stopped before finishing: {why}."),
        };
        msg.content = Some(answer.clone());
        self.emit(
            tx,
            AgentEvent::AssistantText {
                text: answer.clone(),
            },
        )
        .await;
        self.push(msg);
        self.emit(
            tx,
            AgentEvent::RunIncomplete {
                reason: why,
                iterations,
            },
        )
        .await;
        Ok(answer)
    }

    /// Where the segment starting at `at` ends (exclusive): past every
    /// read-only call that follows, when there are two or more of them and
    /// parallel calls are on; one call otherwise.
    fn segment_end(&self, calls: &[ToolCall], at: usize) -> usize {
        if self.config.parallel_tools <= 1 || !self.tools.read_only(&calls[at].name) {
            return at + 1;
        }
        let mut end = at + 1;
        while end < calls.len() && self.tools.read_only(&calls[end].name) {
            end += 1;
        }
        end
    }

    /// One call through the gate: announced, asked about by the owner's
    /// guard, then by the PreToolUse hooks. `Err` is a halt.
    async fn gate(
        &self,
        tx: &mpsc::Sender<AgentEvent>,
        g: Option<&Arc<dyn Guard>>,
        call: &ToolCall,
    ) -> Result<Gated, String> {
        self.emit(
            tx,
            AgentEvent::ToolCallStarted {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
        )
        .await;
        let mut input = self.hook_input();
        input.tool_name = Some(call.name.clone());
        input.tool_input = Some(call.arguments.clone());
        input.tool_use_id = Some(call.id.clone());
        let seen = GuardedCall {
            tool: &call.name,
            args: &call.arguments,
            changes_files: self.tools.changes_files(&call.name),
        };
        let verdict = match g {
            Some(gd) => unless_halted(g, gd.before_tool_call(seen)).await?,
            None => GuardVerdict::Allow,
        };
        if let GuardVerdict::Refuse(why) = verdict {
            return Ok(Gated {
                input,
                pre: Fired::default(),
                settled: Some(format!("refused by ferrule: {why}")),
            });
        }
        let pre = unless_halted(g, self.fire(tx, HookEvent::PreToolUse, input.clone())).await?;
        let settled = pre
            .block
            .as_ref()
            .map(|(_, reason)| format!("error: not run: a PreToolUse hook blocked it: {reason}"));
        Ok(Gated {
            input,
            pre,
            settled,
        })
    }

    /// `ToolCallFinished` for calls announced but never answered.
    async fn unrun(&self, tx: &mpsc::Sender<AgentEvent>, calls: &[ToolCall]) {
        for call in calls {
            self.emit(
                tx,
                AgentEvent::ToolCallFinished {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    ok: false,
                    output_chars: 0,
                },
            )
            .await;
        }
    }

    /// One call, in place, as ferrule always ran them.
    async fn run_one(
        &self,
        g: Option<&Arc<dyn Guard>>,
        call: &ToolCall,
        passed: &Gated,
    ) -> (Vec<Option<Ran>>, Option<Interrupt>) {
        if let Some(raw) = &passed.settled {
            return (vec![Some(Ran::settled(raw))], None);
        }
        let started = Instant::now();
        let run = self
            .tools
            .call(&call.name, call.arguments.clone(), &self.tool_ctx);
        match unless_halted(g, run).await {
            Ok(result) => (vec![Some(Ran::of(result, started.elapsed()))], None),
            Err(why) => (vec![None], Some(Interrupt::Halted(why))),
        }
    }

    /// A segment of read-only calls, side by side: at most
    /// `parallel_tools` at once, one at a time per serial group, each in
    /// its own task. A halt or a stop ends the wait; what finished keeps
    /// its result and the rest is aborted.
    async fn run_side_by_side(
        &self,
        g: Option<&Arc<dyn Guard>>,
        seg: &[ToolCall],
        gated: &[Gated],
    ) -> (Vec<Option<Ran>>, Option<Interrupt>) {
        let mut ran: Vec<Option<Ran>> = (0..seg.len()).map(|_| None).collect();
        let slots = Arc::new(tokio::sync::Semaphore::new(
            self.config.parallel_tools.max(1),
        ));
        let mut groups: HashMap<String, Arc<tokio::sync::Mutex<()>>> = HashMap::new();
        let mut tasks = tokio::task::JoinSet::new();
        let mut which = HashMap::new();
        for (k, (call, passed)) in seg.iter().zip(gated).enumerate() {
            if let Some(raw) = &passed.settled {
                ran[k] = Some(Ran::settled(raw));
                continue;
            }
            let Some(tool) = self.tools.get(&call.name) else {
                let missing = CoreError::ToolNotFound(call.name.clone());
                ran[k] = Some(Ran::of(Err(missing), Duration::ZERO));
                continue;
            };
            let group = tool
                .serial_group()
                .map(|name| groups.entry(name).or_default().clone());
            let slots = slots.clone();
            let args = call.arguments.clone();
            let ctx = self.tool_ctx.clone();
            let handle = tasks.spawn(async move {
                let _turn = match group {
                    Some(lock) => Some(lock.lock_owned().await),
                    None => None,
                };
                let _slot = slots.acquire_owned().await;
                let started = Instant::now();
                let result = tool.call(args, &ctx).await;
                (k, result, started.elapsed())
            });
            which.insert(handle.id(), k);
        }

        let halted = async {
            match g {
                Some(gd) => gd.halted().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(halted);
        let poll = Duration::from_millis(25);
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + poll, poll);
        let settle =
            |ran: &mut Vec<Option<Ran>>, next: Result<_, tokio::task::JoinError>| match next {
                Ok((_, (k, result, took))) => ran[k] = Some(Ran::of(result, took)),
                Err(e) => {
                    let k = which[&e.id()];
                    warn!(tool = %seg[k].name, "tool call panicked");
                    ran[k] = Some(Ran {
                        raw: "error: the tool crashed".into(),
                        ok: false,
                        reached: true,
                        elapsed: Duration::ZERO,
                        finished: SystemTime::now(),
                    });
                }
            };
        let interrupt = loop {
            tokio::select! {
                biased;
                why = &mut halted => break Some(Interrupt::Halted(why)),
                _ = tick.tick() => {
                    if self.stopped() {
                        break Some(Interrupt::Stopped);
                    }
                }
                next = tasks.join_next_with_id() => match next {
                    None => break None,
                    Some(next) => settle(&mut ran, next),
                },
            }
        };
        // Cut short: a call that already finished still counts.
        while let Some(next) = tasks.try_join_next_with_id() {
            settle(&mut ran, next);
        }
        tasks.abort_all();
        (ran, interrupt)
    }

    /// Ends a run the guard stopped, with the guard's own message as the
    /// answer and no model call: a run stopped for spending too much must
    /// not spend more to say so.
    async fn halt(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        iterations: usize,
        why: String,
    ) -> String {
        warn!(%why, "the owner's guard stopped the run");
        self.incomplete = Some(why.clone());
        self.push(Message::assistant(Some(why.clone()), vec![], None));
        self.emit(tx, AgentEvent::AssistantText { text: why.clone() })
            .await;
        self.emit(
            tx,
            AgentEvent::RunIncomplete {
                reason: why.clone(),
                iterations,
            },
        )
        .await;
        why
    }

    /// Whatever reached the inbox since the last model call, as one user
    /// message.
    fn deliver_inbox(&mut self) {
        let Some(inbox) = &self.inbox else { return };
        let items = inbox.take();
        if !items.is_empty() {
            self.push(Message::user(items.join("\n\n")));
        }
    }

    fn request(&self) -> CompletionRequest {
        CompletionRequest {
            messages: self.rendered_messages(),
            tools: self.tools.definitions(),
            max_output_tokens: self.config.max_output_tokens,
            temperature: self.config.temperature,
            stream: None,
        }
    }

    async fn add_usage(&mut self, tx: &mpsc::Sender<AgentEvent>, usage: &Usage) {
        self.usage.input_tokens += usage.input_tokens;
        self.usage.output_tokens += usage.output_tokens;
        self.usage.cached_input_tokens += usage.cached_input_tokens;
        self.usage.cache_write_input_tokens += usage.cache_write_input_tokens;
        self.emit(
            tx,
            AgentEvent::Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cached_input_tokens: usage.cached_input_tokens,
                cache_write_input_tokens: usage.cache_write_input_tokens,
            },
        )
        .await;
    }

    /// Session-start recall, once per agent: the goal is the session's
    /// first user message (a resumed session's history already has one)
    /// plus this run's request. The block goes in a user message right
    /// after the goal, never into the system prompt, which stays the same
    /// bytes for every session (M27: the cached prefix).
    async fn recall_for(&mut self, goal: &str) {
        if self.recalled {
            return;
        }
        self.recalled = true;
        let Some(recall) = self.session_recall.clone() else {
            return;
        };
        let first = self
            .messages
            .iter()
            .find(|m| m.role == crate::message::Role::User)
            .and_then(|m| m.content.clone());
        let query = match first {
            Some(first) if first != goal => format!("{first}\n{goal}"),
            _ => goal.to_string(),
        };
        self.memory = recall.recall(&query).await.filter(|b| !b.trim().is_empty());
        self.memory_due = self.memory.is_some();
    }

    /// The [`TurnContext`] block for this run, if it says something new:
    /// the same block as last time adds nothing while that one is still in
    /// the history (compaction or truncation may have dropped it).
    async fn add_turn_context(&mut self, goal: &str) {
        let Some(source) = self.turn_context.clone() else {
            return;
        };
        let Some(block) = source
            .context(goal, &self.messages)
            .await
            .filter(|b| !b.trim().is_empty())
        else {
            return;
        };
        let present = |text: &str| {
            self.messages
                .iter()
                .any(|m| m.role == crate::message::Role::User && m.content.as_deref() == Some(text))
        };
        if self.turn_context_last.as_deref() == Some(block.as_str()) && present(&block) {
            return;
        }
        self.turn_context_last = Some(block.clone());
        self.push(Message::user(block));
    }

    /// M28: the skills `goal` triggers, each as a user message after it —
    /// never in the system prompt, so every earlier byte stays the same.
    async fn load_triggered(&mut self, goal: &str, tx: &mpsc::Sender<AgentEvent>) {
        let Some(triggers) = self.triggers.clone() else {
            return;
        };
        let loaded: Vec<String> = skill_blocks(&self.messages)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        for t in triggers.triggered(goal, &loaded) {
            let (name, matched) = (t.name, t.matched);
            let event = match t.load {
                TriggerLoad::Loaded(block) => {
                    self.push(Message::user(format!(
                        "[ferrule: the skill `{name}` was loaded because the message says \"{matched}\"]\n{block}"
                    )));
                    info!(skill = %name, %matched, "skill triggered");
                    self.emit(
                        tx,
                        AgentEvent::SkillTriggered {
                            name: name.clone(),
                            matched: matched.clone(),
                        },
                    )
                    .await;
                    format!("skill_triggered name={name} matched={matched:?}")
                }
                TriggerLoad::TooLarge => {
                    self.push(Message::user(format!(
                        "[ferrule: the skill `{name}` matches \"{matched}\" in the message but is too large to \
                         load automatically; call activate_skill if it's needed]"
                    )));
                    warn!(skill = %name, %matched, "triggered skill over the budget, not loaded");
                    format!("skill_trigger_skipped name={name} matched={matched:?} reason=\"over the budget\"")
                }
                TriggerLoad::Refused(why) => {
                    warn!(skill = %name, %matched, %why, "triggered skill refused");
                    format!("skill_trigger_refused name={name} matched={matched:?} reason={why:?}")
                }
            };
            if let Some(t) = &self.transcript {
                let _ = t.log_event(&event);
            }
        }
    }

    /// Adds to the history and the transcript.
    fn push(&mut self, msg: Message) {
        self.log(&msg);
        self.messages.push(msg);
    }

    fn log(&self, msg: &Message) {
        if let Some(t) = &self.transcript {
            if let Err(e) = t.log_message(msg) {
                warn!("transcript write failed: {e}");
            }
        }
    }

    /// Messages as sent to the provider: reasoning stripped when the profile
    /// says the model neither needs nor accepts it back.
    fn rendered_messages(&self) -> Vec<Message> {
        if self.profile.retain_reasoning {
            return self.messages.clone();
        }
        self.messages
            .iter()
            .map(|m| {
                let mut m = m.clone();
                m.reasoning = None;
                m
            })
            .collect()
    }

    /// Compaction: deterministic dedupe first (free), then structured LLM
    /// summary of everything before the trailing verbatim window.
    async fn maybe_compact(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        iteration: usize,
    ) -> Result<(), CoreError> {
        let before = self.est_context_tokens();
        let trigger = self.profile.compaction_trigger_tokens();
        if before <= trigger {
            self.emit(
                tx,
                AgentEvent::ContextReady {
                    est_tokens: before,
                    threshold_tokens: trigger,
                },
            )
            .await;
            return Ok(());
        }
        if self.config.overflow == ContextOverflow::Truncate {
            self.truncate_front(tx, before, trigger).await;
            return Ok(());
        }
        info!(before, trigger, "compacting context");

        self.dedupe_tool_results();
        // Shortening frees room without a provider call and without losing
        // anything (the full text stays fetchable); when it's enough, the
        // history stays verbatim and no summary is made.
        if self.shorten_old_tool_results(tx).await > 0 && self.est_context_tokens() <= trigger {
            return Ok(());
        }

        let keep = self.config.compaction_keep_last;
        if self.messages.len() <= keep + 1 {
            return Ok(()); // nothing foldable
        }
        let split = self.messages.len() - keep;
        let head = &self.messages[..split];
        let folded_refs = shortened_refs(head);

        // Skills activated in the folded part stay in force: their blocks
        // ride along verbatim (unless the verbatim tail already holds them)
        // and are kept out of the summarizer's input.
        let in_tail = skill_blocks(&self.messages[split..]);
        let carried: Vec<String> = skill_blocks(head)
            .into_iter()
            .filter(|(name, _)| !in_tail.iter().any(|(n, _)| n == name))
            .map(|(_, block)| block)
            .collect();

        // Skip a second compaction if the head is already mostly a summary.
        let transcript_text = head
            .iter()
            .map(|m| {
                let role = format!("{:?}", m.role).to_lowercase();
                let body = m.content.clone().unwrap_or_default();
                let body = if may_hold_skill(m) {
                    elide_skill_blocks(&body)
                } else {
                    body
                };
                format!("{role}: {body}")
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let mut input = self.hook_input();
        input.trigger = Some("auto".into());
        self.fire(tx, HookEvent::PreCompact, input.clone()).await;

        let summary_req = CompletionRequest {
            messages: vec![Message::user(format!(
                "{COMPACTION_TEMPLATE}{transcript_text}"
            ))],
            tools: vec![],
            max_output_tokens: Some(4096),
            temperature: Some(0.0),
            stream: None,
        };
        let summary = self
            .call_provider(tx, summary_req, iteration, "compaction")
            .await?
            .message
            .content
            .unwrap_or_default();

        let mut rebuilt = Vec::with_capacity(keep + 2);
        if let Some(sys) = self
            .messages
            .first()
            .filter(|m| m.role == crate::message::Role::System)
        {
            rebuilt.push(sys.clone());
        }
        let mut summary_msg = format!("[Compaction summary of earlier session]\n{summary}");
        if !carried.is_empty() {
            summary_msg.push_str(
                "\n\n[Skill instructions activated earlier in this session — still in force]\n",
            );
            summary_msg.push_str(&carried.join("\n\n"));
        }
        // The request itself, word for word: it's what says when the work
        // is done, and a summary tends to blur exactly that.
        let goal_in_tail = |goal: &str| {
            self.messages[split..]
                .iter()
                .any(|m| m.role == crate::message::Role::User && m.content.as_deref() == Some(goal))
        };
        if !folded_refs.is_empty() {
            summary_msg.push_str(
                "\n\n[Older tool results that were shortened, now folded into this summary — \
                 search_history {\"ref\": …} returns any of them in full]\n",
            );
            summary_msg.push_str(&folded_refs.join(" "));
        }
        if let Some(goal) = self.goal.as_deref().filter(|g| !goal_in_tail(g)) {
            summary_msg.push_str("\n\n[The request being worked on, verbatim]\n");
            summary_msg.push_str(goal);
        }
        // Recalled memory lived in the system prompt before M27, where no
        // compaction reached it; it stays as whole now.
        if let Some(memory) = self.memory.as_deref().filter(|m| !goal_in_tail(m)) {
            summary_msg.push_str("\n\n");
            summary_msg.push_str(memory);
        }
        summary_msg.push_str("\n\nContinue from here.");
        rebuilt.push(Message::user(summary_msg));
        // A provider's own blocks (signed thinking, encrypted reasoning)
        // are bound to the prefix they were made under; the summary just
        // replaced it, so the kept tail goes back as neutral messages (M23).
        rebuilt.extend(self.messages[split..].iter().cloned().map(|mut m| {
            m.native = None;
            m
        }));

        let folded = self.messages.len() - rebuilt.len();
        self.messages = rebuilt;
        let after = self.est_context_tokens();
        self.emit(
            tx,
            AgentEvent::Compacted {
                folded_messages: folded,
                est_tokens_before: before,
                est_tokens_after: after,
            },
        )
        .await;
        if let Some(t) = &self.transcript {
            let _ = t.log_event(&format!(
                "compacted: {folded} messages, {before} -> {after} est tokens"
            ));
        }
        if let Some(note) = self.fire(tx, HookEvent::PostCompact, input).await.context {
            self.push(Message::user(format!("[hook: PostCompact]\n{note}")));
        }
        Ok(())
    }

    /// [`ContextOverflow::Truncate`]: drop the oldest messages after the
    /// system prompt until the context fits under `trigger`. An assistant
    /// message goes together with the tool results that answer it, so no
    /// result is left without its call; the last message always stays.
    async fn truncate_front(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        before: usize,
        trigger: usize,
    ) {
        let start = match self.messages.first() {
            Some(m) if m.role == crate::message::Role::System => 1,
            _ => 0,
        };
        let mut dropped = 0;
        while self.est_context_tokens() > trigger {
            let mut end = start + 1;
            while end < self.messages.len() && self.messages[end].role == crate::message::Role::Tool
            {
                end += 1;
            }
            if end >= self.messages.len() {
                break;
            }
            self.messages.drain(start..end);
            dropped += end - start;
        }
        if dropped > 0 {
            // The prefix changed: provider-bound blocks can't be replayed (M23).
            for m in &mut self.messages {
                m.native = None;
            }
        }
        let after = self.est_context_tokens();
        info!(before, after, dropped, "truncated context");
        self.emit(
            tx,
            AgentEvent::Truncated {
                dropped_messages: dropped,
                est_tokens_before: before,
                est_tokens_after: after,
            },
        )
        .await;
        if let Some(t) = &self.transcript {
            let _ = t.log_event(&format!(
                "truncated: {dropped} oldest messages dropped, {before} -> {after} est tokens"
            ));
        }
    }

    /// Shorten tool results outside the verbatim tail that are longer than
    /// `shorten_tool_results_over` to a preview and a `search_history`
    /// reference. Only when the full text can be fetched back (a
    /// transcript and the tool); skill blocks are never shortened.
    /// Returns how many were shortened.
    async fn shorten_old_tool_results(&mut self, tx: &mpsc::Sender<AgentEvent>) -> usize {
        if self.transcript.is_none() || !self.tools.contains(SEARCH_HISTORY) {
            return 0;
        }
        let keep = self.config.compaction_keep_last;
        let limit = self.config.shorten_tool_results_over;
        let before = self.est_context_tokens();
        let end = self.messages.len().saturating_sub(keep);
        let mut shortened = 0;
        for i in 0..end {
            let m = &self.messages[i];
            if m.role != crate::message::Role::Tool {
                continue;
            }
            let Some(text) = m.content.as_deref() else {
                continue;
            };
            if text.len() <= limit
                || text.chars().count() <= limit
                || text.contains(SKILL_CONTENT_OPEN)
                || text.starts_with(SHORTENED_PREFIX)
            {
                continue;
            }
            let tool = m
                .tool_call_id
                .as_deref()
                .and_then(|id| {
                    self.messages[..i]
                        .iter()
                        .rev()
                        .flat_map(|a| a.tool_calls.iter())
                        .find(|c| c.id == id)
                })
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "tool".into());
            let preview: String = text.chars().take(SHORTENED_PREVIEW_CHARS).collect();
            let replacement = format!(
                "{SHORTENED_PREFIX}{tool}, {} chars) was shortened to save context. It began:\n\
                 {preview}\n…\nThe full text is still in this session's history: \
                 search_history {{\"ref\": \"{}\"}}]",
                text.chars().count(),
                result_ref(text)
            );
            self.messages[i].content = Some(replacement);
            shortened += 1;
        }
        if shortened > 0 {
            let after = self.est_context_tokens();
            info!(before, after, shortened, "shortened old tool results");
            self.emit(
                tx,
                AgentEvent::ToolResultsShortened {
                    shortened,
                    est_tokens_before: before,
                    est_tokens_after: after,
                },
            )
            .await;
            if let Some(t) = &self.transcript {
                let _ = t.log_event(&format!(
                    "shortened: {shortened} old tool results, {before} -> {after} est tokens"
                ));
            }
        }
        shortened
    }

    /// Drop duplicate tool results, keeping only the most recent copy —
    /// deterministic 15-30% context savings with zero information loss.
    fn dedupe_tool_results(&mut self) {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (i, m) in self.messages.iter().enumerate() {
            if m.role == crate::message::Role::Tool {
                if let Some(c) = &m.content {
                    seen.insert(c.clone(), i);
                }
            }
        }
        let mut latest: std::collections::HashSet<usize> = seen.values().cloned().collect();
        latest.insert(self.messages.len().saturating_sub(1));
        for (i, m) in self.messages.iter_mut().enumerate() {
            if m.role == crate::message::Role::Tool
                && !latest.contains(&i)
                && m.content.is_some()
                && seen.contains_key(m.content.as_ref().unwrap())
            {
                m.content = Some("[superseded by identical later tool result]".into());
            }
        }
    }
}

/// How a shortened tool result starts: `[ferrule: an older tool result (`
/// + tool name, size, preview and the `search_history` reference.
const SHORTENED_PREFIX: &str = "[ferrule: an older tool result (";
const SHORTENED_PREVIEW_CHARS: usize = 600;

/// The `search_history` references of shortened results in `messages`
/// (and those an earlier summary listed), the newest 20, oldest first.
fn shortened_refs(messages: &[Message]) -> Vec<String> {
    const MARK: &str = "search_history {\"ref\": \"";
    let mut refs: Vec<String> = Vec::new();
    for text in messages
        .iter()
        .filter(|m| {
            matches!(
                m.role,
                crate::message::Role::Tool | crate::message::Role::User
            )
        })
        .filter_map(|m| m.content.as_deref())
    {
        let mut pos = 0;
        while let Some(i) = text[pos..].find(MARK) {
            let start = pos + i + MARK.len();
            let r: String = text[start..].chars().take(17).collect();
            if r.len() == 17 && r.starts_with('r') && r[1..].chars().all(|c| c.is_ascii_hexdigit())
            {
                refs.retain(|x| x != &r);
                refs.push(r);
            }
            pos = start;
        }
    }
    let skip = refs.len().saturating_sub(20);
    refs.split_off(skip)
}

/// Skill blocks arrive as tool results and, after a compaction, inside the
/// summary (a user message). Anything else quoting the tag is left alone.
fn may_hold_skill(m: &Message) -> bool {
    matches!(
        m.role,
        crate::message::Role::Tool | crate::message::Role::User
    )
}

/// Byte ranges and names of the `<skill_content>` blocks in `text`. A block
/// whose close tag was cut off runs to the end of the text.
fn skill_spans(text: &str) -> Vec<(std::ops::Range<usize>, &str)> {
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(i) = text[pos..].find(SKILL_CONTENT_OPEN) {
        let start = pos + i;
        let name_start = start + SKILL_CONTENT_OPEN.len();
        let Some(name_len) = text[name_start..].find('"') else {
            break;
        };
        let end = text[name_start..]
            .find(SKILL_CONTENT_CLOSE)
            .map(|j| name_start + j + SKILL_CONTENT_CLOSE.len())
            .unwrap_or(text.len());
        spans.push((start..end, &text[name_start..name_start + name_len]));
        pos = end;
    }
    spans
}

/// `(name, block)` for every skill activated in `messages`, first-seen
/// order, the latest copy of each name winning.
fn skill_blocks(messages: &[Message]) -> Vec<(String, String)> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    for text in messages
        .iter()
        .filter(|m| may_hold_skill(m))
        .filter_map(|m| m.content.as_deref())
    {
        for (range, name) in skill_spans(text) {
            let block = text[range].to_string();
            match blocks.iter_mut().find(|(n, _)| n == name) {
                Some(slot) => slot.1 = block,
                None => blocks.push((name.to_string(), block)),
            }
        }
    }
    blocks
}

fn elide_skill_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for (range, name) in skill_spans(text) {
        out.push_str(&text[pos..range.start]);
        out.push_str(&format!(
            "[skill `{name}` instructions — carried forward verbatim, not part of this summary]"
        ));
        pos = range.end;
    }
    out.push_str(&text[pos..]);
    out
}

fn error_kind_of(e: &CoreError) -> String {
    match e {
        CoreError::Provider(_) => "provider",
        CoreError::Transient { .. } => "transient",
        CoreError::MalformedResponse(_) => "malformed_response",
        CoreError::ToolNotFound(_) => "tool_not_found",
        CoreError::ToolFailed { .. } => "tool_failed",
        CoreError::Io(_) => "io",
        CoreError::Serde(_) => "serde",
        CoreError::MaxIterations(_) => "max_iterations",
        CoreError::Stopped(_) => "stopped",
        CoreError::Aborted(_) => "aborted",
    }
    .to_string()
}

/// Keep ledger rows small: the JSONL file is meant to be grepped/aggregated,
/// not to hold full error dumps.
fn truncate_error(s: &str) -> String {
    const MAX_CHARS: usize = 500;
    if s.chars().count() <= MAX_CHARS {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(MAX_CHARS).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::CompletionResponse;
    use crate::tool::{Tool, ToolDefinition, ToolOutput};
    use std::sync::Mutex;

    struct ScriptProvider {
        responses: Mutex<Vec<Message>>,
    }

    #[async_trait::async_trait]
    impl Provider for ScriptProvider {
        fn name(&self) -> &str {
            "script"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let msg = self.responses.lock().unwrap().remove(0);
            Ok(CompletionResponse {
                message: msg,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
            })
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "echo back".into(),
                parameters: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            }
        }
        async fn call(
            &self,
            args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, CoreError> {
            Ok(ToolOutput::ok(
                args["text"].as_str().unwrap_or("").to_string(),
            ))
        }
    }

    fn make_agent(script: Vec<Message>) -> Agent {
        agent_with(script, AgentConfig::default())
    }

    fn agent_with(script: Vec<Message>, config: AgentConfig) -> Agent {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        Agent::new(
            Arc::new(ScriptProvider {
                responses: Mutex::new(script),
            }),
            reg,
            HarnessProfile::generic(),
            config,
            ToolContext::default(),
            None,
        )
    }

    fn echo(text: &str) -> Message {
        let call = crate::message::ToolCall {
            id: "1".into(),
            name: "echo".into(),
            arguments: serde_json::json!({ "text": text }),
        };
        Message::assistant(None, vec![call], None)
    }

    fn say(text: &str) -> Message {
        Message::assistant(Some(text.into()), vec![], None)
    }

    /// Room for every event of a long test run: `emit` waits on a full
    /// channel, and these tests only read after the run.
    fn events() -> (mpsc::Sender<AgentEvent>, mpsc::Receiver<AgentEvent>) {
        mpsc::channel(4096)
    }

    fn drain(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// A tool that another tool adds mid-run (M13: `mcp_add`) is in the
    /// very next provider request and can be called in the same run.
    #[tokio::test]
    async fn a_tool_added_mid_run_reaches_the_next_request() {
        #[derive(Default)]
        struct Live(Mutex<Vec<Arc<dyn Tool>>>);
        impl crate::tool::ToolSource for Live {
            fn tools(&self) -> Vec<Arc<dyn Tool>> {
                self.0.lock().unwrap().clone()
            }
        }
        struct Install(Arc<Live>);
        #[async_trait::async_trait]
        impl Tool for Install {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "install".into(),
                    description: "adds echo".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }
            async fn call(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolContext,
            ) -> Result<ToolOutput, CoreError> {
                self.0 .0.lock().unwrap().push(Arc::new(EchoTool));
                Ok(ToolOutput::ok("installed"))
            }
        }
        struct Recording {
            script: Mutex<Vec<Message>>,
            seen: Arc<Mutex<Vec<Vec<String>>>>,
        }
        #[async_trait::async_trait]
        impl Provider for Recording {
            fn name(&self) -> &str {
                "recording"
            }
            async fn complete(
                &self,
                req: CompletionRequest,
            ) -> Result<CompletionResponse, CoreError> {
                self.seen
                    .lock()
                    .unwrap()
                    .push(req.tools.iter().map(|t| t.name.clone()).collect());
                Ok(CompletionResponse {
                    message: self.script.lock().unwrap().remove(0),
                    usage: Usage::default(),
                })
            }
        }
        let live = Arc::new(Live::default());
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Install(live.clone())));
        reg.attach(live);
        let install = crate::message::ToolCall {
            id: "1".into(),
            name: "install".into(),
            arguments: serde_json::json!({}),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = Recording {
            script: Mutex::new(vec![
                Message::assistant(None, vec![install], None),
                echo("from the new tool"),
                say("done"),
            ]),
            seen: seen.clone(),
        };
        let mut agent = Agent::new(
            Arc::new(provider),
            reg,
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            None,
        );
        let (tx, _rx) = events();
        assert_eq!(agent.run("extend yourself", tx).await.unwrap(), "done");
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], ["install"]);
        assert_eq!(seen[1], ["echo", "install"]);
        assert!(agent
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some("from the new tool")));
    }

    #[tokio::test]
    async fn loop_runs_tool_then_finishes() {
        let script = vec![
            Message::assistant(
                None,
                vec![crate::message::ToolCall {
                    id: "1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "hi"}),
                }],
                None,
            ),
            Message::assistant(Some("done: hi".into()), vec![], None),
        ];
        let mut agent = make_agent(script);
        let (tx, mut rx) = mpsc::channel(64);
        let answer = agent.run("say hi", tx).await.unwrap();
        assert_eq!(answer, "done: hi");
        assert_eq!(agent.messages.len(), 4); // user, assistant(call), tool, assistant(final)
        assert_eq!(agent.usage.input_tokens, 20);

        let mut saw_tool = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, AgentEvent::ToolCallFinished { ref name, ok: true, .. } if name == "echo")
            {
                saw_tool = true;
            }
        }
        assert!(saw_tool);
    }

    #[tokio::test]
    async fn unknown_tool_error_is_fed_back_not_crash() {
        let script = vec![
            Message::assistant(
                None,
                vec![crate::message::ToolCall {
                    id: "1".into(),
                    name: "nope".into(),
                    arguments: serde_json::json!({}),
                }],
                None,
            ),
            Message::assistant(Some("recovered".into()), vec![], None),
        ];
        let mut agent = make_agent(script);
        let (tx, _rx) = mpsc::channel(64);
        let answer = agent.run("break things", tx).await.unwrap();
        assert_eq!(answer, "recovered");
        let tool_msg = &agent.messages[2];
        assert!(tool_msg.content.as_deref().unwrap().contains("error"));
    }

    #[tokio::test]
    async fn reasoning_stripped_only_when_profile_says_so() {
        let script = vec![Message::assistant(
            Some("ok".into()),
            vec![],
            Some("thinking...".into()),
        )];
        let mut agent = make_agent(script);
        agent.profile = HarnessProfile::generic(); // retain_reasoning = false
        let (tx, _rx) = mpsc::channel(64);
        agent.run("t", tx).await.unwrap();
        assert!(agent.rendered_messages()[1].reasoning.is_none());

        let mut agent2 = make_agent(vec![Message::assistant(
            Some("ok".into()),
            vec![],
            Some("thinking...".into()),
        )]);
        agent2.profile = HarnessProfile::kimi(); // retain_reasoning = true
        let (tx2, _rx2) = mpsc::channel(64);
        agent2.run("t", tx2).await.unwrap();
        assert!(agent2.rendered_messages()[1].reasoning.is_some());
    }

    struct RecordingSink {
        records: Mutex<Vec<LedgerRecord>>,
    }

    impl LedgerSink for RecordingSink {
        fn record(&self, record: LedgerRecord) {
            self.records.lock().unwrap().push(record);
        }
    }

    struct FailingProvider;

    #[async_trait::async_trait]
    impl Provider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            Err(CoreError::Provider("boom".into()))
        }
    }

    #[tokio::test]
    async fn failing_provider_call_still_writes_an_error_row() {
        let sink = Arc::new(RecordingSink {
            records: Mutex::new(Vec::new()),
        });
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let mut agent = Agent::new(
            Arc::new(FailingProvider),
            reg,
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            None,
        )
        .with_ledger(sink.clone(), "run", None, "test-model");

        let (tx, _rx) = mpsc::channel(64);
        let result = agent.run("hi", tx).await;
        assert!(result.is_err());

        let records = sink.records.lock().unwrap();
        assert_eq!(records.len(), 1, "a failed call must still produce a row");
        let r = &records[0];
        assert_eq!(r.outcome, "error");
        assert_eq!(r.error_kind.as_deref(), Some("provider"));
        assert!(r.error_message.as_deref().unwrap().contains("boom"));
        assert_eq!(r.task_shape, "run");
        assert_eq!(r.model, "test-model");
        assert_eq!(r.provider, "failing");
        assert_eq!(r.iteration, 0);
        assert_eq!(r.input_tokens, 0);
    }

    /// Like `ScriptProvider` but with per-response usage, so a test can
    /// assert exact token numbers per ledger row instead of a fixed value.
    struct ScriptProviderWithUsage {
        responses: Mutex<Vec<(Message, Usage)>>,
    }

    #[async_trait::async_trait]
    impl Provider for ScriptProviderWithUsage {
        fn name(&self) -> &str {
            "script-usage"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let (message, usage) = self.responses.lock().unwrap().remove(0);
            Ok(CompletionResponse { message, usage })
        }
    }

    #[tokio::test]
    async fn ledger_records_one_row_per_call_with_iteration_and_tokens() {
        let script = vec![
            (
                Message::assistant(
                    None,
                    vec![crate::message::ToolCall {
                        id: "1".into(),
                        name: "echo".into(),
                        arguments: serde_json::json!({"text": "hi"}),
                    }],
                    None,
                ),
                Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cached_input_tokens: 20,
                    cache_write_input_tokens: 0,
                },
            ),
            (
                Message::assistant(Some("done".into()), vec![], None),
                Usage {
                    input_tokens: 150,
                    output_tokens: 8,
                    cached_input_tokens: 30,
                    cache_write_input_tokens: 40,
                },
            ),
        ];
        let sink = Arc::new(RecordingSink {
            records: Mutex::new(Vec::new()),
        });
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let mut agent = Agent::new(
            Arc::new(ScriptProviderWithUsage {
                responses: Mutex::new(script),
            }),
            reg,
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            None,
        )
        .with_ledger(sink.clone(), "chat", Some("telegram".into()), "test-model");

        let (tx, _rx) = mpsc::channel(64);
        let answer = agent.run("say hi", tx).await.unwrap();
        assert_eq!(answer, "done");

        let records = sink.records.lock().unwrap();
        assert_eq!(
            records.len(),
            2,
            "one ledger row per provider call, including tool-call iterations"
        );

        assert_eq!(records[0].iteration, 0);
        assert_eq!(records[0].input_tokens, 100);
        assert_eq!(records[0].cached_input_tokens, 20);
        assert_eq!(records[0].output_tokens, 10);
        assert_eq!(records[0].tool_calls, 1);
        assert_eq!(records[0].call_kind, "turn");
        assert_eq!(records[0].outcome, "ok");
        assert_eq!(records[0].task_shape, "chat");
        assert_eq!(records[0].origin.as_deref(), Some("telegram"));

        assert_eq!(records[1].iteration, 1);
        assert_eq!(records[1].input_tokens, 150);
        assert_eq!(records[1].cached_input_tokens, 30);
        assert_eq!(records[1].cache_write_input_tokens, 40);
        assert_eq!(records[0].cache_write_input_tokens, 0);
        assert_eq!(records[1].output_tokens, 8);
        assert_eq!(records[1].tool_calls, 0);
        assert_eq!(records[1].outcome, "ok");
    }

    /// Records every prompt it is sent and answers with a fixed summary.
    struct CapturingProvider {
        prompts: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Provider for CapturingProvider {
        fn name(&self) -> &str {
            "capture"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let prompt = req
                .messages
                .iter()
                .filter_map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("\n");
            self.prompts.lock().unwrap().push(prompt);
            Ok(CompletionResponse {
                message: Message::assistant(Some("SUMMARY".into()), vec![], None),
                usage: Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn compaction_carries_skill_instructions_forward_verbatim() {
        let provider = Arc::new(CapturingProvider {
            prompts: Mutex::new(Vec::new()),
        });
        let mut profile = HarnessProfile::generic();
        profile.context_window = 1_000;
        profile.output_reserve = 0;
        profile.compaction_threshold = 0.1; // compact above ~100 tokens
        let config = AgentConfig {
            compaction_keep_last: 2,
            ..Default::default()
        };
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            profile,
            config,
            ToolContext::default(),
            None,
        )
        .with_system_prompt("sys");

        let block =
            format!("{SKILL_CONTENT_OPEN}pdf\">\nALWAYS-RUN-EXTRACT-FIRST\n{SKILL_CONTENT_CLOSE}");
        let call = crate::message::ToolCall {
            id: "1".into(),
            name: "activate_skill".into(),
            arguments: serde_json::json!({"name": "pdf"}),
        };
        agent.messages.push(Message::user("convert the pdf"));
        agent
            .messages
            .push(Message::assistant(None, vec![call], None));
        agent
            .messages
            .push(Message::tool_result("1", block.clone()));
        let filler = |i: usize| Message::user(format!("{}{i}", "filler ".repeat(50)));
        agent.messages.extend((0..4).map(filler));

        let summary_text = |agent: &Agent| {
            agent
                .messages
                .iter()
                .filter_map(|m| m.content.clone())
                .find(|c| c.starts_with("[Compaction summary"))
                .unwrap()
        };
        let (tx, _rx) = mpsc::channel(64);
        agent.maybe_compact(&tx, 0).await.unwrap();
        let first = summary_text(&agent);
        assert!(
            first.contains(&block),
            "skill block must survive compaction verbatim:\n{first}"
        );
        assert!(first.ends_with("Continue from here."));
        {
            let prompts = provider.prompts.lock().unwrap();
            assert_eq!(prompts.len(), 1);
            assert!(
                !prompts[0].contains("ALWAYS-RUN-EXTRACT-FIRST"),
                "the summarizer must not see (and paraphrase) the skill body"
            );
            assert!(prompts[0].contains("[skill `pdf` instructions"));
        }

        // A second compaction folds the first summary; the block rides along
        // once more, not twice.
        agent.messages.extend((4..6).map(filler));
        agent.maybe_compact(&tx, 1).await.unwrap();
        let second = summary_text(&agent);
        assert_eq!(second.matches(&block).count(), 1, "{second}");
        assert_eq!(provider.prompts.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn skill_block_still_in_the_verbatim_tail_is_not_duplicated() {
        let provider = Arc::new(CapturingProvider {
            prompts: Mutex::new(Vec::new()),
        });
        let mut profile = HarnessProfile::generic();
        profile.context_window = 1_000;
        profile.output_reserve = 0;
        profile.compaction_threshold = 0.1;
        let config = AgentConfig {
            compaction_keep_last: 2,
            ..Default::default()
        };
        let mut agent = Agent::new(
            provider,
            ToolRegistry::new(),
            profile,
            config,
            ToolContext::default(),
            None,
        );

        let block = format!("{SKILL_CONTENT_OPEN}pdf\">\nbody\n{SKILL_CONTENT_CLOSE}");
        agent
            .messages
            .extend((0..4).map(|i| Message::user(format!("{}{i}", "filler ".repeat(50)))));
        agent
            .messages
            .push(Message::tool_result("1", block.clone()));
        agent
            .messages
            .push(Message::tool_result("2", format!("{block} again")));

        let (tx, _rx) = mpsc::channel(64);
        agent.maybe_compact(&tx, 0).await.unwrap();
        let summary = agent.messages[0].content.clone().unwrap();
        assert!(summary.starts_with("[Compaction summary"));
        assert!(!summary.contains(&block), "the tail already holds it");
    }

    fn fast_retry(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            budget: Duration::from_secs(5),
        }
    }

    /// Fails transiently `failures` times, then answers.
    struct FlakyProvider {
        failures: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl Provider for FlakyProvider {
        fn name(&self) -> &str {
            "flaky"
        }
        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let mut left = self.failures.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(CoreError::Transient {
                    message: "HTTP 503".into(),
                    retry_after: None,
                });
            }
            Ok(CompletionResponse {
                message: say("ok"),
                usage: Usage::default(),
            })
        }
    }

    fn flaky_agent(failures: u32, max_attempts: u32, sink: Arc<RecordingSink>) -> Agent {
        let config = AgentConfig {
            retry: fast_retry(max_attempts),
            ..Default::default()
        };
        Agent::new(
            Arc::new(FlakyProvider {
                failures: Mutex::new(failures),
            }),
            ToolRegistry::new(),
            HarnessProfile::generic(),
            config,
            ToolContext::default(),
            None,
        )
        .with_ledger(sink, "run", None, "m")
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_with_a_row_per_attempt() {
        let sink = Arc::new(RecordingSink {
            records: Mutex::new(Vec::new()),
        });
        let mut agent = flaky_agent(2, 4, sink.clone());
        let (tx, mut rx) = events();
        assert_eq!(agent.run("hi", tx).await.unwrap(), "ok");

        let outcomes: Vec<String> = sink
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.outcome.clone())
            .collect();
        assert_eq!(outcomes, ["retried", "retried", "ok"]);
        assert_eq!(
            sink.records.lock().unwrap()[0].error_kind.as_deref(),
            Some("transient")
        );
        let retries: Vec<u32> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::ProviderRetry {
                    attempt,
                    max_attempts: 4,
                    ..
                } => Some(attempt),
                _ => None,
            })
            .collect();
        assert_eq!(retries, [1, 2]);
    }

    #[tokio::test]
    async fn retries_stop_at_max_attempts() {
        let sink = Arc::new(RecordingSink {
            records: Mutex::new(Vec::new()),
        });
        let mut agent = flaky_agent(10, 3, sink.clone());
        let (tx, _rx) = events();
        let err = agent.run("hi", tx).await.unwrap_err();
        assert!(err.is_transient(), "{err}");
        let outcomes: Vec<String> = sink
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.outcome.clone())
            .collect();
        assert_eq!(outcomes, ["retried", "retried", "error"]);
    }

    /// Two models: `a/one` fails with `error` on every call until it's
    /// marked down, then `b/two` answers. `fail_over` switches once.
    struct Routing {
        down: Mutex<bool>,
        error: fn() -> CoreError,
        asked: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl Provider for Routing {
        fn name(&self) -> &str {
            "routing"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            self.complete_routed(req).await.1
        }
        async fn complete_routed(
            &self,
            _req: CompletionRequest,
        ) -> (
            Option<crate::provider::Served>,
            Result<CompletionResponse, CoreError>,
        ) {
            let served = |p: &str, m: &str| crate::provider::Served {
                provider: p.into(),
                model: m.into(),
            };
            if *self.down.lock().unwrap() {
                let ok = CompletionResponse {
                    message: say("from two"),
                    usage: Usage::default(),
                };
                return (Some(served("b", "two")), Ok(ok));
            }
            (Some(served("a", "one")), Err((self.error)()))
        }
        fn fail_over(
            &self,
            served: Option<&crate::provider::Served>,
            _error: &CoreError,
        ) -> Option<crate::provider::FailOver> {
            *self.asked.lock().unwrap() += 1;
            let mut down = self.down.lock().unwrap();
            if *down {
                return None;
            }
            *down = true;
            Some(crate::provider::FailOver {
                from: served.unwrap().reference(),
                to: "b/two".into(),
            })
        }
    }

    fn routing_agent(error: fn() -> CoreError, sink: Arc<RecordingSink>) -> (Agent, Arc<Routing>) {
        let provider = Arc::new(Routing {
            down: Mutex::new(false),
            error,
            asked: Mutex::new(0),
        });
        let config = AgentConfig {
            retry: fast_retry(3),
            ..Default::default()
        };
        let agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            HarnessProfile::generic(),
            config,
            ToolContext::default(),
            None,
        )
        .with_ledger(sink, "run", None, "configured");
        (agent, provider)
    }

    #[tokio::test]
    async fn an_outage_after_the_retries_falls_over_and_rows_name_the_model_that_ran() {
        let sink = Arc::new(RecordingSink {
            records: Mutex::new(Vec::new()),
        });
        let (mut agent, provider) = routing_agent(
            || CoreError::Transient {
                message: "HTTP 503".into(),
                retry_after: None,
            },
            sink.clone(),
        );
        let (tx, mut rx) = events();
        assert_eq!(agent.run("hi", tx).await.unwrap(), "from two");
        let rows: Vec<(String, String, String)> = sink
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|r| (r.provider.clone(), r.model.clone(), r.outcome.clone()))
            .collect();
        let row = |p: &str, m: &str, o: &str| (p.to_string(), m.to_string(), o.to_string());
        assert_eq!(
            rows,
            [
                row("a", "one", "retried"),
                row("a", "one", "retried"),
                row("a", "one", "retried"),
                row("b", "two", "ok"),
            ]
        );
        assert_eq!(*provider.asked.lock().unwrap(), 1);
        let fallbacks: Vec<(String, String)> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::ModelFallback { from, to, error } => {
                    assert!(error.contains("503"), "{error}");
                    Some((from, to))
                }
                _ => None,
            })
            .collect();
        assert_eq!(fallbacks, [("a/one".to_string(), "b/two".to_string())]);
    }

    #[tokio::test]
    async fn a_refused_key_is_not_an_outage() {
        let sink = Arc::new(RecordingSink {
            records: Mutex::new(Vec::new()),
        });
        let (mut agent, provider) = routing_agent(
            || CoreError::Provider("HTTP 401: invalid api key".into()),
            sink.clone(),
        );
        let (tx, _rx) = events();
        let err = agent.run("hi", tx).await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
        assert_eq!(
            *provider.asked.lock().unwrap(),
            0,
            "never asked to fall over"
        );
        let rows = sink.records.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].provider.as_str(), rows[0].model.as_str()),
            ("a", "one")
        );
        assert_eq!(rows[0].outcome, "error");
    }

    #[tokio::test]
    async fn without_routing_rows_keep_the_configured_model() {
        let sink = Arc::new(RecordingSink {
            records: Mutex::new(Vec::new()),
        });
        let mut agent = flaky_agent(0, 3, sink.clone());
        let (tx, _rx) = events();
        agent.run("hi", tx).await.unwrap();
        let rows = sink.records.lock().unwrap();
        assert_eq!(
            (rows[0].provider.as_str(), rows[0].model.as_str()),
            ("flaky", "m")
        );
    }

    #[test]
    fn retry_delay_backs_off_within_bounds() {
        let policy = RetryPolicy::default();
        let transient = CoreError::Transient {
            message: "x".into(),
            retry_after: None,
        };
        for attempt in 1..=3 {
            let full = Duration::from_secs(2 << (attempt - 1));
            let d = policy.delay(&transient, attempt, Duration::ZERO).unwrap();
            assert!(d >= full / 2 && d <= full, "attempt {attempt}: {d:?}");
        }
        assert_eq!(
            policy.delay(&transient, 4, Duration::ZERO),
            None,
            "4 attempts in all"
        );
        assert_eq!(
            policy.delay(&transient, 1, Duration::from_secs(119)),
            None,
            "past the budget"
        );
        assert_eq!(
            policy.delay(&CoreError::Provider("401".into()), 1, Duration::ZERO),
            None
        );

        // The server's own wait wins over the backoff cap, not over the budget.
        let told = CoreError::Transient {
            message: "429".into(),
            retry_after: Some(Duration::from_secs(45)),
        };
        assert_eq!(
            policy.delay(&told, 1, Duration::ZERO),
            Some(Duration::from_secs(45))
        );
        assert_eq!(policy.delay(&told, 1, Duration::from_secs(80)), None);
    }

    #[tokio::test]
    async fn the_step_limit_ends_with_a_status_not_an_error() {
        let config = AgentConfig {
            max_iterations: 2,
            ..Default::default()
        };
        let mut agent = agent_with(
            vec![
                echo("a"),
                echo("b"),
                say("Stopped: got a and b, c is next."),
            ],
            config,
        );
        let (tx, mut rx) = events();
        let answer = agent.run("do a, b and c", tx).await.unwrap();
        assert_eq!(answer, "Stopped: got a and b, c is next.");
        assert_eq!(
            agent.incomplete.as_deref(),
            Some("it reached the limit of 2 steps")
        );
        let asked = agent.messages[agent.messages.len() - 2]
            .content
            .clone()
            .unwrap();
        assert!(
            asked.starts_with("[ferrule] Stopping here: it reached the limit of 2 steps."),
            "{asked}"
        );
        assert!(drain(&mut rx)
            .iter()
            .any(|e| matches!(e, AgentEvent::RunIncomplete { iterations: 2, .. })));

        // The next run starts clean.
        agent.messages.clear();
        let mut agent = agent_with(vec![say("fine")], AgentConfig::default());
        let (tx, _rx) = events();
        agent.run("again", tx).await.unwrap();
        assert_eq!(agent.incomplete, None);
    }

    #[tokio::test]
    async fn a_status_answer_that_calls_tools_anyway_falls_back() {
        let config = AgentConfig {
            max_iterations: 1,
            ..Default::default()
        };
        let mut agent = agent_with(vec![echo("a"), echo("b")], config);
        let (tx, _rx) = events();
        let answer = agent.run("go", tx).await.unwrap();
        assert_eq!(
            answer,
            "Stopped before finishing: it reached the limit of 1 step."
        );
        assert!(
            agent.messages.last().unwrap().tool_calls.is_empty(),
            "no dangling tool call in the history"
        );
    }

    #[tokio::test]
    async fn with_the_stuck_detector_off_a_loop_runs_on() {
        let mut script = vec![echo("same"); 8];
        script.push(say("done"));
        let config = AgentConfig {
            detect_stuck: false,
            ..Default::default()
        };
        let mut agent = agent_with(script, config);
        let (tx, mut rx) = events();
        assert_eq!(agent.run("loop", tx).await.unwrap(), "done");
        assert_eq!(agent.incomplete, None);
        assert!(!drain(&mut rx)
            .iter()
            .any(|e| matches!(e, AgentEvent::Stuck { .. })));
    }

    #[tokio::test]
    async fn a_loop_gets_one_warning_then_the_run_stops() {
        let mut script = vec![echo("same"); 8];
        script.push(say("I'm stuck on the same result."));
        let mut agent = make_agent(script);
        let (tx, mut rx) = events();
        let answer = agent.run("loop", tx).await.unwrap();
        assert_eq!(answer, "I'm stuck on the same result.");
        assert_eq!(
            agent.incomplete.as_deref(),
            Some("it kept repeating the same `echo` call after being warned")
        );

        let events = drain(&mut rx);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::Stuck { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::ToolCallFinished { .. }))
                .count(),
            8
        );
        let warned = agent
            .messages
            .iter()
            .filter_map(|m| m.content.as_deref())
            .filter(|c| c.contains("Doing it again won't change"))
            .count();
        assert_eq!(warned, 1);
    }

    #[tokio::test]
    async fn a_warned_loop_that_changes_course_finishes_normally() {
        let mut script = vec![echo("same"); 4];
        script.extend([echo("different"), say("done")]);
        let mut agent = make_agent(script);
        let (tx, _rx) = events();
        assert_eq!(agent.run("loop", tx).await.unwrap(), "done");
        assert_eq!(agent.incomplete, None);
    }

    /// Answers from a script and counts its runs.
    struct ScriptedCheck {
        results: Mutex<Vec<Result<(), String>>>,
        runs: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl Verifier for ScriptedCheck {
        fn describe(&self) -> String {
            "cargo test".into()
        }
        async fn verify(&self, _ctx: &ToolContext) -> Result<(), String> {
            *self.runs.lock().unwrap() += 1;
            self.results.lock().unwrap().remove(0)
        }
    }

    fn check(results: Vec<Result<(), String>>) -> Arc<ScriptedCheck> {
        Arc::new(ScriptedCheck {
            results: Mutex::new(results),
            runs: Mutex::new(0),
        })
    }

    #[tokio::test]
    async fn a_failing_check_sends_the_run_back_to_work() {
        let verifier = check(vec![Err("test foo ... FAILED".into()), Ok(())]);
        let script = vec![
            echo("edit"),
            say("done"),
            echo("fix"),
            say("done, tests pass"),
        ];
        let mut agent = make_agent(script).with_verifier(verifier.clone());
        let (tx, mut rx) = events();
        assert_eq!(
            agent.run("fix the bug", tx).await.unwrap(),
            "done, tests pass"
        );
        assert_eq!(*verifier.runs.lock().unwrap(), 2);
        assert_eq!(agent.incomplete, None);

        let told = agent
            .messages
            .iter()
            .filter_map(|m| m.content.as_deref())
            .find(|c| c.starts_with("[ferrule] `cargo test` fails"))
            .unwrap();
        assert!(
            told.ends_with("test foo ... FAILED"),
            "the check's output reaches the model: {told}"
        );
        let results: Vec<bool> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::VerifyFinished { ok, .. } => Some(ok),
                _ => None,
            })
            .collect();
        assert_eq!(results, [false, true]);
    }

    #[tokio::test]
    async fn nothing_changed_nothing_to_check() {
        let verifier = check(vec![]);
        let mut agent = make_agent(vec![say("the answer is 4")]).with_verifier(verifier.clone());
        let (tx, _rx) = events();
        agent.run("what is 2+2", tx).await.unwrap();
        assert_eq!(*verifier.runs.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_check_that_keeps_failing_ends_with_a_status() {
        let verifier = check(vec![Err("1 failed".into()), Err("still 1 failed".into())]);
        let config = AgentConfig {
            max_verify_rounds: 1,
            ..Default::default()
        };
        let script = vec![
            echo("edit"),
            say("done"),
            say("done now"),
            say("One test still fails; I couldn't find why."),
        ];
        let mut agent = agent_with(script, config).with_verifier(verifier.clone());
        let (tx, _rx) = events();
        let answer = agent.run("fix it", tx).await.unwrap();
        assert_eq!(answer, "One test still fails; I couldn't find why.");
        assert_eq!(
            agent.incomplete.as_deref(),
            Some("`cargo test` still fails after one round of fixes")
        );
        assert_eq!(*verifier.runs.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn compaction_keeps_the_request_verbatim() {
        let provider = Arc::new(CapturingProvider {
            prompts: Mutex::new(Vec::new()),
        });
        let mut profile = HarnessProfile::generic();
        profile.context_window = 1_000;
        profile.output_reserve = 0;
        profile.compaction_threshold = 0.1;
        let config = AgentConfig {
            compaction_keep_last: 2,
            ..Default::default()
        };
        let mut agent = Agent::new(
            provider,
            ToolRegistry::new(),
            profile,
            config,
            ToolContext::default(),
            None,
        );

        let goal = "Rename every `Foo` to `Bar`, but not in tests/.";
        agent.goal = Some(goal.into());
        agent.messages.push(Message::user(goal));
        agent
            .messages
            .extend((0..4).map(|i| Message::user(format!("{}{i}", "filler ".repeat(50)))));
        let (tx, _rx) = events();
        agent.maybe_compact(&tx, 0).await.unwrap();
        let summary = agent.messages[0].content.clone().unwrap();
        assert!(
            summary.contains(&format!("[The request being worked on, verbatim]\n{goal}")),
            "{summary}"
        );
        assert!(summary.ends_with("Continue from here."));

        // Still in the verbatim tail: not repeated.
        agent.messages.push(Message::user(goal));
        agent.messages.push(Message::user("last"));
        agent.maybe_compact(&tx, 1).await.unwrap();
        let summary = agent.messages[0].content.clone().unwrap();
        assert!(!summary.contains(goal), "{summary}");
    }

    #[tokio::test]
    async fn compaction_drops_native_blocks_from_the_kept_tail() {
        let provider = Arc::new(CapturingProvider {
            prompts: Mutex::new(Vec::new()),
        });
        let mut profile = HarnessProfile::generic();
        profile.context_window = 1_000;
        profile.output_reserve = 0;
        profile.compaction_threshold = 0.1;
        let config = AgentConfig {
            compaction_keep_last: 2,
            ..Default::default()
        };
        let mut agent = Agent::new(
            provider,
            ToolRegistry::new(),
            profile,
            config,
            ToolContext::default(),
            None,
        );
        let native = crate::message::NativeBlocks {
            api: "anthropic".into(),
            model: "m".into(),
            items: vec![serde_json::json!({"type": "thinking", "signature": "s"})],
        };
        agent.messages.extend((0..4).map(|i| {
            Message::assistant(Some(format!("{}{i}", "filler ".repeat(50))), vec![], None)
                .with_native(native.clone())
        }));
        let (tx, _rx) = events();
        agent.maybe_compact(&tx, 0).await.unwrap();
        assert_eq!(agent.messages.len(), 3);
        // Thinking signed under the old prefix can't follow the summary.
        assert!(agent.messages.iter().all(|m| m.native.is_none()));
        assert!(agent.messages[2].content.as_deref().unwrap().ends_with('3'));
    }

    #[tokio::test]
    async fn truncation_drops_the_oldest_and_keeps_the_system_prompt() {
        let provider = Arc::new(CapturingProvider {
            prompts: Mutex::new(Vec::new()),
        });
        let mut profile = HarnessProfile::generic();
        profile.context_window = 200;
        profile.output_reserve = 0;
        profile.compaction_threshold = 1.0;
        let config = AgentConfig {
            overflow: ContextOverflow::Truncate,
            ..Default::default()
        };
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(),
            profile,
            config,
            ToolContext::default(),
            None,
        );
        let filler = |tag: &str| format!("{tag} {}", "x".repeat(300));
        agent.messages.push(Message::system("SYSTEM PROMPT"));
        agent.messages.push(Message::user(filler("GOAL")));
        agent.messages.push(Message::assistant(
            None,
            vec![crate::message::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({}),
            }],
            None,
        ));
        agent
            .messages
            .push(Message::tool_result("c1", filler("RESULT")));
        agent.messages.push(Message::user(filler("MIDDLE")));
        agent.messages.push(Message::user(filler("LAST")));

        let (tx, mut rx) = events();
        agent.maybe_compact(&tx, 0).await.unwrap();

        let texts: Vec<String> = agent
            .messages
            .iter()
            .map(|m| m.content.clone().unwrap_or_default())
            .collect();
        assert_eq!(texts[0], "SYSTEM PROMPT");
        assert!(texts.iter().all(|t| !t.starts_with("GOAL")), "{texts:?}");
        assert!(texts.last().unwrap().starts_with("LAST"));
        // No tool result survives without the call it answers.
        assert!(agent
            .messages
            .iter()
            .all(|m| m.role != crate::message::Role::Tool));
        assert!(agent.est_context_tokens() <= 200);
        assert!(texts.iter().any(|t| t.starts_with("MIDDLE")));
        // Nothing summarized: no provider call at all.
        assert!(provider.prompts.lock().unwrap().is_empty());
        let mut saw = false;
        while let Ok(ev) = rx.try_recv() {
            if let AgentEvent::Truncated {
                dropped_messages, ..
            } = ev
            {
                assert_eq!(dropped_messages, 3);
                saw = true;
            }
        }
        assert!(saw);
    }

    #[tokio::test]
    async fn truncation_never_drops_the_last_message() {
        let provider = Arc::new(CapturingProvider {
            prompts: Mutex::new(Vec::new()),
        });
        let mut profile = HarnessProfile::generic();
        profile.context_window = 10;
        profile.output_reserve = 0;
        let config = AgentConfig {
            overflow: ContextOverflow::Truncate,
            ..Default::default()
        };
        let mut agent = Agent::new(
            provider,
            ToolRegistry::new(),
            profile,
            config,
            ToolContext::default(),
            None,
        );
        agent.messages.push(Message::system("S"));
        agent.messages.push(Message::user("x".repeat(400)));
        agent.messages.push(Message::user("y".repeat(400)));
        let (tx, _rx) = events();
        agent.maybe_compact(&tx, 0).await.unwrap();
        assert_eq!(agent.messages.len(), 2);
        assert_eq!(
            agent.messages[1].content.as_deref(),
            Some("y".repeat(400).as_str())
        );
    }

    struct CapBudget {
        spent: Mutex<u64>,
        cap: u64,
    }

    impl Budget for CapBudget {
        fn charge(&self, usage: &Usage) {
            *self.spent.lock().unwrap() += usage.input_tokens + usage.output_tokens;
        }
        fn exhausted(&self) -> Option<String> {
            let spent = *self.spent.lock().unwrap();
            (spent >= self.cap).then(|| format!("the budget of {} tokens is spent", self.cap))
        }
    }

    #[tokio::test]
    async fn budget_stops_the_run_with_a_status() {
        // Each call costs 15 tokens; the cap allows two turns.
        let budget = Arc::new(CapBudget {
            spent: Mutex::new(0),
            cap: 30,
        });
        let mut agent = make_agent(vec![echo("a"), echo("b"), say("status: stopped on budget")])
            .with_budget(budget.clone());
        let (tx, _rx) = events();
        let answer = agent.run("go", tx).await.unwrap();
        assert_eq!(answer, "status: stopped on budget");
        let why = agent.incomplete.clone().unwrap();
        assert!(why.contains("budget of 30 tokens"), "{why}");
        // Two turns plus the status call, and no third turn.
        assert_eq!(*budget.spent.lock().unwrap(), 45);
    }

    #[derive(Default)]
    struct TestInbox {
        items: Mutex<Vec<String>>,
        events: Mutex<Vec<&'static str>>,
    }

    impl Inbox for TestInbox {
        fn begin(&self) {
            self.events.lock().unwrap().push("begin");
        }
        fn take(&self) -> Vec<String> {
            self.events.lock().unwrap().push("take");
            std::mem::take(&mut *self.items.lock().unwrap())
        }
        fn end(&self) {
            self.events.lock().unwrap().push("end");
        }
    }

    /// Records every request it's sent, and answers from a script.
    struct SeeingProvider {
        responses: Mutex<Vec<Message>>,
        seen: Mutex<Vec<Vec<Message>>>,
        on_call: Box<dyn Fn(usize) + Send + Sync>,
    }

    #[async_trait::async_trait]
    impl Provider for SeeingProvider {
        fn name(&self) -> &str {
            "seeing"
        }
        async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
            let n = {
                let mut seen = self.seen.lock().unwrap();
                seen.push(req.messages);
                seen.len()
            };
            (self.on_call)(n);
            Ok(CompletionResponse {
                message: self.responses.lock().unwrap().remove(0),
                usage: Usage::default(),
            })
        }
    }

    fn seeing_agent(
        script: Vec<Message>,
        on_call: impl Fn(usize) + Send + Sync + 'static,
    ) -> (Agent, Arc<SeeingProvider>) {
        let provider = Arc::new(SeeingProvider {
            responses: Mutex::new(script),
            seen: Mutex::new(Vec::new()),
            on_call: Box::new(on_call),
        });
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let agent = Agent::new(
            provider.clone(),
            reg,
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            None,
        );
        (agent, provider)
    }

    /// Answers whatever block it's currently set to (M29: the repo map).
    struct SetContext(Mutex<Option<String>>);

    #[async_trait::async_trait]
    impl TurnContext for SetContext {
        async fn context(&self, _goal: &str, _history: &[Message]) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
    }

    fn bodies(msgs: &[Message]) -> Vec<String> {
        msgs.iter()
            .map(|m| format!("{:?}:{}", m.role, m.content.clone().unwrap_or_default()))
            .collect()
    }

    /// M29: a turn in which the block didn't change adds no bytes, so the
    /// whole previous request is a prefix of the next one (M27's cache);
    /// a changed block is appended after the goal, never edited in place.
    #[tokio::test]
    async fn turn_context_is_added_only_when_it_changes() {
        let (agent, provider) = seeing_agent(vec![say("one"), say("two"), say("three")], |_| {});
        let map = Arc::new(SetContext(Mutex::new(Some("[map v1]".into()))));
        let mut agent = agent.with_turn_context(map.clone());
        let (tx, _rx) = events();
        agent.run("first", tx.clone()).await.unwrap();
        agent.run("second", tx.clone()).await.unwrap();
        *map.0.lock().unwrap() = Some("[map v2]".into());
        agent.run("third", tx).await.unwrap();

        let seen = provider.seen.lock().unwrap();
        let (a, b, c) = (bodies(&seen[0]), bodies(&seen[1]), bodies(&seen[2]));
        assert_eq!(
            a[a.len() - 2..],
            ["User:first".to_string(), "User:[map v1]".into()]
        );
        assert_eq!(
            b[..a.len()],
            a[..],
            "the first request is a prefix of the second"
        );
        assert_eq!(
            b[a.len()..],
            ["Assistant:one".to_string(), "User:second".into()]
        );
        assert_eq!(c[..b.len()], b[..]);
        assert_eq!(
            c[b.len()..],
            [
                "Assistant:two".to_string(),
                "User:third".into(),
                "User:[map v2]".into()
            ]
        );
    }

    /// Once the block has left the history (compaction, truncation, a
    /// fresh history), the same answer goes in again.
    #[tokio::test]
    async fn turn_context_comes_back_when_the_history_lost_it() {
        let (agent, provider) = seeing_agent(vec![say("one"), say("two")], |_| {});
        let map = Arc::new(SetContext(Mutex::new(Some("[map]".into()))));
        let mut agent = agent.with_turn_context(map);
        let (tx, _rx) = events();
        agent.run("first", tx.clone()).await.unwrap();
        agent
            .messages
            .retain(|m| m.content.as_deref() != Some("[map]"));
        agent.run("second", tx).await.unwrap();
        let seen = provider.seen.lock().unwrap();
        assert_eq!(
            bodies(&seen[1]).last().map(String::as_str),
            Some("User:[map]")
        );
    }

    /// No answer, or a blank one, adds nothing.
    #[tokio::test]
    async fn an_empty_turn_context_adds_nothing() {
        let (agent, provider) = seeing_agent(vec![say("one")], |_| {});
        let mut agent =
            agent.with_turn_context(Arc::new(SetContext(Mutex::new(Some(" \n".into())))));
        let (tx, _rx) = events();
        agent.run("first", tx).await.unwrap();
        let seen = provider.seen.lock().unwrap();
        assert_eq!(
            bodies(&seen[0]).last().map(String::as_str),
            Some("User:first")
        );
    }

    /// Records what it was told (M29: the seam auto-commit uses).
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl RunObserver for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        async fn begin(&self, _session_id: &str) {
            self.0.lock().unwrap().push("begin".into());
        }
        async fn end(&self, run: &RunEnd<'_>) -> Option<String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("end {} -> {:?}", run.goal, run.answer));
            Some(format!("noted {}", run.goal))
        }
    }

    /// Every run is wrapped: begin before the model is called, end with
    /// the answer, and the note goes out as a Notice after the run.
    #[tokio::test]
    async fn run_observers_wrap_every_run() {
        let rec = Arc::new(Recorder::default());
        let (agent, _) = seeing_agent(vec![echo("a"), say("done"), say("again")], |_| {});
        let mut agent = agent.with_run_observer(rec.clone());
        let (tx, mut rx) = events();
        agent.run("go", tx.clone()).await.unwrap();
        agent.run("more", tx).await.unwrap();
        assert_eq!(
            *rec.0.lock().unwrap(),
            [
                "begin",
                "end go -> Some(\"done\")",
                "begin",
                "end more -> Some(\"again\")"
            ]
        );
        let notices: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::Notice { source, text } => Some(format!("{source}: {text}")),
                _ => None,
            })
            .collect();
        assert_eq!(notices, ["recorder: noted go", "recorder: noted more"]);
    }

    #[tokio::test]
    async fn inbox_items_arrive_before_the_next_model_call() {
        let inbox = Arc::new(TestInbox::default());
        let pushed = inbox.clone();
        // A notice lands while the first call is in flight.
        let (agent, provider) = seeing_agent(vec![echo("a"), say("done")], move |n| {
            if n == 1 {
                pushed
                    .items
                    .lock()
                    .unwrap()
                    .extend(["notice one".to_string(), "notice two".to_string()]);
            }
        });
        let mut agent = agent.with_inbox(inbox.clone());
        let (tx, _rx) = events();
        assert_eq!(agent.run("go", tx).await.unwrap(), "done");

        let seen = provider.seen.lock().unwrap();
        let first_has = |i: usize| {
            seen[i]
                .iter()
                .any(|m| m.content.as_deref() == Some("notice one\n\nnotice two"))
        };
        assert!(!first_has(0));
        assert!(first_has(1), "both notices as one user message");
        let events = inbox.events.lock().unwrap().clone();
        assert_eq!(events.first(), Some(&"begin"));
        assert_eq!(events.last(), Some(&"end"));
        assert_eq!(events.iter().filter(|e| **e == "take").count(), 2);
    }

    #[tokio::test]
    async fn inbox_end_runs_even_when_the_run_fails() {
        let inbox = Arc::new(TestInbox::default());
        let stop = StopFlag::new();
        stop.stop();
        let mut agent = make_agent(vec![say("never")])
            .with_inbox(inbox.clone())
            .with_stop_flag(stop);
        let (tx, _rx) = events();
        assert!(matches!(
            agent.run("go", tx).await,
            Err(CoreError::Aborted(_))
        ));
        assert_eq!(*inbox.events.lock().unwrap(), vec!["begin", "end"]);
    }

    #[tokio::test]
    async fn stop_flag_skips_remaining_tool_calls_and_answers_each() {
        let stop = StopFlag::new();
        let flag = stop.clone();
        let two_calls = Message::assistant(
            None,
            vec![
                crate::message::ToolCall {
                    id: "1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "x"}),
                },
                crate::message::ToolCall {
                    id: "2".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "y"}),
                },
            ],
            None,
        );
        // The parent stops it while the model is deciding.
        let (agent, provider) = seeing_agent(vec![two_calls], move |_| flag.stop());
        let mut agent = agent.with_stop_flag(stop.clone());
        let (tx, _rx) = events();
        let err = agent.run("go", tx).await.unwrap_err();
        assert!(matches!(err, CoreError::Aborted(_)), "{err}");
        assert_eq!(provider.seen.lock().unwrap().len(), 1);
        let results: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .map(|m| m.content.clone().unwrap())
            .collect();
        assert_eq!(results.len(), 2, "every call has a result: {results:?}");
        assert!(results.iter().all(|r| r.contains("stopped")));

        // Reset, the agent can run again from the same history.
        stop.reset();
        provider.responses.lock().unwrap().push(say("resumed"));
        let (tx, _rx) = events();
        assert_eq!(agent.run("continue", tx).await.unwrap(), "resumed");
    }

    #[tokio::test]
    async fn register_tool_and_append_system_prompt() {
        struct Other;
        #[async_trait::async_trait]
        impl Tool for Other {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "other".into(),
                    description: "".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }
            async fn call(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolContext,
            ) -> Result<ToolOutput, CoreError> {
                Ok(ToolOutput::ok("hi"))
            }
        }
        let mut bare = make_agent(vec![]);
        bare.append_system_prompt("only");
        assert_eq!(bare.messages[0].content.as_deref(), Some("only"));

        let mut agent = make_agent(vec![]).with_system_prompt("base");
        agent.append_system_prompt("more");
        assert_eq!(agent.messages.len(), 1);
        assert_eq!(agent.messages[0].content.as_deref(), Some("base\n\nmore"));
        assert!(!agent.has_tool("other"));
        agent.register_tool(Arc::new(Other));
        assert!(agent.has_tool("other"));
    }

    /// A guard for the loop's M19 seam: stops before the n-th model call,
    /// refuses one tool by name, and halts when told to.
    struct TestGuard {
        stop_at_call: Option<usize>,
        calls: Mutex<usize>,
        refuse: Option<&'static str>,
        halt: tokio::sync::watch::Sender<Option<String>>,
        begun: Mutex<usize>,
        seen: Mutex<Vec<(String, bool)>>,
    }

    impl TestGuard {
        fn new() -> Self {
            Self {
                stop_at_call: None,
                calls: Mutex::new(0),
                refuse: None,
                halt: tokio::sync::watch::channel(None).0,
                begun: Mutex::new(0),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Guard for TestGuard {
        fn begin(&self) {
            *self.begun.lock().unwrap() += 1;
        }
        fn before_model_call(&self) -> Option<String> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            (Some(*calls) == self.stop_at_call).then(|| "Stopped: the cap is spent.".to_string())
        }
        async fn before_tool_call(&self, call: GuardedCall<'_>) -> crate::guard::Verdict {
            self.seen
                .lock()
                .unwrap()
                .push((call.tool.to_string(), call.changes_files));
            match self.refuse {
                Some(name) if name == call.tool => {
                    crate::guard::Verdict::Refuse("the owner said no".into())
                }
                _ => crate::guard::Verdict::Allow,
            }
        }
        async fn halted(&self) -> String {
            let mut rx = self.halt.subscribe();
            let why = rx.wait_for(Option::is_some).await.unwrap();
            why.clone().unwrap()
        }
    }

    #[tokio::test]
    async fn a_guard_stop_ends_the_run_without_a_model_call() {
        let guard = Arc::new(TestGuard {
            stop_at_call: Some(2),
            ..TestGuard::new()
        });
        let mut agent = make_agent(vec![echo("a"), say("never sent"), say("no status either")])
            .with_guard(guard.clone());
        let (tx, mut rx) = events();
        let answer = agent.run("go", tx).await.unwrap();
        assert_eq!(answer, "Stopped: the cap is spent.");
        assert_eq!(agent.incomplete.as_deref(), Some(answer.as_str()));
        assert_eq!(*guard.begun.lock().unwrap(), 1);
        // One model call only, and the history ends with the stop message.
        assert_eq!(agent.usage.input_tokens, 10);
        let last = agent.messages.last().unwrap();
        assert_eq!(last.role, crate::message::Role::Assistant);
        assert_eq!(last.content.as_deref(), Some(answer.as_str()));
        let events = drain(&mut rx);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::RunIncomplete { iterations: 1, .. })));
    }

    #[tokio::test]
    async fn a_refused_tool_call_is_the_tools_result_and_the_run_goes_on() {
        let guard = Arc::new(TestGuard {
            refuse: Some("echo"),
            ..TestGuard::new()
        });
        let mut agent =
            make_agent(vec![echo("rm -rf x"), say("done another way")]).with_guard(guard.clone());
        let (tx, _rx) = events();
        assert_eq!(agent.run("go", tx).await.unwrap(), "done another way");
        assert_eq!(agent.incomplete, None);
        let result = agent
            .messages
            .iter()
            .find(|m| m.role == crate::message::Role::Tool)
            .unwrap();
        assert_eq!(
            result.content.as_deref(),
            Some("refused by ferrule: the owner said no")
        );
        assert_eq!(
            *guard.seen.lock().unwrap(),
            vec![("echo".to_string(), true)]
        );
    }

    struct SlowTool;

    #[async_trait::async_trait]
    impl Tool for SlowTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "slow".into(),
                description: "".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn call(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, CoreError> {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Ok(ToolOutput::ok("finished"))
        }
    }

    #[tokio::test]
    async fn a_halt_stops_a_tool_mid_call_and_answers_every_call() {
        let guard = Arc::new(TestGuard::new());
        let slow = crate::message::ToolCall {
            id: "s".into(),
            name: "slow".into(),
            arguments: serde_json::json!({}),
        };
        let then_echo = crate::message::ToolCall {
            id: "e".into(),
            name: "echo".into(),
            arguments: serde_json::json!({"text": "x"}),
        };
        let mut agent = make_agent(vec![
            Message::assistant(None, vec![slow, then_echo], None),
            say("never sent"),
        ])
        .with_guard(guard.clone());
        agent.register_tool(Arc::new(SlowTool));
        let halt = guard.halt.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            halt.send_replace(Some("Stopped: ferrule stop.".into()));
        });
        let (tx, _rx) = events();
        let answer = tokio::time::timeout(std::time::Duration::from_secs(10), agent.run("go", tx))
            .await
            .expect("the halt didn't stop the tool")
            .unwrap();
        assert_eq!(answer, "Stopped: ferrule stop.");
        let results: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .map(|m| m.content.clone().unwrap_or_default())
            .collect();
        assert_eq!(results.len(), 2, "{results:?}");
        assert!(results.iter().all(|r| r.starts_with("not run")));
        // A halted run doesn't ask the next tool call's verdict.
        assert_eq!(guard.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_halt_ends_a_model_call_in_flight() {
        struct Hang;
        #[async_trait::async_trait]
        impl Provider for Hang {
            fn name(&self) -> &str {
                "hang"
            }
            async fn complete(
                &self,
                _req: CompletionRequest,
            ) -> Result<CompletionResponse, CoreError> {
                std::future::pending().await
            }
        }
        let guard = Arc::new(TestGuard::new());
        let mut agent = Agent::new(
            Arc::new(Hang),
            ToolRegistry::new(),
            HarnessProfile::generic(),
            AgentConfig::default(),
            ToolContext::default(),
            None,
        )
        .with_guard(guard.clone());
        let halt = guard.halt.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            halt.send_replace(Some("Stopped.".into()));
        });
        let (tx, _rx) = events();
        let answer = tokio::time::timeout(std::time::Duration::from_secs(10), agent.run("go", tx))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answer, "Stopped.");
        assert_eq!(agent.incomplete.as_deref(), Some("Stopped."));
    }

    #[tokio::test]
    async fn the_step_limit_status_call_is_skipped_when_the_guard_says_stop() {
        let guard = Arc::new(TestGuard {
            stop_at_call: Some(2),
            ..TestGuard::new()
        });
        let config = AgentConfig {
            max_iterations: 1,
            ..Default::default()
        };
        let mut agent =
            agent_with(vec![echo("a"), say("status never sent")], config).with_guard(guard);
        let (tx, _rx) = events();
        let answer = agent.run("go", tx).await.unwrap();
        assert_eq!(answer, "Stopped: the cap is spent.");
        assert_eq!(agent.usage.input_tokens, 10);
    }

    // ---- M18: lifecycle hooks in the loop ----

    type HookFn = dyn Fn(&HookInput) -> crate::lifecycle::HookRun + Send + Sync;

    /// A hook whose command is a closure; keeps every payload it got.
    struct FnHook {
        name: &'static str,
        f: Box<HookFn>,
        seen: Mutex<Vec<HookInput>>,
    }

    #[async_trait::async_trait]
    impl crate::lifecycle::HookHandler for FnHook {
        fn command(&self) -> String {
            self.name.into()
        }
        async fn run(&self, input: &HookInput, _ctx: &ToolContext) -> crate::lifecycle::HookRun {
            self.seen.lock().unwrap().push(input.clone());
            (self.f)(input)
        }
    }

    fn fn_hook(
        name: &'static str,
        f: impl Fn(&HookInput) -> crate::lifecycle::HookRun + Send + Sync + 'static,
    ) -> Arc<FnHook> {
        Arc::new(FnHook {
            name,
            f: Box::new(f),
            seen: Mutex::default(),
        })
    }

    fn exit(code: i32, stdout: &str, stderr: &str) -> crate::lifecycle::HookRun {
        crate::lifecycle::HookRun {
            exit_code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
            ..Default::default()
        }
    }

    fn hook_set(hooks: Vec<(HookEvent, Option<&str>, Arc<FnHook>)>) -> HookSet {
        let mut set = HookSet::new();
        for (event, matcher, handler) in hooks {
            set.add(Hook::new(
                event,
                crate::lifecycle::Matcher::parse(matcher),
                crate::lifecycle::HookSource::User,
                handler,
            ));
        }
        set
    }

    /// Counts its calls: proof a blocked call never ran.
    #[derive(Default)]
    struct CountTool(Mutex<usize>);

    #[async_trait::async_trait]
    impl Tool for CountTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "counts".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn call(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, CoreError> {
            *self.0.lock().unwrap() += 1;
            Ok(ToolOutput::ok("ran"))
        }
    }

    #[tokio::test]
    async fn a_pre_tool_use_block_refuses_the_call_and_tells_the_model_why() {
        let guard = fn_hook("guard", |_| exit(2, "", "no echo in this repo\n"));
        let (agent, provider) = seeing_agent(vec![echo("x"), say("ok, I won't")], |_| {});
        let counter = Arc::new(CountTool::default());
        let mut agent = agent.with_hooks(hook_set(vec![(
            HookEvent::PreToolUse,
            Some("echo"),
            guard.clone(),
        )]));
        agent.register_tool(counter.clone());
        let (tx, mut rx) = events();
        assert_eq!(agent.run("go", tx).await.unwrap(), "ok, I won't");
        assert_eq!(*counter.0.lock().unwrap(), 0, "the tool never ran");
        let seen = provider.seen.lock().unwrap();
        let result = seen[1].last().unwrap().content.clone().unwrap();
        assert_eq!(
            result,
            "error: not run: a PreToolUse hook blocked it: no echo in this repo"
        );
        let input = &guard.seen.lock().unwrap()[0];
        assert_eq!(input.hook_event_name, "PreToolUse");
        assert_eq!(input.tool_name.as_deref(), Some("echo"));
        assert_eq!(input.tool_input, Some(serde_json::json!({"text": "x"})));
        let ev = drain(&mut rx);
        assert!(ev.iter().any(|e| matches!(
            e,
            AgentEvent::HookFinished { event, blocked: true, .. } if event == "PreToolUse"
        )));
        assert!(ev
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallFinished { ok: false, .. })));
    }

    #[tokio::test]
    async fn notes_are_appended_after_the_prompt_and_to_tool_results_only() {
        let start = fn_hook("start", |_| exit(0, "branch: main", ""));
        let prompt = fn_hook("prompt", |_| exit(0, "the user is on call", ""));
        let post = fn_hook("post", |i| {
            let ok = i.tool_response.as_ref().unwrap()["ok"] == true;
            exit(
                0,
                &format!(
                    r#"{{"hookSpecificOutput":{{"additionalContext":"lint clean, ok={ok}"}}}}"#
                ),
                "",
            )
        });
        let (agent, provider) = seeing_agent(vec![echo("hi"), say("done"), say("again")], |_| {});
        let mut agent = agent.with_system_prompt("SYSTEM").with_hooks(hook_set(vec![
            (HookEvent::SessionStart, None, start.clone()),
            (HookEvent::UserPromptSubmit, None, prompt.clone()),
            (HookEvent::PostToolUse, Some("ech*"), post.clone()),
        ]));
        let (tx, _rx) = events();
        agent.run("first", tx.clone()).await.unwrap();
        agent.run("second", tx).await.unwrap();

        let seen = provider.seen.lock().unwrap();
        let texts = |i: usize| -> Vec<String> {
            seen[i]
                .iter()
                .map(|m| m.content.clone().unwrap_or_default())
                .collect()
        };
        assert_eq!(
            texts(0),
            [
                "SYSTEM",
                "first",
                "[hook: SessionStart]\nbranch: main",
                "[hook: UserPromptSubmit]\nthe user is on call"
            ]
        );
        assert_eq!(
            texts(1).last().unwrap(),
            "hi\n\n[hook: PostToolUse] lint clean, ok=true"
        );
        // Each request extends the one before: the cached prefix holds.
        for i in 1..seen.len() {
            assert_eq!(seen[i][..seen[i - 1].len()].len(), seen[i - 1].len());
            for (a, b) in seen[i - 1].iter().zip(&seen[i]) {
                assert_eq!(a.content, b.content);
            }
        }
        // SessionStart once per agent; the prompt hook once per request.
        assert_eq!(start.seen.lock().unwrap().len(), 1);
        assert_eq!(
            start.seen.lock().unwrap()[0].source.as_deref(),
            Some("startup")
        );
        let prompts: Vec<Option<String>> = prompt
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|i| i.prompt.clone())
            .collect();
        assert_eq!(prompts, [Some("first".into()), Some("second".into())]);
    }

    #[tokio::test]
    async fn a_blocked_prompt_never_reaches_the_model() {
        let gate = fn_hook("gate", |i| {
            if i.prompt.as_deref().unwrap_or_default().contains("secret") {
                exit(
                    0,
                    r#"{"decision":"block","reason":"that looks like a key"}"#,
                    "",
                )
            } else {
                exit(0, "", "")
            }
        });
        // An empty script: any model call would panic.
        let mut agent = make_agent(vec![]).with_hooks(hook_set(vec![(
            HookEvent::UserPromptSubmit,
            None,
            gate,
        )]));
        let (tx, _rx) = events();
        let answer = agent.run("here is my secret sk-123", tx).await.unwrap();
        assert_eq!(answer, "Blocked by a hook: that looks like a key");
        assert!(agent.incomplete.is_some());
        assert!(agent.messages.iter().all(|m| !m
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("sk-123")));
    }

    #[tokio::test]
    async fn a_stop_hook_that_always_blocks_is_capped() {
        let nag = fn_hook("nag", |_| exit(2, "", "write the changelog first"));
        let script = vec![
            say("done"),
            say("done 2"),
            say("done 3"),
            say("done 4"),
            say("Stopped: the Stop hook keeps blocking."),
        ];
        let mut agent =
            make_agent(script).with_hooks(hook_set(vec![(HookEvent::Stop, None, nag.clone())]));
        let (tx, _rx) = events();
        let answer = agent.run("do it", tx).await.unwrap();
        assert_eq!(answer, "Stopped: the Stop hook keeps blocking.");
        assert_eq!(
            agent.incomplete.as_deref(),
            Some("the Stop hook `nag` still blocks after 3 tries")
        );
        let active: Vec<Option<bool>> = nag
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|i| i.stop_hook_active)
            .collect();
        assert_eq!(active, [Some(false), Some(true), Some(true), Some(true)]);
        assert_eq!(
            nag.seen.lock().unwrap()[0]
                .last_assistant_message
                .as_deref(),
            Some("done")
        );
        let told = agent
            .messages
            .iter()
            .filter(|m| {
                m.content.as_deref()
                    == Some("[hook: Stop] This isn't done yet:\n\nwrite the changelog first")
            })
            .count();
        assert_eq!(told, 3);
    }

    #[tokio::test]
    async fn the_check_runs_before_stop_hooks_and_has_its_own_cap() {
        let verifier = check(vec![Err("1 failed".into()), Ok(())]);
        let once = Arc::new(Mutex::new(false));
        let flag = once.clone();
        let stop = fn_hook("stop", move |_| {
            let mut blocked = flag.lock().unwrap();
            if *blocked {
                exit(0, "", "")
            } else {
                *blocked = true;
                exit(2, "", "update the docs")
            }
        });
        let script = vec![echo("edit"), say("done"), say("fixed"), say("docs too")];
        let config = AgentConfig {
            max_verify_rounds: 1,
            ..Default::default()
        };
        let mut agent = agent_with(script, config)
            .with_hooks(hook_set(vec![(HookEvent::Stop, None, stop.clone())]))
            .with_verifier(verifier.clone());
        let (tx, _rx) = events();
        assert_eq!(agent.run("fix", tx).await.unwrap(), "docs too");
        assert_eq!(agent.incomplete, None);
        assert_eq!(*verifier.runs.lock().unwrap(), 2);
        // The user hook only ran once the check passed.
        let seen = stop.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].stop_hook_active, Some(true));
        assert_eq!(seen[0].files_changed, Some(true));
        assert!(agent.hooks().hooks()[0].check, "the check goes first");
    }

    #[tokio::test]
    async fn a_failing_hook_is_the_owners_problem_not_the_models() {
        let broken = fn_hook("broken", |_| exit(1, "", "python: not found"));
        let (agent, provider) = seeing_agent(vec![echo("x"), say("done")], |_| {});
        let mut agent = agent.with_hooks(hook_set(vec![
            (HookEvent::PreToolUse, None, broken.clone()),
            (HookEvent::PostToolUse, None, broken.clone()),
            (HookEvent::Stop, None, broken.clone()),
        ]));
        let (tx, mut rx) = events();
        assert_eq!(agent.run("go", tx).await.unwrap(), "done");
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen[1].last().unwrap().content.as_deref(), Some("x"));
        let errors: Vec<String> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::HookFinished { error, .. } => error,
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0], "exit code 1: python: not found");
    }

    #[tokio::test]
    async fn compaction_fires_its_hooks_and_the_note_comes_after() {
        let provider = Arc::new(CapturingProvider {
            prompts: Mutex::new(Vec::new()),
        });
        let mut profile = HarnessProfile::generic();
        profile.context_window = 1_000;
        profile.output_reserve = 0;
        profile.compaction_threshold = 0.1;
        let config = AgentConfig {
            compaction_keep_last: 2,
            ..Default::default()
        };
        let pre = fn_hook("pre", |_| exit(0, "", ""));
        let post = fn_hook("post", |_| {
            exit(0, r#"{"additionalContext":"re-read NOTES.md"}"#, "")
        });
        let mut agent = Agent::new(
            provider,
            ToolRegistry::new(),
            profile,
            config,
            ToolContext::default(),
            None,
        )
        .with_hooks(hook_set(vec![
            (HookEvent::PreCompact, Some("auto"), pre.clone()),
            (HookEvent::PostCompact, None, post.clone()),
        ]));
        agent
            .messages
            .extend((0..5).map(|i| Message::user(format!("{}{i}", "filler ".repeat(50)))));
        let (tx, _rx) = events();
        agent.maybe_compact(&tx, 0).await.unwrap();
        assert_eq!(pre.seen.lock().unwrap().len(), 1);
        assert_eq!(pre.seen.lock().unwrap()[0].trigger.as_deref(), Some("auto"));
        assert_eq!(
            agent.messages.last().unwrap().content.as_deref(),
            Some("[hook: PostCompact]\nre-read NOTES.md")
        );
    }

    #[tokio::test]
    async fn session_end_fires_with_its_reason() {
        let end = fn_hook("end", |_| exit(0, "", ""));
        let agent = make_agent(vec![]).with_hooks(hook_set(vec![(
            HookEvent::SessionEnd,
            None,
            end.clone(),
        )]));
        let (tx, _rx) = events();
        agent.end_session("exit", &tx).await;
        let seen = end.seen.lock().unwrap();
        assert_eq!(seen[0].reason.as_deref(), Some("exit"));
        assert_eq!(seen[0].session_id, "ephemeral");
    }
}
