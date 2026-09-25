//! M19 end to end: a real `Agent` loop under a `TrustGuard`, with a
//! scripted model, recording tools, a scripted owner on "Telegram", a fake
//! clock and ledgers in temp dirs. Nothing here touches the network or the
//! owner's real data directory.

use async_trait::async_trait;
use ferrule_core::message::ToolCall;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::{ToolDefinition, ToolOutput};
use ferrule_core::{
    Agent, AgentConfig, CoreError, HarnessProfile, LedgerRecord, LedgerSink, Message, Provider,
    Role, Tool, ToolContext, ToolRegistry, Usage,
};
use ferrule_trust::{
    FakeClock, Hub, Intercept, Notifier, Prompter, Route, TrustConfig, TrustGuard, TrustSink,
};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::sync::mpsc;

// ---- the model -------------------------------------------------------

/// Answers from a script, `tokens` input tokens per call, and keeps every
/// request so a test can read what the tools returned.
struct Model {
    script: Mutex<Vec<Message>>,
    tokens: u64,
    calls: Mutex<usize>,
    seen: Mutex<Vec<Vec<Message>>>,
}

impl Model {
    fn new(script: Vec<Message>, tokens: u64) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            tokens,
            calls: Mutex::new(0),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }

    /// Every tool result the model was shown, in order.
    fn tool_results(&self) -> Vec<String> {
        let seen = self.seen.lock().unwrap();
        let last = seen.last().cloned().unwrap_or_default();
        last.iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| m.content.clone())
            .collect()
    }
}

#[async_trait]
impl Provider for Model {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        *self.calls.lock().unwrap() += 1;
        self.seen.lock().unwrap().push(req.messages);
        let mut script = self.script.lock().unwrap();
        let message = if script.is_empty() {
            Message::assistant(Some("done".into()), vec![], None)
        } else {
            script.remove(0)
        };
        Ok(CompletionResponse {
            message,
            usage: Usage {
                input_tokens: self.tokens,
                output_tokens: 0,
                cached_input_tokens: 0,
            },
        })
    }
}

fn call(tool: &str, args: Value) -> Message {
    Message::assistant(
        None,
        vec![ToolCall {
            id: format!("c-{tool}"),
            name: tool.into(),
            arguments: args,
        }],
        None,
    )
}

fn shell(cmd: &str) -> Message {
    call("shell", json!({ "command": cmd }))
}

// ---- the tools -------------------------------------------------------

/// Records what it was asked to do and does nothing; `delay` makes it slow.
struct Recorder {
    name: &'static str,
    changes: bool,
    delay: Duration,
    ran: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for Recorder {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "test".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        tokio::time::sleep(self.delay).await;
        let what = args
            .get("command")
            .and_then(|c| c.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| args.to_string());
        self.ran
            .lock()
            .unwrap()
            .push(format!("{}: {what}", self.name));
        Ok(ToolOutput::ok(format!("ran {what}")))
    }
    fn changes_files(&self) -> bool {
        self.changes
    }
}

/// A tool that runs a sub-agent (same tree, a child guard) to completion.
struct Spawn {
    hub: Arc<Hub>,
    parent: TrustGuard,
    tokens: u64,
    calls: usize,
    sink: Arc<dyn LedgerSink>,
}

#[async_trait]
impl Tool for Spawn {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agent".into(),
            description: "test".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let script = (0..self.calls)
            .map(|_| call("read_file", json!({"path": "x"})))
            .collect();
        let child = self.parent.child();
        let mut agent = agent(
            Model::new(script, self.tokens),
            tools(&Arc::default(), Duration::ZERO),
        )
        .with_ledger(
            Arc::new(TrustSink::new(
                self.sink.clone(),
                self.hub.clone(),
                child.tree(),
                None,
            )),
            "run",
            None,
            "m",
        )
        .with_guard(Arc::new(child));
        let (tx, _rx) = mpsc::channel(4096);
        let answer = agent.run("child task", tx).await?;
        Ok(ToolOutput::ok(answer))
    }
    fn changes_files(&self) -> bool {
        false
    }
}

