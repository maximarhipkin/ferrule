//! The agent tools the model calls. One type, one variant per tool; each
//! call is made on behalf of the agent the tool was registered for.
//!
//! M58.3: five tools, after Claude Code's `Agent`, `SendMessage` and
//! `TaskStop`: `spawn_agent`, `message_agent` (wait for reports, list, or
//! tell one agent something), `stop_agent`, `board` and `tasks`. The
//! eleven names before them stay callable for one release, so a transcript
//! or a session that still uses one works. They are never offered
//! ([`Tool::replaced_by`]), and a call to one logs a deprecation warning.

use crate::error::AgentsError;
use crate::prompts;
use crate::store::{AgentRow, Status};
use crate::supervisor::{Limits, Role, SpawnRequest, Supervisor};
use async_trait::async_trait;
use ferrule_core::tool::ToolDefinition;
use ferrule_core::{CoreError, Tool, ToolContext, ToolOutput};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Spawn,
    Message,
    Stop,
    Board,
    Tasks,
    // The names before M58.3, kept for one release.
    Wait,
    Resume,
    Close,
    List,
    Post,
    Read,
    TaskAdd,
    TaskList,
    TaskClaim,
    TaskDone,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Spawn => "spawn_agent",
            Kind::Message => "message_agent",
            Kind::Stop => "stop_agent",
            Kind::Board => "board",
            Kind::Tasks => "tasks",
            Kind::Wait => "wait_agent",
            Kind::Resume => "resume_agent",
            Kind::Close => "close_agent",
            Kind::List => "list_agents",
            Kind::Post => "board_post",
            Kind::Read => "board_read",
            Kind::TaskAdd => "task_add",
            Kind::TaskList => "task_list",
            Kind::TaskClaim => "task_claim",
            Kind::TaskDone => "task_done",
        }
    }

    /// For a name kept from before M58.3, the tool that replaced it.
    fn replaced_by(self) -> Option<&'static str> {
        match self {
            Kind::Wait | Kind::Resume | Kind::List => Some("message_agent"),
            Kind::Close => Some("stop_agent"),
            Kind::Post | Kind::Read => Some("board"),
            Kind::TaskAdd | Kind::TaskList | Kind::TaskClaim | Kind::TaskDone => Some("tasks"),
            Kind::Spawn | Kind::Message | Kind::Stop | Kind::Board | Kind::Tasks => None,
        }
    }
}

pub struct AgentTool {
    sup: Arc<Supervisor>,
    caller: String,
    kind: Kind,
}

