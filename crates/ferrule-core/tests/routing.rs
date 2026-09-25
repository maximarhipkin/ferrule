//! M25 routing through the agent loop: a weak and a strong scripted model
//! behind [`Tiered`], one test per trigger, stickiness within a turn and
//! de-escalation at the next, and the ledger's tier and reason.

use ferrule_core::lifecycle::{Hook, HookHandler, HookInput, HookRun, HookSource, Matcher};
use ferrule_core::message::ToolCall;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::{
    Agent, AgentConfig, AgentEvent, CoreError, HarnessProfile, HookEvent, HookSet, LedgerRecord,
    LedgerSink, Message, Policy, Provider, Served, Tier, Tiered, ToolRegistry, Usage, Verifier,
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

type Brain = dyn Fn(&CompletionRequest, usize) -> Result<Message, CoreError> + Send + Sync;

/// A model that answers from a script of its own call count.
struct Scripted {
    name: &'static str,
    brain: Box<Brain>,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(
        name: &'static str,
        brain: impl Fn(&CompletionRequest, usize) -> Result<Message, CoreError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            name,
            brain: Box::new(brain),
            calls: AtomicUsize::new(0),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for Scripted {
    fn name(&self) -> &str {
        self.name
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        (self.brain)(&req, n).map(|message| CompletionResponse {
            message,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 10,
                ..Usage::default()
            },
        })
    }
}

fn call(name: &str, args: serde_json::Value) -> Message {
    Message::assistant(
        None,
        vec![ToolCall {
            id: format!("c-{name}-{args}"),
            name: name.into(),
            arguments: args,
        }],
        None,
    )
}

fn say(text: &str) -> Message {
    Message::assistant(Some(text.into()), vec![], None)
}

/// Writes a file; `path` is required.
struct Edit;

#[async_trait::async_trait]
impl Tool for Edit {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".into(),
            description: "write a file".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }
    }
    async fn call(
        &self,
        args: serde_json::Value,
        _: &ToolContext,
    ) -> Result<ToolOutput, CoreError> {
        Ok(ToolOutput::ok(format!(
            "wrote {}",
            args["path"].as_str().unwrap_or("")
        )))
    }
    fn changes_files(&self) -> bool {
        true
    }
}

/// Reads something; `what` is required.
struct Look;

#[async_trait::async_trait]
impl Tool for Look {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "look".into(),
            description: "look something up".into(),
            parameters: json!({
                "type": "object",
                "properties": {"what": {"type": "string"}},
                "required": ["what"]
            }),
        }
    }
    async fn call(
        &self,
        args: serde_json::Value,
        _: &ToolContext,
    ) -> Result<ToolOutput, CoreError> {
        Ok(ToolOutput::ok(format!(
            "nothing about {}",
            args["what"].as_str().unwrap_or("")
        )))
    }
    fn changes_files(&self) -> bool {
        false
    }
}

/// Passes once `fixed` is set.
struct Check(Arc<AtomicBool>);

#[async_trait::async_trait]
impl Verifier for Check {
    fn describe(&self) -> String {
        "make test".into()
    }
    async fn verify(&self, _: &ToolContext) -> Result<(), String> {
        if self.0.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("1 test failed".into())
        }
    }
}

#[derive(Default)]
struct Rows(Mutex<Vec<LedgerRecord>>);

impl LedgerSink for Rows {
    fn record(&self, record: LedgerRecord) {
        self.0.lock().unwrap().push(record);
    }
}

fn served(model: &str) -> Served {
    Served {
        provider: "mock".into(),
        model: model.into(),
    }
}

fn tiered(weak: Arc<Scripted>, strong: Arc<Scripted>, policy: Policy) -> Arc<Tiered> {
    Arc::new(Tiered::new(
        "routed",
        vec![
            Tier {
                name: "cheap".into(),
                provider: weak,
                served: served("weak"),
            },
            Tier {
                name: "strong".into(),
                provider: strong,
                served: served("strong"),
            },
        ],
        policy,
    ))
}

struct Setup {
    agent: Agent,
    rows: Arc<Rows>,
    _dir: tempfile::TempDir,
}

fn setup(provider: Arc<dyn Provider>, check: Option<Arc<AtomicBool>>) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Edit));
    tools.register(Arc::new(Look));
    let rows = Arc::new(Rows::default());
    let mut agent = Agent::new(
        provider,
        tools,
        HarnessProfile::generic(),
        AgentConfig::default(),
        ToolContext {
            workspace: dir.path().to_path_buf(),
            max_output_chars: 40_000,
        },
        None,
    )
    .with_system_prompt("You are a test agent.")
    .with_ledger(rows.clone(), "run", None, "weak");
    if let Some(fixed) = check {
        agent = agent.with_verifier(Arc::new(Check(fixed)));
    }
    Setup {
        agent,
        rows,
        _dir: dir,
    }
}

async fn run(agent: &mut Agent, goal: &str) -> (String, Vec<AgentEvent>) {
    let (tx, mut rx) = mpsc::channel(4096);
    let answer = agent.run(goal, tx).await.unwrap();
    let events = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    (answer, events)
}