fn tools(ran: &Arc<Mutex<Vec<String>>>, delay: Duration) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    for (name, changes) in [("shell", true), ("write_file", true), ("read_file", false)] {
        reg.register(Arc::new(Recorder {
            name,
            changes,
            delay: if name == "shell" {
                delay
            } else {
                Duration::ZERO
            },
            ran: ran.clone(),
        }));
    }
    reg
}

fn agent(model: Arc<Model>, reg: ToolRegistry) -> Agent {
    Agent::new(
        model,
        reg,
        HarnessProfile::generic(),
        AgentConfig {
            max_iterations: 30,
            ..Default::default()
        },
        ToolContext::default(),
        None,
    )
}

// ---- the ledger, the owner, the world --------------------------------

/// The CLI's file sink, near enough: one JSON line per row, timed by the
/// test's clock (the agent stamps rows with the real one).
struct FileSink(PathBuf, Arc<FakeClock>);

impl LedgerSink for FileSink {
    fn record(&self, mut r: LedgerRecord) {
        if r.session_id == "ephemeral" {
            r.timestamp = ferrule_trust::Clock::now(&*self.1).to_rfc3339();
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)
            .unwrap();
        writeln!(f, "{}", serde_json::to_string(&r).unwrap()).unwrap();
    }
}

/// The owner on Telegram: records what the bot sent, and answers each
/// question with the next scripted reply (through the hub, as the gateway
/// would). `fail`: Telegram is down.
#[derive(Default)]
struct Owner {
    hub: OnceLock<Weak<Hub>>,
    sent: Mutex<Vec<(i64, String)>>,
    replies: Mutex<Vec<&'static str>>,
    fail: bool,
}

#[async_trait]
impl Notifier for Owner {
    async fn send(&self, chat: i64, text: &str) -> Result<(), String> {
        if self.fail {
            return Err("telegram: connection refused".into());
        }
        self.sent.lock().unwrap().push((chat, text.to_string()));
        if text.contains("(code ") {
            let reply = {
                let mut r = self.replies.lock().unwrap();
                (!r.is_empty()).then(|| r.remove(0))
            };
            if let (Some(reply), Some(hub)) = (reply, self.hub.get().and_then(Weak::upgrade)) {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    hub.intercept(OWNER, reply);
                });
            }
        }
        Ok(())
    }
}

impl Owner {
    fn sent(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| t.clone())
            .collect()
    }
}

const OWNER: i64 = 4242;

struct World {
    _dir: tempfile::TempDir,
    data: PathBuf,
    ledger: PathBuf,
    clock: Arc<FakeClock>,
    hub: Arc<Hub>,
    owner: Arc<Owner>,
}

impl World {
    fn new(cfg: TrustConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let ledger = data.join("ledger.jsonl");
        std::fs::create_dir_all(&data).unwrap();
        let clock = Arc::new(FakeClock::at("2026-09-25T10:00:00Z"));
        let owner = Arc::new(Owner::default());
        let hub = hub(cfg, &data, &ledger, clock.clone(), owner.clone());
        Self {
            _dir: dir,
            data,
            ledger,
            clock,
            hub,
            owner,
        }
    }

    /// The same data directory in a new process.
    fn restart(&mut self, cfg: TrustConfig) {
        self.owner = Arc::new(Owner::default());
        self.hub = hub(
            cfg,
            &self.data,
            &self.ledger,
            self.clock.clone(),
            self.owner.clone(),
        );
    }

    fn sink(&self) -> Arc<dyn LedgerSink> {
        Arc::new(FileSink(self.ledger.clone(), self.clock.clone()))
    }

    /// One root run of `tree`: its answer and the model.
    async fn run(&self, tree: &str, route: Route, script: Vec<Message>, tokens: u64) -> Run {
        self.run_with(
            TrustGuard::root(self.hub.clone(), tree, route),
            script,
            tokens,
            Duration::ZERO,
        )
        .await
    }

