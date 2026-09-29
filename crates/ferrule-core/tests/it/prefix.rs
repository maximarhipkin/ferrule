//! M27's cache-stable prefix: every request a session sends starts with the
//! exact bytes of the one before it — the system prompt, the tools in the
//! same order, and the history — across a tool round-trip (a parallel batch
//! included) and across turns, with recalled memory and hook notes in play.
//! The one thing that differs is what's new at the end, which is what a
//! provider's prompt cache needs to hit.

use ferrule_core::lifecycle::{Hook, HookHandler, HookInput, HookRun, HookSource, Matcher};
use ferrule_core::message::ToolCall;
use ferrule_core::provider::{CompletionRequest, CompletionResponse};
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_core::{
    Agent, AgentConfig, CoreError, HarnessProfile, HookEvent, HookSet, Message, Provider,
    SessionRecall, ToolRegistry, Usage,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Turn 1: two lookups in one answer, then text; turn 2: one lookup, then
/// text. Keeps every request as the bytes a driver would be built from.
#[derive(Default)]
struct Model {
    calls: AtomicUsize,
    seen: Mutex<Vec<(String, String, Vec<String>)>>,
}

fn call(id: &str, what: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "look".into(),
        arguments: json!({ "what": what }),
    }
}

#[async_trait::async_trait]
impl Provider for Model {
    fn name(&self) -> &str {
        "plain"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let bytes = |v: Value| serde_json::to_string(&v).unwrap();
        let tools = bytes(serde_json::to_value(&req.tools).unwrap());
        let (system, rest) = req.messages.split_first().unwrap();
        self.seen.lock().unwrap().push((
            bytes(serde_json::to_value(system).unwrap()),
            tools,
            rest.iter()
                .map(|m| bytes(serde_json::to_value(m).unwrap()))
                .collect(),
        ));
        let message = match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => Message::assistant(None, vec![call("a", "port"), call("b", "user")], None),
            2 => Message::assistant(Some("One more.".into()), vec![call("c", "host")], None),
            _ => Message::assistant(Some("Done.".into()), vec![], None),
        };
        Ok(CompletionResponse {
            message,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 5,
                ..Usage::default()
            },
        })
    }
}

/// Read-only, so a batch of them runs in parallel.
struct Look(&'static str);

#[async_trait::async_trait]
impl Tool for Look {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.0.into(),
            description: format!("{} something up", self.0),
            parameters: json!({"type": "object", "properties": {"what": {"type": "string"}}}),
        }
    }
    async fn call(&self, args: Value, _: &ToolContext) -> Result<ToolOutput, CoreError> {
        Ok(ToolOutput::ok(format!("nothing about {}", args["what"])))
    }
    fn changes_files(&self) -> bool {
        false
    }
}

struct Memory;

#[async_trait::async_trait]
impl SessionRecall for Memory {
    async fn recall(&self, _: &str) -> Option<String> {
        Some("[Long-term memory]\n- #4 The staging port is 5782".into())
    }
}

/// A `UserPromptSubmit` hook that adds a note to every turn.
struct Note;

#[async_trait::async_trait]
impl HookHandler for Note {
    fn command(&self) -> String {
        "note".into()
    }
    async fn run(&self, _: &HookInput, _: &ToolContext) -> HookRun {
        HookRun {
            exit_code: Some(0),
            stdout: r#"{"additionalContext":"the build is at /out"}"#.into(),
            ..Default::default()
        }
    }
}

#[tokio::test]
async fn each_request_starts_with_the_bytes_of_the_one_before() {
    let model = Arc::new(Model::default());
    let mut tools = ToolRegistry::new();
    for name in ["look", "find", "grep", "read"] {
        tools.register(Arc::new(Look(name)));
    }
    let mut hooks = HookSet::new();
    hooks.add(Hook::new(
        HookEvent::UserPromptSubmit,
        Matcher::parse(None),
        HookSource::User,
        Arc::new(Note),
    ));
    let mut agent = Agent::new(
        model.clone(),
        tools,
        HarnessProfile::generic(),
        AgentConfig::default(),
        ToolContext::default(),
        None,
    )
    .with_system_prompt("You are a test agent.")
    .with_session_recall(Arc::new(Memory))
    .with_hooks(hooks);

    for goal in ["Which port?", "And the host?"] {
        let (tx, _rx) = mpsc::channel(4096);
        agent.run(goal, tx).await.unwrap();
    }

    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 4);
    // The system prompt is the configured one, untouched by recall.
    assert!(
        seen[0].0.contains(r#""You are a test agent.""#),
        "{}",
        seen[0].0
    );
    for (i, pair) in seen.windows(2).enumerate() {
        let ((sys_a, tools_a, msgs_a), (sys_b, tools_b, msgs_b)) = (&pair[0], &pair[1]);
        assert_eq!(sys_a, sys_b, "request {} changed the system prompt", i + 1);
        assert_eq!(tools_a, tools_b, "request {} changed the tools", i + 1);
        assert!(msgs_b.len() > msgs_a.len());
        assert_eq!(
            msgs_a[..],
            msgs_b[..msgs_a.len()],
            "request {} rewrote the history before it",
            i + 1
        );
    }
    // The memory and the hook note come after the goal, not before it.
    let first = &seen[0].2;
    assert!(first[0].contains("Which port?"), "{first:?}");
    assert!(first[1].contains("[Long-term memory]"), "{first:?}");
    assert!(first[2].contains("the build is at /out"), "{first:?}");
    // Memory is recalled once a session; the second turn's note is new.
    let last = &seen[3].2;
    assert_eq!(
        last.iter()
            .filter(|m| m.contains("[Long-term memory]"))
            .count(),
        1
    );
    assert_eq!(
        last.iter()
            .filter(|m| m.contains("the build is at /out"))
            .count(),
        2
    );
}
