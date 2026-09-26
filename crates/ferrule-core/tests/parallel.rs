//! M27: read-only tool calls from one response run side by side, and
//! everything a call had before (hooks, the guard, a stop, a halt, the
//! order its result comes back in) holds per call.

use ferrule_core::lifecycle::{Hook, HookHandler, HookInput, HookRun, HookSource, Matcher};
use ferrule_core::message::ToolCall;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::{
    Agent, AgentConfig, AgentEvent, CoreError, Guard, GuardedCall, HarnessProfile, HookEvent,
    HookSet, LedgerRecord, LedgerSink, Message, Provider, Role, StopFlag, ToolRegistry, Usage,
    Verdict,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// Answers with `batch` as its first response, then "done"; keeps every
/// request's messages.
struct Model {
    batch: Vec<ToolCall>,
    calls: AtomicUsize,
    seen: Mutex<Vec<Vec<Message>>>,
}

fn model(calls: &[(&str, Value)]) -> Arc<Model> {
    Arc::new(Model {
        batch: calls
            .iter()
            .enumerate()
            .map(|(i, (name, args))| ToolCall {
                id: format!("c{i}"),
                name: name.to_string(),
                arguments: args.clone(),
            })
            .collect(),
        calls: AtomicUsize::new(0),
        seen: Mutex::default(),
    })
}

#[async_trait::async_trait]
impl Provider for Model {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        self.seen.lock().unwrap().push(req.messages.clone());
        let message = match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => Message::assistant(None, self.batch.clone(), None),
            _ => Message::assistant(Some("done".into()), vec![], None),
        };
        Ok(CompletionResponse {
            message,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 10,
                ..Usage::default()
            },
        })
    }
}

/// When each call of each tool started and ended.
type Log = Arc<Mutex<Vec<(String, Instant, Instant)>>>;

#[derive(Clone, Copy, PartialEq)]
enum Does {
    Work,
    Fail,
    Panic,
}

/// Sleeps `args.ms`, logs its span under `args.tag`, then does what it's told.
struct Slow {
    name: &'static str,
    read_only: bool,
    group: Option<&'static str>,
    does: Does,
    log: Log,
    stop: Option<StopFlag>,
}

impl Slow {
    fn new(name: &'static str, read_only: bool, log: &Log) -> Self {
        Self {
            name,
            read_only,
            group: None,
            does: Does::Work,
            log: log.clone(),
            stop: None,
        }
    }
}

#[async_trait::async_trait]
impl Tool for Slow {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "sleeps".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, args: Value, _: &ToolContext) -> Result<ToolOutput, CoreError> {
        let tag = args["tag"].as_str().unwrap_or(self.name).to_string();
        let start = Instant::now();
        if let Some(stop) = &self.stop {
            stop.stop();
        }
        tokio::time::sleep(Duration::from_millis(args["ms"].as_u64().unwrap_or(0))).await;
        self.log
            .lock()
            .unwrap()
            .push((tag.clone(), start, Instant::now()));
        match self.does {
            Does::Work => Ok(ToolOutput::ok(format!("{tag} ok"))),
            Does::Fail => Err(CoreError::ToolFailed {
                tool: self.name.into(),
                message: format!("{tag} broke"),
            }),
            Does::Panic => panic!("{tag} panicked"),
        }
    }
    fn changes_files(&self) -> bool {
        !self.read_only
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn serial_group(&self) -> Option<String> {
        self.group.map(String::from)
    }
}

fn agent(model: Arc<Model>, tools: ToolRegistry, parallel_tools: usize) -> Agent {
    Agent::new(
        model,
        tools,
        HarnessProfile::generic(),
        AgentConfig {
            parallel_tools,
            ..AgentConfig::default()
        },
        ToolContext::default(),
        None,
    )
    .with_system_prompt("test")
}

async fn run(agent: &mut Agent) -> (Result<String, CoreError>, Vec<AgentEvent>) {
    let (tx, mut rx) = mpsc::channel(4096);
    let out = agent.run("go", tx).await;
    (out, std::iter::from_fn(|| rx.try_recv().ok()).collect())
}

/// The tool results in the history, as (call id, content).
fn results(agent: &Agent) -> Vec<(String, String)> {
    agent
        .messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| {
            (
                m.tool_call_id.clone().unwrap_or_default(),
                m.content.clone().unwrap_or_default(),
            )
        })
        .collect()
}

fn span(log: &Log, tag: &str) -> (Instant, Instant) {
    let log = log.lock().unwrap();
    let (_, s, e) = log.iter().find(|(t, _, _)| t == tag).unwrap();
    (*s, *e)
}