    async fn run_with(
        &self,
        guard: TrustGuard,
        script: Vec<Message>,
        tokens: u64,
        delay: Duration,
    ) -> Run {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let model = Model::new(script, tokens);
        let mut reg = tools(&ran, delay);
        reg.register(Arc::new(Spawn {
            hub: self.hub.clone(),
            parent: guard.clone(),
            tokens,
            calls: 4,
            sink: self.sink(),
        }));
        let sink = TrustSink::new(self.sink(), self.hub.clone(), guard.tree(), None);
        let mut agent = agent(model.clone(), reg)
            .with_ledger(Arc::new(sink), "run", None, "m")
            .with_guard(Arc::new(guard));
        let (tx, _rx) = mpsc::channel(4096);
        let answer = agent.run("the task", tx).await.unwrap();
        let ran = ran.lock().unwrap().clone();
        Run { answer, model, ran }
    }

    fn audit(&self, event: &str) -> Vec<Value> {
        self.hub
            .audit()
            .read(None)
            .unwrap()
            .into_iter()
            .filter(|e| e.event == event)
            .map(|e| e.detail)
            .collect()
    }
}

fn hub(
    cfg: TrustConfig,
    data: &Path,
    ledger: &Path,
    clock: Arc<FakeClock>,
    owner: Arc<Owner>,
) -> Arc<Hub> {
    let bound = vec![ferrule_proxy::HostPattern::parse("api.github.com").unwrap()];
    let hub = Arc::new(
        Hub::new(cfg, data, ledger, clock, bound)
            .unwrap()
            .with_poll(Duration::from_millis(20)),
    );
    owner.hub.set(Arc::downgrade(&hub)).ok();
    hub.set_notifier(Some(owner));
    hub.set_owner(Some(OWNER));
    hub
}

struct Run {
    answer: String,
    model: Arc<Model>,
    ran: Vec<String>,
}

fn reads(n: usize) -> Vec<Message> {
    (0..n)
        .map(|_| call("read_file", json!({"path": "x"})))
        .collect()
}

fn unattended() -> Route {
    Route::Unattended("this is a scheduled task with nobody watching".into())
}

fn telegram() -> Route {
    Route::Owner {
        chat_label: "telegram chat 4242".into(),
    }
}

fn caps() -> TrustConfig {
    TrustConfig {
        gates: true,
        ..TrustConfig::off()
    }
}

// ---- caps ------------------------------------------------------------

#[tokio::test]
async fn a_run_stops_at_its_token_cap_with_the_owners_message_and_no_model_call() {
    let w = World::new(TrustConfig {
        max_tokens_per_run: 2_500,
        ..caps()
    });
    let r = w.run("s1", telegram(), reads(10), 1_000).await;
    assert_eq!(
        r.model.calls(),
        3,
        "1,000 + 1,000 + 1,000 crosses 2,500; no fourth call"
    );
    assert!(
        r.answer
            .starts_with("Stopped: this run reached its token cap (max_tokens_per_run = 2,500)"),
        "{}",
        r.answer
    );
    assert!(
        r.answer.contains("3,000 tokens and $0.00 this run"),
        "{}",
        r.answer
    );
    let stops = w.audit("cap_stop");
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0]["cap"], "max_tokens_per_run");

    // The next run is a new run: its own 2,500.
    let r = w.run("s1", telegram(), reads(1), 1_000).await;
    assert_eq!(r.answer, "done");
}

