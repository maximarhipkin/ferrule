//! The agent tools the model calls. One type, one variant per tool; each
//! call is made on behalf of the agent the tool was registered for.

use crate::error::AgentsError;
use crate::prompts;
use crate::store::AgentRow;
use crate::supervisor::{Limits, Role, SpawnRequest, Supervisor};
use async_trait::async_trait;
use ferrule_core::tool::ToolDefinition;
use ferrule_core::{CoreError, Tool, ToolContext, ToolOutput};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Spawn,
    Wait,
    Resume,
    Close,
    List,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Spawn => "spawn_agent",
            Kind::Wait => "wait_agent",
            Kind::Resume => "resume_agent",
            Kind::Close => "close_agent",
            Kind::List => "list_agents",
        }
    }
}

pub struct AgentTool {
    sup: Arc<Supervisor>,
    caller: String,
    kind: Kind,
}

/// The tools `row`'s agent gets. An agent at the deepest level can't start
/// agents, so it gets none.
pub fn for_agent(sup: Arc<Supervisor>, row: &AgentRow, limits: &Limits) -> Vec<Arc<dyn Tool>> {
    if row.depth >= limits.max_depth {
        return Vec::new();
    }
    [
        Kind::Spawn,
        Kind::Wait,
        Kind::Resume,
        Kind::Close,
        Kind::List,
    ]
    .into_iter()
    .map(|kind| {
        Arc::new(AgentTool {
            sup: sup.clone(),
            caller: row.id.clone(),
            kind,
        }) as Arc<dyn Tool>
    })
    .collect()
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn ids_arg(args: &Value) -> Vec<String> {
    match args.get("ids") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => args
            .get("id")
            .and_then(Value::as_str)
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
    }
}

impl AgentTool {
    fn fail(&self, message: impl Into<String>) -> CoreError {
        CoreError::ToolFailed {
            tool: self.kind.name().into(),
            message: message.into(),
        }
    }

    fn map_err(&self, e: AgentsError) -> CoreError {
        match e {
            // The caller itself was stopped: stop, don't report.
            AgentsError::Core(e @ CoreError::Aborted(_)) => e,
            e => self.fail(e.to_string()),
        }
    }

    async fn run(&self, args: &Value) -> Result<String, CoreError> {
        let need =
            |key: &str| str_arg(args, key).ok_or_else(|| self.fail(format!("`{key}` is required")));
        match self.kind {
            Kind::Spawn => {
                let task = need("task")?.to_string();
                let role = match str_arg(args, "role") {
                    None => Role::Worker,
                    Some(r) => Role::parse(r).ok_or_else(|| {
                        self.fail(format!(
                            "unknown role {r:?}; use worker, planner or verifier"
                        ))
                    })?,
                };
                let name = str_arg(args, "name").map(str::to_string);
                let s = self
                    .sup
                    .spawn(&self.caller, SpawnRequest { task, name, role })
                    .map_err(|e| self.map_err(e))?;
                let mut out = format!(
                    "Started agent {} ({}), working in {}.",
                    s.id,
                    role.as_str(),
                    s.workspace.display()
                );
                for note in &s.notes {
                    out.push('\n');
                    out.push_str(note);
                }
                out.push('\n');
                out.push_str(if s.wakes_parent {
                    "It runs in the background. You can keep working or end your turn; when it finishes \
                     you'll be woken with a notice. Use wait_agent to block on it instead."
                } else if s.parent_is_root {
                    "It runs in the background. Nothing will wake you when it finishes, so call wait_agent \
                     for its report before you give your final answer."
                } else {
                    "It runs in the background. Call wait_agent for its report before you give your final \
                     answer; notices about it reach you only while you are running."
                });
                Ok(out)
            }
            Kind::Wait => {
                let ids = ids_arg(args);
                let timeout = args.get("timeout_seconds").and_then(Value::as_u64);
                self.sup
                    .wait(&self.caller, &ids, timeout)
                    .await
                    .map_err(|e| self.map_err(e))
            }
            Kind::Resume => {
                let id = need("id")?;
                let message = need("message")?;
                self.sup
                    .resume(&self.caller, id, message)
                    .map_err(|e| self.map_err(e))?;
                Ok(format!(
                    "Agent {id} is running again. Call wait_agent for its report."
                ))
            }
            Kind::Close => {
                let id = need("id")?;
                self.sup
                    .close(&self.caller, id)
                    .await
                    .map_err(|e| self.map_err(e))
            }
            Kind::List => self.sup.list(&self.caller).map_err(|e| self.map_err(e)),
        }
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn definition(&self) -> ToolDefinition {
        let (description, parameters) = match self.kind {
            Kind::Spawn => (
                prompts::SPAWN_DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string", "description": "The complete task: goal, context, constraints, what to report."},
                        "name": {"type": "string", "description": "A short label, for you and the owner."},
                        "role": {"type": "string", "enum": ["worker", "planner", "verifier"]}
                    },
                    "required": ["task"]
                }),
            ),
            Kind::Wait => (
                prompts::WAIT_DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {
                        "ids": {"type": "array", "items": {"type": "string"}},
                        "timeout_seconds": {"type": "integer"}
                    },
                    "required": ["ids"]
                }),
            ),
            Kind::Resume => (
                prompts::RESUME_DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "message": {"type": "string", "description": "What it should do next."}
                    },
                    "required": ["id", "message"]
                }),
            ),
            Kind::Close => (
                prompts::CLOSE_DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {"id": {"type": "string"}},
                    "required": ["id"]
                }),
            ),
            Kind::List => (
                prompts::LIST_DESCRIPTION,
                json!({"type": "object", "properties": {}}),
            ),
        };
        ToolDefinition {
            name: self.kind.name().into(),
            description: description.into(),
            parameters,
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        self.run(&args).await.map(ToolOutput::ok)
    }

    fn changes_files(&self) -> bool {
        false
    }
}