fn overlap(log: &Log, a: &str, b: &str) -> bool {
    let (sa, ea) = span(log, a);
    let (sb, eb) = span(log, b);
    sa < eb && sb < ea
}

fn nap(tag: &str, ms: u64) -> Value {
    json!({"tag": tag, "ms": ms})
}

#[derive(Default)]
struct Rows(Mutex<Vec<LedgerRecord>>);

impl LedgerSink for Rows {
    fn record(&self, record: LedgerRecord) {
        self.0.lock().unwrap().push(record);
    }
}

#[tokio::test]
async fn read_only_calls_overlap_and_come_back_in_order() {
    let log = Log::default();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Slow::new("look", true, &log)));
    // Last to be asked, first to finish: the order is the model's, not
    // the finish line's.
    let m = model(&[
        ("look", nap("a", 300)),
        ("look", nap("b", 200)),
        ("look", nap("c", 100)),
    ]);
    let rows = Arc::new(Rows::default());
    let mut a = agent(m.clone(), tools, 4).with_ledger(rows.clone(), "run", None, "m");
    let (out, events) = run(&mut a).await;
    assert_eq!(out.unwrap(), "done");
    assert!(overlap(&log, "a", "b") && overlap(&log, "b", "c") && overlap(&log, "a", "c"));
    assert_eq!(
        results(&a),
        [
            ("c0".into(), "a ok".into()),
            ("c1".into(), "b ok".into()),
            ("c2".into(), "c ok".into())
        ]
    );
    // The model saw the results in its own order too.
    let seen = m.seen.lock().unwrap();
    let ids: Vec<_> = seen[1]
        .iter()
        .filter_map(|m| m.tool_call_id.clone())
        .collect();
    assert_eq!(ids, ["c0", "c1", "c2"]);
    // Every call is announced and finished, finished in order.
    let finished: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallFinished { id, ok: true, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(finished, ["c0", "c1", "c2"]);
    // The batch lands on the next provider row: three side by side, the
    // wall time well under the sum.
    let rows = rows.0.lock().unwrap();
    let batch = rows[1].speed.as_ref().unwrap().tool_batch.clone().unwrap();
    assert_eq!((batch.calls, batch.parallel), (3, 3));
    assert!(
        batch.sum_ms >= 600 && batch.wall_ms < batch.sum_ms * 3 / 4,
        "{batch:?}"
    );
    assert!(rows[0].speed.is_none());
}

#[tokio::test]
async fn a_write_is_a_barrier_between_two_parallel_runs() {
    let log = Log::default();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Slow::new("look", true, &log)));
    tools.register(Arc::new(Slow::new("edit", false, &log)));
    let m = model(&[
        ("look", nap("r1", 150)),
        ("look", nap("r2", 150)),
        ("edit", nap("w", 50)),
        ("look", nap("r3", 150)),
        ("look", nap("r4", 150)),
    ]);
    let rows = Arc::new(Rows::default());
    let mut a = agent(m, tools, 4).with_ledger(rows.clone(), "run", None, "m");
    let (out, _) = run(&mut a).await;
    assert_eq!(out.unwrap(), "done");
    assert!(overlap(&log, "r1", "r2") && overlap(&log, "r3", "r4"));
    let (ws, we) = span(&log, "w");
    assert!(span(&log, "r1").1 <= ws && span(&log, "r2").1 <= ws);
    assert!(span(&log, "r3").0 >= we && span(&log, "r4").0 >= we);
    let ids: Vec<_> = results(&a).into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, ["c0", "c1", "c2", "c3", "c4"]);
    let batch = rows.0.lock().unwrap()[1].speed.clone().unwrap().tool_batch;
    assert_eq!(batch.map(|b| (b.calls, b.parallel)), Some((5, 4)));
}

#[tokio::test]
async fn one_failure_or_crash_doesnt_lose_the_others() {
    let log = Log::default();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Slow::new("look", true, &log)));
    tools.register(Arc::new(Slow {
        does: Does::Fail,
        ..Slow::new("broken", true, &log)
    }));
    tools.register(Arc::new(Slow {
        does: Does::Panic,
        ..Slow::new("crashy", true, &log)
    }));
    let m = model(&[
        ("look", nap("a", 50)),
        ("broken", nap("b", 10)),
        ("crashy", nap("c", 10)),
        ("look", nap("d", 80)),
    ]);
    let mut a = agent(m, tools, 4);
    let (out, events) = run(&mut a).await;
    assert_eq!(out.unwrap(), "done");
    let got = results(&a);
    assert_eq!(got[0].1, "a ok");
    assert!(got[1].1.starts_with("error:") && got[1].1.contains("b broke"));
    assert_eq!(got[2].1, "error: the tool crashed");
    assert_eq!(got[3].1, "d ok");
    let oks: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallFinished { ok, .. } => Some(*ok),
            _ => None,
        })
        .collect();
    assert_eq!(oks, [true, false, false, true]);
}