#[tokio::test]
async fn the_day_cap_counts_every_process_survives_a_restart_and_resets_at_midnight() {
    let cfg = TrustConfig {
        max_tokens_per_day: 5_000,
        timezone: "Asia/Jerusalem".into(),
        ..caps()
    };
    let mut w = World::new(cfg.clone());
    // Another process spent 3,000 today (a row without a tree: pre-M19).
    let mut other = LedgerRecord {
        timestamp: "2026-09-25T08:00:00+00:00".into(),
        session_id: "telegram__7".into(),
        task_shape: "gateway".into(),
        origin: None,
        provider: "p".into(),
        model: "m".into(),
        iteration: 0,
        call_kind: "turn".into(),
        input_tokens: 3_000,
        cached_input_tokens: 0,
        output_tokens: 0,
        tool_calls: 0,
        latency_ms: 1,
        outcome: "ok".into(),
        error_kind: None,
        error_message: None,
        cost_usd: None,
        eval: None,
        tree: None,
    };
    w.sink().record(other.clone());
    // Yesterday's spend (in Jerusalem) doesn't count.
    other.timestamp = "2026-09-24T20:59:00+00:00".into();
    other.input_tokens = 100_000;
    w.sink().record(other);

    let r = w.run("s1", telegram(), reads(10), 1_000).await;
    assert_eq!(r.model.calls(), 2, "3,000 + 2 × 1,000 = 5,000: at the cap");
    assert!(
        r.answer.contains("today's spend reached its token cap"),
        "{}",
        r.answer
    );
    assert!(
        r.answer
            .contains("5,000 tokens and $0.00 today (Asia/Jerusalem)"),
        "{}",
        r.answer
    );

    w.restart(cfg.clone());
    let r = w.run("s2", telegram(), reads(1), 1_000).await;
    assert_eq!(
        r.model.calls(),
        0,
        "a new process reads the day back from the ledger"
    );
    assert!(r.answer.starts_with("Stopped:"));

    // 21:00 UTC is midnight in Jerusalem.
    w.clock.set("2026-09-25T21:00:00Z");
    let r = w.run("s3", telegram(), reads(1), 1_000).await;
    assert_eq!(r.answer, "done");
}

#[tokio::test]
async fn a_scheduled_tasks_cap_covers_all_its_runs_but_not_other_work() {
    let w = World::new(TrustConfig {
        max_tokens_per_task: 3_000,
        ..caps()
    });
    let r = w
        .run("scheduler__nightly", unattended(), reads(1), 1_000)
        .await;
    assert_eq!(r.answer, "done");
    let r = w
        .run("scheduler__nightly", unattended(), reads(5), 1_000)
        .await;
    assert_eq!(r.model.calls(), 1, "2,000 from the first run + 1,000");
    assert!(
        r.answer
            .contains("scheduled task `nightly` today reached its token cap"),
        "{}",
        r.answer
    );
    // An unattended stop is sent to the owner.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(w.owner.sent().iter().any(|t| t.starts_with("Stopped:")));

    let r = w
        .run("scheduler__other", unattended(), reads(1), 1_000)
        .await;
    assert_eq!(r.answer, "done", "another task has its own day");
    let r = w.run("s1", telegram(), reads(1), 1_000).await;
    assert_eq!(r.answer, "done", "a chat isn't a task");
}

#[tokio::test]
async fn dollar_caps_use_the_price_on_the_row() {
    let w = World::new(TrustConfig {
        max_usd_per_run: 0.25,
        ..caps()
    });
    let ran = Arc::new(Mutex::new(Vec::new()));
    let model = Model::new(reads(10), 1_000_000);
    let guard = TrustGuard::root(w.hub.clone(), "s1", telegram());
    let price: ferrule_trust::Pricer =
        Arc::new(|r: &LedgerRecord| Some(r.input_tokens as f64 * 0.1 / 1e6));
    let sink = TrustSink::new(w.sink(), w.hub.clone(), "s1", Some(price));
    let mut a = agent(model.clone(), tools(&ran, Duration::ZERO))
        .with_ledger(Arc::new(sink), "run", None, "m")
        .with_guard(Arc::new(guard));
    let (tx, _rx) = mpsc::channel(4096);
    let answer = a.run("t", tx).await.unwrap();
    assert_eq!(model.calls(), 3, "$0.10 a call: the third reaches $0.30");
    assert!(
        answer.contains("dollar cap (max_usd_per_run = $0.25)"),
        "{answer}"
    );
    let rows = std::fs::read_to_string(&w.ledger).unwrap();
    let row: LedgerRecord = serde_json::from_str(rows.lines().next().unwrap()).unwrap();
    assert_eq!(row.tree.as_deref(), Some("s1"));
    assert!((row.cost_usd.unwrap() - 0.1).abs() < 1e-9);
}