fn escalations(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Escalated { from, to, reason } => Some(format!("{from}>{to}:{reason}")),
            _ => None,
        })
        .collect()
}

/// `(model, tier, escalated)` of each row.
fn route(rows: &Rows) -> Vec<(String, String, Option<String>)> {
    rows.0
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            let tag = r.route.clone().expect("a routed row has a tag");
            (r.model.clone(), tag.tier, tag.escalated)
        })
        .collect()
}

/// The weak model claims it's done without fixing anything; the check
/// fails, the turn moves up, the strong model fixes it.
#[tokio::test]
async fn a_failed_check_moves_the_turn_up_and_the_strong_model_finishes() {
    let fixed = Arc::new(AtomicBool::new(false));
    let weak = Scripted::new("weak", |_, n| {
        Ok(match n {
            1 => call("edit", json!({"path": "a.rs"})),
            _ => say("done"),
        })
    });
    let f = fixed.clone();
    let strong = Scripted::new("strong", move |_, n| {
        Ok(match n {
            1 => {
                f.store(true, Ordering::SeqCst);
                call("edit", json!({"path": "a.rs", "fix": true}))
            }
            _ => say("fixed properly"),
        })
    });
    let mut s = setup(
        tiered(weak.clone(), strong.clone(), Policy::default()),
        Some(fixed),
    );
    let (answer, events) = run(&mut s.agent, "fix the bug").await;
    assert_eq!(answer, "fixed properly");
    assert_eq!(escalations(&events), ["cheap>strong:check_failed"]);
    assert_eq!((weak.calls(), strong.calls()), (2, 2));
    let rows = route(&s.rows);
    assert_eq!(
        rows,
        [
            ("weak".into(), "cheap".into(), None),
            ("weak".into(), "cheap".into(), None),
            (
                "strong".into(),
                "strong".into(),
                Some("check_failed".into())
            ),
            ("strong".into(), "strong".into(), None),
        ]
    );
}

/// A Stop hook that sends the answer back once.
struct Nope(AtomicUsize);

#[async_trait::async_trait]
impl HookHandler for Nope {
    fn command(&self) -> String {
        "nope".into()
    }
    async fn run(&self, _: &HookInput, _: &ToolContext) -> HookRun {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        HookRun {
            exit_code: Some(if first { 2 } else { 0 }),
            stderr: "say which file you changed".into(),
            ..Default::default()
        }
    }
}

#[tokio::test]
async fn a_stop_hook_sending_the_answer_back_moves_the_turn_up() {
    let weak = Scripted::new("weak", |_, _| Ok(say("done")));
    let strong = Scripted::new("strong", |_, _| Ok(say("done: a.rs")));
    let mut s = setup(tiered(weak, strong, Policy::default()), None);
    let mut hooks = HookSet::new();
    hooks.add(Hook::new(
        HookEvent::Stop,
        Matcher::parse(None),
        HookSource::User,
        Arc::new(Nope(AtomicUsize::new(0))),
    ));
    s.agent.add_hooks(hooks);
    let (answer, events) = run(&mut s.agent, "change a file").await;
    assert_eq!(answer, "done: a.rs");
    assert_eq!(escalations(&events), ["cheap>strong:stop_hook"]);
}

/// One bad tool call is forgiven; the second in a row moves the turn up.
#[tokio::test]
async fn two_invalid_tool_calls_in_a_row_move_the_turn_up_one_does_not() {
    let weak = Scripted::new("weak", |_, n| {
        Ok(match n {
            1 => call("look", json!({})),
            2 => call("look", json!({"what": "x"})),
            3 => call("nosuch", json!({"what": "x"})),
            4 => call("look", json!("not an object")),
            _ => say("weak done"),
        })
    });
    let strong = Scripted::new("strong", |_, _| Ok(say("strong done")));
    let mut s = setup(tiered(weak.clone(), strong, Policy::default()), None);
    let (answer, events) = run(&mut s.agent, "look").await;
    assert_eq!(answer, "strong done");
    assert_eq!(escalations(&events), ["cheap>strong:tool_errors"]);
    assert_eq!(weak.calls(), 4, "calls 3 and 4 were the two in a row");
}

#[tokio::test]
async fn the_same_call_three_times_is_no_progress() {
    let weak = Scripted::new("weak", |_, n| {
        Ok(if n <= 3 {
            call("look", json!({"what": "x"}))
        } else {
            say("weak done")
        })
    });
    let strong = Scripted::new("strong", |_, _| Ok(say("strong done")));
    let mut s = setup(tiered(weak.clone(), strong, Policy::default()), None);
    let (answer, events) = run(&mut s.agent, "look").await;
    assert_eq!(answer, "strong done");
    assert_eq!(escalations(&events), ["cheap>strong:no_progress"]);
    assert_eq!(weak.calls(), 3);
}