/// Counts the calls it saw, and blocks the one tagged `blocked`.
struct Watch {
    seen: Arc<Mutex<Vec<(HookEvent, String)>>>,
    event: HookEvent,
}

#[async_trait::async_trait]
impl HookHandler for Watch {
    fn command(&self) -> String {
        "watch".into()
    }
    async fn run(&self, input: &HookInput, _: &ToolContext) -> HookRun {
        let id = input.tool_use_id.clone().unwrap_or_default();
        self.seen.lock().unwrap().push((self.event, id.clone()));
        let tag = input.tool_input.as_ref().map(|a| a["tag"].clone());
        if self.event == HookEvent::PreToolUse && tag == Some(json!("blocked")) {
            return HookRun {
                exit_code: Some(2),
                stderr: "not this one".into(),
                ..Default::default()
            };
        }
        HookRun {
            exit_code: Some(0),
            stdout: format!("{{\"additionalContext\":\"seen {id}\"}}"),
            ..Default::default()
        }
    }
}

#[tokio::test]
async fn hooks_run_per_call_and_a_blocked_call_leaves_the_rest() {
    let log = Log::default();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut hooks = HookSet::new();
    for event in [HookEvent::PreToolUse, HookEvent::PostToolUse] {
        hooks.add(Hook::new(
            event,
            Matcher::parse(None),
            HookSource::User,
            Arc::new(Watch {
                seen: seen.clone(),
                event,
            }),
        ));
    }
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Slow::new("look", true, &log)));
    let m = model(&[
        ("look", nap("a", 100)),
        ("look", nap("blocked", 100)),
        ("look", nap("c", 100)),
    ]);
    let mut a = agent(m, tools, 4).with_hooks(hooks);
    let (out, _) = run(&mut a).await;
    assert_eq!(out.unwrap(), "done");
    assert!(overlap(&log, "a", "c"));
    assert!(log.lock().unwrap().iter().all(|(t, _, _)| t != "blocked"));
    let got = results(&a);
    assert_eq!(
        got[0].1,
        "a ok\n\n[hook: PreToolUse] seen c0\n\n[hook: PostToolUse] seen c0"
    );
    assert!(got[1]
        .1
        .starts_with("error: not run: a PreToolUse hook blocked it: not this one"));
    assert_eq!(
        got[2].1,
        "c ok\n\n[hook: PreToolUse] seen c2\n\n[hook: PostToolUse] seen c2"
    );
    // Every PreToolUse before any tool ran; PostToolUse in order, and
    // not for the blocked call.
    let seen = seen.lock().unwrap();
    let pre: Vec<_> = seen
        .iter()
        .filter(|(e, _)| *e == HookEvent::PreToolUse)
        .map(|(_, id)| id.as_str())
        .collect();
    let post: Vec<_> = seen
        .iter()
        .filter(|(e, _)| *e == HookEvent::PostToolUse)
        .map(|(_, id)| id.as_str())
        .collect();
    assert_eq!(pre, ["c0", "c1", "c2"]);
    assert_eq!(post, ["c0", "c2"]);
}

#[tokio::test]
async fn a_stop_mid_batch_keeps_what_finished_and_marks_the_rest() {
    let log = Log::default();
    let stop = StopFlag::new();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Slow::new("look", true, &log)));
    tools.register(Arc::new(Slow {
        stop: Some(stop.clone()),
        ..Slow::new("stopper", true, &log)
    }));
    let m = model(&[
        ("look", nap("quick", 1)),
        ("stopper", nap("s", 5_000)),
        ("look", nap("long", 5_000)),
        ("edit", nap("never", 1)),
    ]);
    let mut a = agent(m, tools, 4).with_stop_flag(stop);
    let started = Instant::now();
    let (out, events) = run(&mut a).await;
    assert!(matches!(out, Err(CoreError::Aborted(_))), "{out:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "waited for the tools"
    );
    let got = results(&a);
    assert_eq!(got.len(), 4, "every call needs a result");
    assert_eq!(got[0].1, "quick ok");
    for (_, content) in &got[1..] {
        assert_eq!(content, "not run: the agent was stopped");
    }
    // Every call announced got a finish.
    let started_ids = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallStarted { .. }))
        .count();
    let finished_ids = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallFinished { .. }))
        .count();
    assert_eq!((started_ids, finished_ids), (3, 3));
}