#[tokio::test]
async fn the_warning_is_sent_once_per_window_even_across_a_restart() {
    let cfg = TrustConfig {
        max_tokens_per_run: 10_000,
        max_tokens_per_day: 20_000,
        ..caps()
    };
    let mut w = World::new(cfg.clone());
    // Run 1: 9,000 tokens — 80% of the run cap is crossed once.
    w.run("s1", telegram(), reads(8), 1_000).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sent = w.owner.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        sent[0].starts_with("ferrule: 80% of this run's token cap is spent (8,000 of 10,000)"),
        "{}",
        sent[0]
    );

    // Run 2 takes the day past 16,000: one run warning (a new run) and
    // one day warning, each once however many calls follow.
    w.run("s1", telegram(), reads(8), 1_000).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sent = w.owner.sent();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent
        .iter()
        .any(|t| t.contains("today's (UTC day) token cap")));

    // A restart: today's day warning isn't sent again.
    w.restart(cfg);
    w.run("s2", telegram(), reads(1), 100).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        w.owner.sent().iter().all(|t| !t.contains("today's")),
        "{:?}",
        w.owner.sent()
    );
    assert_eq!(w.audit("cap_warning").len(), 3);
}

#[tokio::test]
async fn an_unreadable_ledger_stops_runs_only_when_a_day_cap_needs_it() {
    let mut w = World::new(TrustConfig {
        max_tokens_per_day: 1_000_000,
        ..caps()
    });
    std::fs::create_dir_all(&w.ledger).unwrap(); // a directory: can't be read
    let r = w.run("s1", telegram(), reads(1), 10).await;
    assert_eq!(r.model.calls(), 0);
    assert!(r.answer.contains("can't be read"), "{}", r.answer);
    assert!(r.answer.contains("the day caps can't be checked"));

    w.restart(TrustConfig {
        max_tokens_per_run: 1_000_000,
        ..caps()
    });
    // No row can be written either; the run cap still works in memory.
    let hub = w.hub.clone();
    let guard = TrustGuard::root(hub.clone(), "s2", telegram());
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mut a =
        agent(Model::new(reads(1), 10), tools(&ran, Duration::ZERO)).with_guard(Arc::new(guard));
    let (tx, _rx) = mpsc::channel(4096);
    assert_eq!(a.run("t", tx).await.unwrap(), "done");
}

// ---- the kill switch -------------------------------------------------

#[tokio::test]
async fn a_stopped_ferrule_runs_nothing_until_cleared_and_it_survives_a_restart() {
    let mut w = World::new(caps());
    w.hub
        .engage("ferrule stop", Some("runaway spend".into()))
        .unwrap();
    w.restart(caps());
    let r = w.run("s1", telegram(), reads(1), 10).await;
    assert_eq!(r.model.calls(), 0);
    assert!(
        r.answer
            .starts_with("Ferrule is stopped (by ferrule stop at "),
        "{}",
        r.answer
    );
    assert!(r.answer.contains("reason: runaway spend"));

    // /resume only from the owner chat.
    assert_eq!(
        w.hub.intercept(99, "/resume"),
        Intercept::Reply("Only the owner chat can resume ferrule.".into())
    );
    assert!(w.hub.stopped().is_some());
    assert!(
        matches!(w.hub.intercept(OWNER, "/resume"), Intercept::Reply(t) if t.starts_with("Resumed"))
    );
    let r = w.run("s1", telegram(), reads(1), 10).await;
    assert_eq!(r.answer, "done");
    let events: Vec<String> = w
        .hub
        .audit()
        .read(None)
        .unwrap()
        .into_iter()
        .map(|e| e.event)
        .collect();
    assert_eq!(events, ["stop_engaged", "stop_cleared"]);
}

#[tokio::test]
async fn stop_halts_a_command_mid_call_and_a_sub_agent_too() {
    let w = World::new(caps());
    let hub = w.hub.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Any allowed chat can stop; the gateway reads it before the lane.
        assert!(matches!(hub.intercept(77, "/stop"), Intercept::Reply(_)));
    });
    let guard = TrustGuard::root(w.hub.clone(), "s1", telegram());
    let started = std::time::Instant::now();
    let r = w
        .run_with(
            guard,
            vec![shell("sleep 30"), shell("echo never")],
            10,
            Duration::from_secs(30),
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "halted mid-call"
    );
    assert!(
        r.answer
            .starts_with("Ferrule is stopped (by telegram chat 77"),
        "{}",
        r.answer
    );
    assert!(
        r.ran.is_empty(),
        "the slow command never finished: {:?}",
        r.ran
    );
    assert_eq!(r.model.calls(), 1);
}

