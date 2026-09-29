//! M25's promise that routing off changes nothing: a session that hits
//! every routing trigger (a failed check, a Stop hook, invalid tool calls,
//! the same call repeated, a stuck loop, a malformed answer) runs on a
//! provider that doesn't route, and what the model was sent, the events and
//! the ledger rows must match `golden/routing_off.json`. The golden was
//! written by the code before M25; `FERRULE_UPDATE_GOLDEN=1` rewrites it.

use ferrule_core::lifecycle::{Hook, HookHandler, HookInput, HookRun, HookSource, Matcher};
use ferrule_core::message::ToolCall;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::{
    Agent, AgentConfig, AgentEvent, CoreError, HarnessProfile, HookEvent, HookSet, LedgerRecord,
    LedgerSink, Message, Provider, ToolRegistry, Usage, Verifier,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// One model, scripted by its call count; keeps every request.
struct Model {
    calls: AtomicUsize,
    fixed: Arc<AtomicBool>,
    seen: Mutex<Vec<Value>>,
}

fn call(name: &str, args: Value) -> Message {
    Message::assistant(
        None,
        vec![ToolCall {
            id: format!("c{}", name.len()),
            name: name.into(),
            arguments: args,
        }],
        None,
    )
}

fn say(text: &str) -> Message {
    Message::assistant(Some(text.into()), vec![], None)
}

#[async_trait::async_trait]
impl Provider for Model {
    fn name(&self) -> &str {
        "plain"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.seen.lock().unwrap().push(json!({
            "messages": serde_json::to_value(&req.messages).unwrap(),
            "tools": req.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
        }));
        let message = match n {
            1 => call("edit", json!({"path": "a.rs"})),
            2 => say("done"),
            3 => call("look", json!({})),
            4 => call("nosuch", json!({"what": "x"})),
            5 => call("look", json!("not an object")),
            6..=8 => call("look", json!({"what": "same"})),
            9 => {
                self.fixed.store(true, Ordering::SeqCst);
                call("edit", json!({"path": "a.rs", "really": true}))
            }
            10 => say("done now"),
            11 => say("done: a.rs"),
            12 => return Err(CoreError::MalformedResponse("no choices".into())),
            13 => return Err(CoreError::Provider("HTTP 400: context window".into())),
            _ => say("second turn"),
        };
        Ok(CompletionResponse {
            message,
            usage: Usage {
                input_tokens: 100 + n as u64,
                output_tokens: 10,
                ..Usage::default()
            },
        })
    }
}

struct Edit;

#[async_trait::async_trait]
impl Tool for Edit {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".into(),
            description: "write a file".into(),
            parameters: json!({"type": "object", "required": ["path"]}),
        }
    }
    async fn call(&self, args: Value, _: &ToolContext) -> Result<ToolOutput, CoreError> {
        Ok(ToolOutput::ok(format!("wrote {}", args["path"])))
    }
    fn changes_files(&self) -> bool {
        true
    }
}

struct Look;

#[async_trait::async_trait]
impl Tool for Look {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "look".into(),
            description: "look something up".into(),
            parameters: json!({"type": "object", "required": ["what"]}),
        }
    }
    async fn call(&self, args: Value, _: &ToolContext) -> Result<ToolOutput, CoreError> {
        Ok(ToolOutput::ok(format!("nothing about {}", args["what"])))
    }
    fn changes_files(&self) -> bool {
        false
    }
}

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

#[derive(Default)]
struct Rows(Mutex<Vec<LedgerRecord>>);

impl LedgerSink for Rows {
    fn record(&self, record: LedgerRecord) {
        self.0.lock().unwrap().push(record);
    }
}

/// Drop what differs from run to run: times, durations, the session id.
fn steady(mut v: Value) -> Value {
    match &mut v {
        Value::Object(map) => {
            for k in ["timestamp", "latency_ms", "duration_ms", "session_id"] {
                map.remove(k);
            }
            for (_, x) in map.iter_mut() {
                *x = steady(x.take());
            }
        }
        Value::Array(items) => {
            for x in items.iter_mut() {
                *x = steady(x.take());
            }
        }
        _ => {}
    }
    v
}

#[tokio::test]
async fn with_routing_off_a_session_runs_exactly_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let fixed = Arc::new(AtomicBool::new(false));
    let model = Arc::new(Model {
        calls: AtomicUsize::new(0),
        fixed: fixed.clone(),
        seen: Mutex::default(),
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Edit));
    tools.register(Arc::new(Look));
    let rows = Arc::new(Rows::default());
    let mut hooks = HookSet::new();
    hooks.add(Hook::new(
        HookEvent::Stop,
        Matcher::parse(None),
        HookSource::User,
        Arc::new(Nope(AtomicUsize::new(0))),
    ));
    let mut agent = Agent::new(
        model.clone(),
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
    .with_ledger(rows.clone(), "run", None, "the-model")
    .with_verifier(Arc::new(Check(fixed)))
    .with_hooks(hooks);

    let mut answers = Vec::new();
    let mut events: Vec<AgentEvent> = Vec::new();
    for goal in ["fix the bug", "a malformed answer", "too long", "and again"] {
        let (tx, mut rx) = mpsc::channel(4096);
        let answer = agent.run(goal, tx).await.map_err(|e| e.to_string());
        answers.push(json!(answer));
        events.extend(std::iter::from_fn(|| rx.try_recv().ok()));
    }
    let got = steady(json!({
        "answers": answers,
        "sent": *model.seen.lock().unwrap(),
        "events": serde_json::to_value(&events).unwrap(),
        "ledger": serde_json::to_value(&*rows.0.lock().unwrap()).unwrap(),
    }));
    let text = serde_json::to_string_pretty(&got).unwrap() + "\n";
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/routing_off.json");
    if std::env::var_os("FERRULE_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
    }
    let want = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\r\n", "\n");
    assert!(
        want == text,
        "routing off changed the session; diff against {}:\n{text}",
        path.display()
    );
}
