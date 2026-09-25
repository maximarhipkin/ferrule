//! Shared by the integration tests: a scripted provider whose "model" is a
//! closure over the conversation, and a supervisor rig around it.

#![allow(dead_code)]

use async_trait::async_trait;
use ferrule_agents::{
    AgentStore, AgentsError, ChildFactory, ChildSpec, Limits, Role, SpawnRequest, Status,
    Supervisor,
};
use ferrule_core::{
    Agent, AgentConfig, CompletionRequest, CompletionResponse, CoreError, HarnessProfile, Message,
    Provider, ToolCall, ToolContext, ToolRegistry, Transcript, Usage,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

pub type Brain = Arc<dyn Fn(&[Message]) -> Message + Send + Sync>;

/// A provider that answers with `brain(conversation)`, after taking a permit
/// from `gate` if there is one; each call costs 10 in and 10 out.
pub struct Scripted {
    pub brain: Brain,
    pub gate: Option<Arc<Semaphore>>,
    pub seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

/// Tool names offered on every call of every child, in call order.
pub static TOOLS_OFFERED: Mutex<Vec<(String, Vec<String>)>> = Mutex::new(Vec::new());

#[async_trait]
impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        self.seen.lock().unwrap().push(req.messages.clone());
        let system = req.messages[0].content.clone().unwrap_or_default();
        let names = req.tools.iter().map(|t| t.name.clone()).collect();
        TOOLS_OFFERED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((system, names));
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        Ok(CompletionResponse {
            message: (self.brain)(&req.messages),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 10,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
            },
        })
    }
}

pub fn answer(text: &str) -> Message {
    Message::assistant(Some(text.into()), vec![], None)
}

pub fn call(n: usize, name: &str, args: serde_json::Value) -> Message {
    Message::assistant(
        None,
        vec![ToolCall {
            id: format!("c{n}"),
            name: name.into(),
            arguments: args,
        }],
        None,
    )
}

/// Tool results so far, oldest first.
pub fn tool_results(msgs: &[Message]) -> Vec<&str> {
    msgs.iter()
        .filter(|m| m.role == ferrule_core::Role::Tool)
        .map(|m| m.content.as_deref().unwrap_or(""))
        .collect()
}

/// The id in a spawn_agent result.
pub fn started_id(text: &str) -> String {
    let at = text.find("Started agent ").expect("a spawn result") + "Started agent ".len();
    text[at..at + 10].to_string()
}

pub fn agent(provider: Scripted, transcript: Option<Transcript>, dir: &Path) -> Agent {
    Agent::new(
        Arc::new(provider),
        ToolRegistry::new(),
        HarnessProfile::generic(),
        AgentConfig::default(),
        ToolContext {
            workspace: dir.to_path_buf(),
            ..Default::default()
        },
        transcript,
    )
    .with_system_prompt("You are a test agent.")
}

pub struct Rig {
    pub dir: tempfile::TempDir,
    pub sup: Arc<Supervisor>,
    /// What every child's provider was asked, in call order.
    pub seen: Arc<Mutex<Vec<Vec<Message>>>>,
    /// Every child spec the factory was given.
    pub specs: Arc<Mutex<Vec<ChildSpec>>>,
}

/// A supervisor whose children all think with `brain`, gated by `gate`.
pub fn rig_in(
    dir: tempfile::TempDir,
    limits: Limits,
    brain: Brain,
    gate: Option<Arc<Semaphore>>,
) -> Rig {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let specs = Arc::new(Mutex::new(Vec::new()));
    let factory: ChildFactory = {
        let (seen, specs) = (seen.clone(), specs.clone());
        Arc::new(move |spec: &ChildSpec| {
            specs.lock().unwrap().push(spec.clone());
            let provider = Scripted {
                brain: brain.clone(),
                gate: gate.clone(),
                seen: seen.clone(),
            };
            Ok(agent(
                provider,
                Some(spec.transcript.clone()),
                &spec.workspace,
            ))
        })
    };
    let store = AgentStore::open(dir.path().join("agents.db")).unwrap();
    let sup = Supervisor::new(store, dir.path().join("sessions"), limits, factory).unwrap();
    Rig {
        dir,
        sup,
        seen,
        specs,
    }
}

pub fn rig(limits: Limits, brain: Brain, gate: Option<Arc<Semaphore>>) -> Rig {
    rig_in(tempfile::tempdir().unwrap(), limits, brain, gate)
}

pub fn reporter(report: &'static str) -> Brain {
    Arc::new(move |_| answer(report))
}

/// Attaches a root that never thinks; for driving the supervisor directly.
pub fn idle_root(rig: &Rig, id: &str) {
    idle_root_in(rig, id, rig.dir.path());
}

pub fn idle_root_in(rig: &Rig, id: &str, workspace: &std::path::Path) {
    let provider = Scripted {
        brain: reporter("root"),
        gate: None,
        seen: Arc::default(),
    };
    rig.sup
        .attach_root(agent(provider, None, workspace), id, workspace)
        .unwrap();
}

pub fn spawn(sup: &Supervisor, caller: &str, task: &str) -> Result<String, AgentsError> {
    sup.spawn(
        caller,
        SpawnRequest {
            task: task.into(),
            name: None,
            role: Role::Worker,
            worktree: true,
            model: None,
        },
    )
    .map(|s| s.id)
}

pub async fn until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting until {what}");
}

pub fn status(sup: &Supervisor, id: &str) -> Status {
    sup.store().get(id).unwrap().unwrap().status
}