#[tokio::test]
async fn a_sub_agents_spend_is_the_runs_and_crossing_the_cap_halts_the_parent() {
    let w = World::new(TrustConfig {
        max_tokens_per_run: 3_500,
        ..caps()
    });
    // Parent: 1,000 (its first call spawns). Child: 1,000 per call, four
    // calls planned; its third takes the run to 4,000, so the child is
    // stopped by its own check and the parent, waiting in the tool call,
    // is halted by the crossing.
    let r = w
        .run(
            "s1",
            telegram(),
            vec![call("spawn_agent", json!({})), call("read_file", json!({}))],
            1_000,
        )
        .await;
    assert!(
        r.answer
            .starts_with("Stopped: this run reached its token cap"),
        "{}",
        r.answer
    );
    assert_eq!(
        r.model.calls(),
        1,
        "the parent never called the model again"
    );
    assert_eq!(w.hub.run_spend("s1").tokens, 4_000);
    let rows = std::fs::read_to_string(&w.ledger).unwrap();
    assert_eq!(rows.lines().count(), 4);
    assert!(rows.lines().all(|l| l.contains("\"tree\":\"s1\"")));
}

// ---- approvals -------------------------------------------------------

#[tokio::test]
async fn a_gated_command_runs_after_the_owner_says_yes() {
    let w = World::new(caps());
    w.owner.replies.lock().unwrap().push("Yes!");
    let r = w
        .run(
            "s1",
            telegram(),
            vec![shell("git push --force origin main")],
            10,
        )
        .await;
    assert_eq!(r.ran, ["shell: git push --force origin main"]);
    let asked = &w.owner.sent()[0];
    assert!(
        asked.contains("`git push --force origin main` — force push."),
        "{asked}"
    );
    assert!(asked.contains("Reply `yes` to allow it"));
    let answers = w.audit("approval_answered");
    assert_eq!(answers[0]["answer"], "yes");
}

#[tokio::test]
async fn anything_but_yes_refuses_and_the_run_goes_on() {
    let w = World::new(caps());
    w.owner
        .replies
        .lock()
        .unwrap()
        .push("hmm, what's that for?");
    let r = w
        .run(
            "s1",
            telegram(),
            vec![shell("rm -rf target"), shell("ls")],
            10,
        )
        .await;
    assert_eq!(r.ran, ["shell: ls"], "the refused command never ran");
    let results = r.model.tool_results();
    assert!(
        results[0].starts_with("refused by ferrule: `rm -rf target` is a recursive delete, which needs the owner's approval, and the owner refused it"),
        "{}",
        results[0]
    );
    assert!(results[0].ends_with("It was not run."));
    assert_eq!(r.answer, "done");
}