/// The tools `row`'s agent gets: the board and the task list always; the
/// tools that start and manage agents only below the deepest level, so a
/// model never sees a tool it can't use.
pub fn for_agent(sup: Arc<Supervisor>, row: &AgentRow, limits: &Limits) -> Vec<Arc<dyn Tool>> {
    let mut kinds = Vec::new();
    if row.depth < limits.max_depth {
        kinds.extend([Kind::Spawn, Kind::Message, Kind::Stop]);
        kinds.extend([Kind::Wait, Kind::Resume, Kind::Close, Kind::List]);
    }
    kinds.extend([Kind::Board, Kind::Tasks]);
    kinds.extend([
        Kind::Post,
        Kind::Read,
        Kind::TaskAdd,
        Kind::TaskList,
        Kind::TaskClaim,
        Kind::TaskDone,
    ]);
    kinds
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

/// An integer, also when the model sends it as a string.
fn as_int(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn int_arg(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(as_int)
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
        if let Some(instead) = self.kind.replaced_by() {
            warn!(
                tool = self.kind.name(),
                instead,
                "a deprecated agent tool name was called (M58.3); it goes in the next release"
            );
        }
        let need =
            |key: &str| str_arg(args, key).ok_or_else(|| self.fail(format!("`{key}` is required")));
        let timeout = args.get("timeout_seconds").and_then(Value::as_u64);
        match self.kind {
            Kind::Spawn => self.spawn(args),
            Kind::Message => {
                let ids = ids_arg(args);
                match (ids.as_slice(), str_arg(args, "message")) {
                    ([], None) => self.list(),
                    ([id], Some(message)) => self.message(id, message),
                    (_, Some(_)) => {
                        Err(self.fail("a message goes to one agent: put its id alone in `ids`"))
                    }
                    (ids, None) => self.wait(ids, timeout).await,
                }
            }
            Kind::Wait => self.wait(&ids_arg(args), timeout).await,
            Kind::Resume => self.resume(need("id")?, need("message")?),
            Kind::Stop | Kind::Close => self.close(need("id")?).await,
            Kind::List => self.list(),
            Kind::Board => match args.get("body").and_then(Value::as_str) {
                Some(body) => self.post(body, args),
                None => self.read(args),
            },
            Kind::Post => {
                let body = args.get("body").and_then(Value::as_str).unwrap_or("");
                self.post(body, args)
            }
            Kind::Read => self.read(args),
            Kind::Tasks => match need("action")? {
                "add" => self.task_add(args),
                "list" => self.task_list(),
                "claim" => self.task_claim(args),
                "done" => self.task_done(args),
                other => Err(self.fail(format!(
                    "unknown action {other:?}; use add, list, claim or done"
                ))),
            },
            Kind::TaskAdd => self.task_add(args),
            Kind::TaskList => self.task_list(),
            Kind::TaskClaim => self.task_claim(args),
            Kind::TaskDone => self.task_done(args),
        }
    }

    fn spawn(&self, args: &Value) -> Result<String, CoreError> {
        let task = str_arg(args, "task")
            .ok_or_else(|| self.fail("`task` is required"))?
            .to_string();
        let role = match str_arg(args, "role") {
            None => Role::Worker,
            Some(r) => Role::parse(r).ok_or_else(|| {
                self.fail(format!(
                    "unknown role {r:?}; use worker, planner or verifier"
                ))
            })?,
        };
        let name = str_arg(args, "name").map(str::to_string);
        let worktree = args
            .get("worktree")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let fork = args.get("fork").and_then(Value::as_bool).unwrap_or(false);
        let model = str_arg(args, "model").map(str::to_string);
        let effort = match str_arg(args, "effort") {
            None => None,
            Some(e @ ("low" | "medium" | "high")) => Some(e.to_string()),
            Some(e) => {
                return Err(self.fail(format!(
                    "unknown effort {e:?}; use low, medium or high, or leave it out"
                )))
            }
        };
        let s = self
            .sup
            .spawn(
                &self.caller,
                SpawnRequest {
                    task,
                    name,
                    role,
                    worktree,
                    model,
                    effort,
                    fork,
                },
            )
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
             you'll be woken with a notice. Use message_agent with its id to block on it instead."
        } else if s.parent_is_root {
            "It runs in the background. Nothing will wake you when it finishes, so call message_agent \
             with its id for its report before you give your final answer."
        } else {
            "It runs in the background. Call message_agent with its id for its report before you give \
             your final answer; notices about it reach you only while you are running."
        });
        Ok(out)
    }

    async fn wait(&self, ids: &[String], timeout: Option<u64>) -> Result<String, CoreError> {
        self.sup
            .wait(&self.caller, ids, timeout)
            .await
            .map_err(|e| self.map_err(e))
    }

    /// `message_agent` with one id and a message: a running child reads it
    /// at its next step (a direct message); any other is resumed with it.
    fn message(&self, id: &str, message: &str) -> Result<String, CoreError> {
        let row = self.sup.store().get(id).map_err(|e| self.map_err(e))?;
        let running_child = row.is_some_and(|r| {
            r.status == Status::Running && r.parent.as_deref() == Some(self.caller.as_str())
        });
        if running_child {
            return self
                .sup
                .post(&self.caller, message, None, Some(id))
                .map_err(|e| self.map_err(e));
        }
        self.resume(id, message)
    }

    fn resume(&self, id: &str, message: &str) -> Result<String, CoreError> {
        self.sup
            .resume(&self.caller, id, message)
            .map_err(|e| self.map_err(e))?;
        Ok(format!(
            "Agent {id} is running again. Call message_agent with its id for its report."
        ))
    }

    async fn close(&self, id: &str) -> Result<String, CoreError> {
        self.sup
            .close(&self.caller, id)
            .await
            .map_err(|e| self.map_err(e))
    }

    fn list(&self) -> Result<String, CoreError> {
        self.sup.list(&self.caller).map_err(|e| self.map_err(e))
    }

    fn post(&self, body: &str, args: &Value) -> Result<String, CoreError> {
        self.sup
            .post(
                &self.caller,
                body,
                str_arg(args, "topic"),
                str_arg(args, "to"),
            )
            .map_err(|e| self.map_err(e))
    }

    fn read(&self, args: &Value) -> Result<String, CoreError> {
        self.sup
            .read(&self.caller, int_arg(args, "since"), str_arg(args, "topic"))
            .map_err(|e| self.map_err(e))
    }

    fn task_add(&self, args: &Value) -> Result<String, CoreError> {
        let title = str_arg(args, "title").ok_or_else(|| self.fail("`title` is required"))?;
        let after: Vec<i64> = match args.get("after") {
            Some(Value::Array(a)) => a.iter().filter_map(as_int).collect(),
            Some(v) => as_int(v).into_iter().collect(),
            None => Vec::new(),
        };
        self.sup
            .task_add(&self.caller, title, str_arg(args, "detail"), &after)
            .map_err(|e| self.map_err(e))
    }

    fn task_list(&self) -> Result<String, CoreError> {
        self.sup
            .task_list(&self.caller)
            .map_err(|e| self.map_err(e))
    }

    fn task_claim(&self, args: &Value) -> Result<String, CoreError> {
        self.sup
            .task_claim(&self.caller, int_arg(args, "id"))
            .map_err(|e| self.map_err(e))
    }

    fn task_done(&self, args: &Value) -> Result<String, CoreError> {
        let id = int_arg(args, "id").ok_or_else(|| self.fail("`id` is required"))?;
        let result = args.get("result").and_then(Value::as_str).unwrap_or("");
        let failed = args.get("failed").and_then(Value::as_bool).unwrap_or(false);
        self.sup
            .task_done(&self.caller, id, result, failed)
            .map_err(|e| self.map_err(e))
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
                        "role": {"type": "string", "enum": ["worker", "planner", "verifier"]},
                        "worktree": {"type": "boolean", "description": "In a git repo: its own worktree and branch (default true). false shares your files."},
                        "fork": {"type": "boolean", "description": "Start it from a copy of the end of your conversation, then the task (default false: the task alone)."},
                        "model": {"type": "string", "description": "Only if the owner asked for it: a connected model (provider/model or an alias) to run it on. Leave it out to use its role's model or yours."},
                        "effort": {"type": "string", "enum": ["low", "medium", "high"], "description": "Reasoning effort for all its turns, where its model takes one: low for lookups and mechanical edits, high for hard design or debugging. Leave it out for the usual."}
                    },
                    "required": ["task"]
                }),
            ),
            Kind::Message => (
                prompts::MESSAGE_DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {
                        "ids": {"type": "array", "items": {"type": "string"}},
                        "message": {"type": "string"},
                        "timeout_seconds": {"type": "integer"}
                    }
                }),
            ),
            Kind::Stop => (
                prompts::STOP_DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {"id": {"type": "string"}},
                    "required": ["id"]
                }),
            ),
            Kind::Board => (
                prompts::BOARD_DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {
                        "body": {"type": "string"},
                        "topic": {"type": "string"},
                        "to": {"type": "string"},
                        "since": {"type": "integer"}
                    }
                }),
            ),
            Kind::Tasks => (
                prompts::TASKS_DESCRIPTION,
                json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["add", "list", "claim", "done"]},
                        "id": {"type": "integer"},
                        "title": {"type": "string"},
                        "detail": {"type": "string"},
                        "after": {"type": "array", "items": {"type": "integer"}},
                        "result": {"type": "string"},
                        "failed": {"type": "boolean"}
                    },
                    "required": ["action"]
                }),
            ),
            // Never offered: only the name and what a call needs (for M25's
            // count of calls that don't fit) matter.
            old => {
                let required: &[&str] = match old {
                    Kind::Wait => &["ids"],
                    Kind::Resume => &["id", "message"],
                    Kind::Close => &["id"],
                    Kind::Post => &["body"],
                    Kind::TaskAdd => &["title"],
                    Kind::TaskDone => &["id", "result"],
                    _ => &[],
                };
                return ToolDefinition {
                    name: old.name().into(),
                    description: format!(
                        "Deprecated: use {}.",
                        old.replaced_by().unwrap_or_default()
                    ),
                    parameters: json!({"type": "object", "required": required}),
                };
            }
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

    fn replaced_by(&self) -> Option<&'static str> {
        self.kind.replaced_by()
    }
}