/// Halts the run once `halt` is sent.
struct Halter(watch::Receiver<bool>);

#[async_trait::async_trait]
impl Guard for Halter {
    fn before_model_call(&self) -> Option<String> {
        None
    }
    async fn before_tool_call(&self, _: GuardedCall<'_>) -> Verdict {
        Verdict::Allow
    }
    async fn halted(&self) -> String {
        let mut rx = self.0.clone();
        let _ = rx.wait_for(|h| *h).await;
        "over budget".into()
    }
}

/// Sends the halt when called.
struct Trip(watch::Sender<bool>);

#[async_trait::async_trait]
impl Tool for Trip {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "trip".into(),
            description: "halts".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, _: Value, _: &ToolContext) -> Result<ToolOutput, CoreError> {
        let _ = self.0.send(true);
        Ok(ToolOutput::ok("tripped"))
    }
    fn read_only(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn a_halt_mid_batch_ends_the_run_with_every_call_answered() {
    let log = Log::default();
    let (tx, rx) = watch::channel(false);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Slow::new("look", true, &log)));
    tools.register(Arc::new(Trip(tx)));
    let m = model(&[
        ("look", nap("long", 5_000)),
        ("trip", json!({})),
        ("look", nap("long2", 5_000)),
    ]);
    let mut a = agent(m.clone(), tools, 4).with_guard(Arc::new(Halter(rx)));
    let started = Instant::now();
    let (out, _) = run(&mut a).await;
    assert_eq!(out.unwrap(), "over budget");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(a.incomplete.as_deref(), Some("over budget"));
    let got = results(&a);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].1, "not run: ferrule halted the run");
    assert_eq!(got[2].1, "not run: ferrule halted the run");
    // No second model call.
    assert_eq!(m.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_serial_group_and_the_cap_hold_inside_a_batch() {
    let log = Log::default();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Slow {
        group: Some("mcp:one"),
        ..Slow::new("mcp__one__a", true, &log)
    }));
    tools.register(Arc::new(Slow {
        group: Some("mcp:one"),
        ..Slow::new("mcp__one__b", true, &log)
    }));
    tools.register(Arc::new(Slow::new("look", true, &log)));
    let m = model(&[
        ("mcp__one__a", nap("a", 150)),
        ("mcp__one__b", nap("b", 150)),
        ("look", nap("x", 150)),
    ]);
    let mut a = agent(m, tools, 4);
    let (out, _) = run(&mut a).await;
    assert_eq!(out.unwrap(), "done");
    assert!(
        !overlap(&log, "a", "b"),
        "one stdio server took two at once"
    );
    assert!(overlap(&log, "a", "x"));

    // A cap of 2: of four calls, never three at once.
    let log = Log::default();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Slow::new("look", true, &log)));
    let tags = ["p", "q", "r", "s"];
    let calls: Vec<_> = tags.iter().map(|t| ("look", nap(t, 100))).collect();
    let mut a = agent(model(&calls), tools, 2);
    let (out, _) = run(&mut a).await;
    assert_eq!(out.unwrap(), "done");
    let spans: Vec<_> = tags.iter().map(|t| span(&log, t)).collect();
    for (s, _) in &spans {
        let running = spans.iter().filter(|(a, b)| a <= s && s < b).count();
        assert!(running <= 2, "{running} at once");
    }
}

#[tokio::test]
async fn parallel_off_runs_every_call_one_after_another() {
    let log = Log::default();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Slow::new("look", true, &log)));
    let m = model(&[("look", nap("a", 60)), ("look", nap("b", 60))]);
    let rows = Arc::new(Rows::default());
    let mut a = agent(m, tools, 1).with_ledger(rows.clone(), "run", None, "m");
    let (out, events) = run(&mut a).await;
    assert_eq!(out.unwrap(), "done");
    assert!(!overlap(&log, "a", "b"));
    // Started, finished, started, finished: as before M27.
    let kinds: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallStarted { id, .. } => Some(format!("+{id}")),
            AgentEvent::ToolCallFinished { id, .. } => Some(format!("-{id}")),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["+c0", "-c0", "+c1", "-c1"]);
    let batch = rows.0.lock().unwrap()[1].speed.clone().unwrap().tool_batch;
    assert_eq!(batch.map(|b| (b.calls, b.parallel)), Some((2, 0)));
}