#[tokio::test]
async fn no_answer_in_time_refuses_and_a_late_yes_is_told_so() {
    let w = World::new(TrustConfig {
        approval_timeout_secs: 1,
        ..caps()
    });
    let r = w
        .run("s1", telegram(), vec![shell("rm -rf target")], 10)
        .await;
    assert!(r.ran.is_empty());
    assert!(
        r.model.tool_results()[0].contains("no answer in 1 seconds"),
        "{:?}",
        r.model.tool_results()
    );
    match w.hub.intercept(OWNER, "yes") {
        Intercept::Reply(t) => assert!(t.contains("expired"), "{t}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(w.audit("approval_answered")[0]["answer"], "timeout");
}

#[tokio::test]
async fn unattended_runs_and_a_dead_telegram_refuse_without_waiting() {
    let w = World::new(caps());
    let r = w
        .run(
            "scheduler__nightly",
            unattended(),
            vec![shell("curl -X DELETE https://api.github.com/repos/o/r")],
            10,
        )
        .await;
    assert!(r.ran.is_empty());
    assert!(
        r.model.tool_results()[0]
            .contains("this is a scheduled task with nobody watching, so nobody can approve it"),
        "{:?}",
        r.model.tool_results()
    );
    assert!(w.owner.sent().is_empty(), "nobody was asked");

    let mut w = World::new(caps());
    w.owner = Arc::new(Owner {
        fail: true,
        ..Owner::default()
    });
    w.hub.set_notifier(Some(w.owner.clone()));
    let started = std::time::Instant::now();
    let r = w
        .run("s1", telegram(), vec![shell("git push -f")], 10)
        .await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        r.model.tool_results()[0].contains("couldn't reach the owner"),
        "{:?}",
        r.model.tool_results()
    );

    // Nobody to ask: no owner chat.
    w.hub.set_owner(None);
    let r = w
        .run("s2", telegram(), vec![shell("git push -f")], 10)
        .await;
    assert!(r.model.tool_results()[0].contains("no owner chat is set"));
}

#[tokio::test]
async fn a_halt_while_waiting_withdraws_the_question() {
    let w = World::new(caps());
    let hub = w.hub.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        hub.engage("ferrule stop", None).unwrap();
    });
    let r = w
        .run("s1", telegram(), vec![shell("rm -rf /tmp/x")], 10)
        .await;
    assert!(r.ran.is_empty());
    assert!(r.answer.starts_with("Ferrule is stopped"));
    assert_eq!(w.hub.approvals().pending_in(OWNER), 0);
    assert_eq!(w.audit("approval_answered")[0]["answer"], "halted");
    assert!(matches!(w.hub.intercept(OWNER, "yes"), Intercept::Reply(t) if t.contains("expired")));
}

#[tokio::test]
async fn the_terminal_is_asked_when_the_run_has_one() {
    struct Says(&'static str, Mutex<Vec<String>>);
    impl Prompter for Says {
        fn ask(&self, text: &str) -> Option<String> {
            self.1.lock().unwrap().push(text.into());
            Some(self.0.into())
        }
    }
    let w = World::new(caps());
    let yes = Arc::new(Says("yes\n", Mutex::default()));
    let r = w
        .run(
            "s1",
            Route::Terminal(yes.clone()),
            vec![shell("rm -rf build")],
            10,
        )
        .await;
    assert_eq!(r.ran, ["shell: rm -rf build"]);
    assert!(yes.1.lock().unwrap()[0].contains("`rm -rf build` — recursive delete"));
    let no = Arc::new(Says("y", Mutex::default()));
    let r = w
        .run("s1", Route::Terminal(no), vec![shell("rm -rf build")], 10)
        .await;
    assert!(r.ran.is_empty(), "only yes");
    assert!(w.owner.sent().is_empty(), "the terminal, not Telegram");
}

#[tokio::test]
async fn gates_off_asks_nobody() {
    let w = World::new(TrustConfig::off());
    let r = w
        .run("s1", unattended(), vec![shell("rm -rf target")], 10)
        .await;
    assert_eq!(r.ran, ["shell: rm -rf target"]);
}

#[tokio::test]
async fn a_sub_agent_of_an_unattended_run_is_unattended_too() {
    let w = World::new(caps());
    let parent = TrustGuard::root(w.hub.clone(), "scheduler__t", unattended());
    let child = parent.child();
    let args = json!({"command": "rm -rf x"});
    let v = ferrule_core::Guard::before_tool_call(
        &child,
        ferrule_core::GuardedCall {
            tool: "shell",
            args: &args,
            changes_files: true,
        },
    )
    .await;
    assert!(matches!(v, ferrule_core::Verdict::Refuse(t) if t.contains("nobody can approve it")));
}

/// M20: a connected service's tools that can change things ask first;
/// the ones marked read-only, and other servers' tools, don't.
#[tokio::test]
async fn a_connected_services_write_tool_needs_the_owners_yes() {
    let w = World::new(caps());
    w.hub.set_connected(vec!["notion".into()]);
    let guard = TrustGuard::root(w.hub.clone(), "scheduler__t", unattended());
    let args = json!({"title": "x"});
    let ask = |tool: &'static str, changes: bool| {
        let guard = &guard;
        let args = &args;
        async move {
            ferrule_core::Guard::before_tool_call(
                guard,
                ferrule_core::GuardedCall {
                    tool,
                    args,
                    changes_files: changes,
                },
            )
            .await
        }
    };
    let v = ask("mcp__notion__create_page", true).await;
    assert!(
        matches!(&v, ferrule_core::Verdict::Refuse(t) if t.contains("`mcp__notion__create_page` is a change through a connected service")),
        "{v:?}"
    );
    assert!(matches!(
        ask("mcp__notion__search", false).await,
        ferrule_core::Verdict::Allow
    ));
    assert!(matches!(
        ask("mcp__local__write", true).await,
        ferrule_core::Verdict::Allow
    ));
    w.hub.set_connected(vec![]);
    assert!(matches!(
        ask("mcp__notion__create_page", true).await,
        ferrule_core::Verdict::Allow
    ));
}