/// With the repeat count out of reach, the stuck detector's nudge is the
/// no-progress signal.
#[tokio::test]
async fn the_stuck_nudge_is_no_progress_too() {
    let weak = Scripted::new("weak", |_, n| {
        // Alternating calls: never three the same in a row, but a loop.
        Ok(call(
            "look",
            json!({"what": if n % 2 == 0 { "a" } else { "b" }}),
        ))
    });
    let strong = Scripted::new("strong", |_, _| Ok(say("strong done")));
    let policy = Policy {
        no_progress: 100,
        ..Policy::default()
    };
    let mut s = setup(tiered(weak, strong, policy), None);
    let (answer, events) = run(&mut s.agent, "look").await;
    assert_eq!(answer, "strong done");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Stuck { .. })));
    assert_eq!(escalations(&events), ["cheap>strong:no_progress"]);
}

/// A failure a stronger model may not repeat: the same request goes to the
/// strong tier at once. The failed row stays "retried"; the next row says
/// why it moved.
#[tokio::test]
async fn a_call_the_cheap_model_cannot_answer_goes_again_on_the_strong_one() {
    let weak = Scripted::new("weak", |_, _| {
        Err(CoreError::Provider(
            "HTTP 400: This model's maximum context length is 8192 tokens".into(),
        ))
    });
    let strong = Scripted::new("strong", |_, _| Ok(say("answered")));
    let mut s = setup(tiered(weak.clone(), strong, Policy::default()), None);
    let (answer, events) = run(&mut s.agent, "a long question").await;
    assert_eq!(answer, "answered");
    assert_eq!(
        escalations(&events),
        ["cheap>strong:call_failed:context_too_long"]
    );
    let rows = s.rows.0.lock().unwrap().clone();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].outcome, "retried");
    assert_eq!(rows[0].model, "weak");
    assert_eq!(
        rows[1].route.as_ref().unwrap().escalated.as_deref(),
        Some("call_failed:context_too_long")
    );
}

/// Outages are M21's (retry, then fallback), and a refused key is the
/// owner's to fix: neither moves up a tier.
#[tokio::test]
async fn outages_and_auth_errors_do_not_escalate() {
    let weak = Scripted::new("weak", |_, n| {
        if n == 1 {
            Err(CoreError::Transient {
                message: "HTTP 503".into(),
                retry_after: Some(std::time::Duration::from_millis(1)),
            })
        } else {
            Ok(say("back"))
        }
    });
    let strong = Scripted::new("strong", |_, _| Ok(say("strong")));
    let mut s = setup(tiered(weak, strong.clone(), Policy::default()), None);
    let (answer, events) = run(&mut s.agent, "hi").await;
    assert_eq!(answer, "back");
    assert!(escalations(&events).is_empty());

    let weak = Scripted::new("weak", |_, _| {
        Err(CoreError::Provider("HTTP 401: invalid api key".into()))
    });
    let mut s = setup(tiered(weak, strong.clone(), Policy::default()), None);
    let (tx, _rx) = mpsc::channel(4096);
    assert!(s.agent.run("hi", tx).await.is_err());
    assert_eq!(strong.calls(), 0);
}

/// Sticky for the turn; the next turn starts cheap again, or stays up
/// with `de_escalate = false`.
#[tokio::test]
async fn an_escalation_lasts_the_turn_and_the_next_turn_starts_cheap() {
    for de_escalate in [true, false] {
        let weak = Scripted::new("weak", |_, n| {
            Ok(match n {
                1 => call("look", json!({})),
                2 => call("look", json!({"x": 1})),
                _ => say("weak answer"),
            })
        });
        let strong = Scripted::new("strong", |_, n| {
            Ok(match n {
                1 => call("look", json!({"what": "a"})),
                _ => say("strong answer"),
            })
        });
        let policy = Policy {
            de_escalate,
            ..Policy::default()
        };
        let mut s = setup(tiered(weak.clone(), strong.clone(), policy), None);
        let (first, _) = run(&mut s.agent, "one").await;
        assert_eq!(first, "strong answer", "strong for the rest of the turn");
        let (second, events) = run(&mut s.agent, "two").await;
        assert!(escalations(&events).is_empty());
        if de_escalate {
            assert_eq!(second, "weak answer");
            assert_eq!(s.rows.0.lock().unwrap().last().unwrap().model, "weak");
        } else {
            assert_eq!(second, "strong answer");
        }
    }
}

/// With every trigger off, nothing moves.
#[tokio::test]
async fn triggers_that_are_off_do_not_fire() {
    let fixed = Arc::new(AtomicBool::new(false));
    let weak = Scripted::new("weak", |_, n| {
        Ok(match n {
            1 => call("edit", json!({"path": "a"})),
            2 => call("look", json!({})),
            3 => call("look", json!({})),
            _ => say("done"),
        })
    });
    let strong = Scripted::new("strong", |_, _| Ok(say("strong")));
    let policy = Policy {
        de_escalate: true,
        call_failed: false,
        tool_errors: 0,
        checks: false,
        stop_hooks: false,
        no_progress: 0,
        watchdog: false,
    };
    let mut s = setup(tiered(weak, strong.clone(), policy), Some(fixed.clone()));
    let (tx, mut rx) = mpsc::channel(4096);
    let _ = s.agent.run("go", tx).await;
    let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(escalations(&events).is_empty());
    assert_eq!(strong.calls(), 0);
}