// ---- plan mode -------------------------------------------------------

#[tokio::test]
async fn plan_mode_refuses_every_change_and_every_gated_command() {
    let w = World::new(TrustConfig::off());
    let guard = TrustGuard::root(w.hub.clone(), "s1", telegram()).planning(true);
    let child = guard.child();
    assert!(
        child.is_planning(),
        "a sub-agent started while planning plans too"
    );
    let r = w
        .run_with(
            guard,
            vec![
                call("write_file", json!({"path": "a", "content": "b"})),
                shell("git push --force"),
                call("read_file", json!({"path": "a"})),
                shell("ls"),
            ],
            10,
            Duration::ZERO,
        )
        .await;
    assert_eq!(r.ran, ["read_file: {\"path\":\"a\"}", "shell: ls"]);
    let results = r.model.tool_results();
    assert!(
        results[0].contains("this is plan mode: `write_file` can change files"),
        "{}",
        results[0]
    );
    assert!(
        results[1].contains("this is plan mode: `git push --force` is a force push"),
        "{}",
        results[1]
    );
    assert!(
        w.owner.sent().is_empty(),
        "plan mode asks nobody mid-exploration"
    );
}

#[tokio::test]
async fn plan_mode_starts_sub_agents_only_without_a_worktree() {
    use ferrule_core::{Guard as _, GuardedCall, Verdict};
    let w = World::new(TrustConfig::off());
    let guard = TrustGuard::root(w.hub.clone(), "s1", telegram()).planning(true);
    let verdict = |args: Value| {
        let guard = guard.clone();
        async move {
            guard
                .before_tool_call(GuardedCall {
                    tool: "spawn_agent",
                    args: &args,
                    changes_files: false,
                })
                .await
        }
    };
    for args in [
        json!({"task": "look"}),
        json!({"task": "look", "worktree": true}),
    ] {
        assert!(
            matches!(verdict(args).await, Verdict::Refuse(t) if t.contains("own git worktree")),
            "the default worktree makes a branch"
        );
    }
    assert!(matches!(
        verdict(json!({"task": "look", "worktree": false})).await,
        Verdict::Allow
    ));
    // Outside plan mode it's none of the guard's business.
    let plain = TrustGuard::root(w.hub.clone(), "s2", telegram());
    let args = json!({"task": "look"});
    assert!(matches!(
        plain
            .before_tool_call(GuardedCall {
                tool: "spawn_agent",
                args: &args,
                changes_files: false,
            })
            .await,
        Verdict::Allow
    ));
}

#[tokio::test]
async fn slash_plan_and_slash_stop_are_read_before_the_session() {
    let w = World::new(caps());
    assert_eq!(
        w.hub.intercept(OWNER, "/plan tidy the repo"),
        Intercept::Plan("tidy the repo".into())
    );
    assert!(matches!(
        w.hub.intercept(OWNER, "/plan"),
        Intercept::Reply(_)
    ));
    assert_eq!(w.hub.intercept(OWNER, "hello"), Intercept::Pass);
    assert!(matches!(
        w.hub.intercept(OWNER, "/stop@ferrule_bot going out"),
        Intercept::Reply(_)
    ));
    assert_eq!(
        w.hub.stopped().unwrap().reason.as_deref(),
        Some("going out")
    );
}
