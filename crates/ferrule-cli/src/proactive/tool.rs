//! M64: the `commitments` tool, in the owner's own sessions while
//! proactive is on (docs/m64-proactive.md §1). The pass notes most
//! commitments from the owner's messages by itself; this lets the chat
//! model list them, track one the owner asks to, and close or snooze one.

use super::store::{self, NewCommitment, Store};
use super::{audit, door::line, offer_reminder, parse_due, settings, Offered};
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_trust::Hub;
use serde_json::{json, Value};
use std::sync::Arc;

pub const NAME: &str = "commitments";

pub struct CommitmentsTool {
    hub: Arc<Hub>,
    session: String,
}

impl CommitmentsTool {
    pub fn new(hub: Arc<Hub>, session: &str) -> Self {
        Self {
            hub,
            session: session.to_string(),
        }
    }
}

fn failed(message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: NAME.into(),
        message: message.into(),
    }
}

#[async_trait::async_trait]
impl Tool for CommitmentsTool {
    fn changes_files(&self) -> bool {
        false
    }

    fn deferrable(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: NAME.into(),
            description: "The owner's commitments and unfinished things, tracked so they can be followed up: `list` them, `add` one the owner asks you to track (with `due` when they said when), mark one `done`, `snooze` it for some days, or `drop` it. Things the owner says they'll do are also noted on their own between chats. For an explicit \"remind me at <time>\", schedule a task instead.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "add", "done", "snooze", "drop"] },
                    "text": { "type": "string", "description": "add: what the owner will do, short, in their words" },
                    "due": { "type": "string", "description": "add: 2026-10-24 (a day) or 2026-10-24T17:00 (the owner's clock)" },
                    "unfinished": { "type": "boolean", "description": "add: something started and not finished, rather than a promise" },
                    "id": { "type": "integer", "description": "done, snooze, drop: the commitment's number" },
                    "days": { "type": "integer", "description": "snooze: for how many days (default 1)" }
                },
                "required": ["action"]
            }),
        }
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let (cfg, tz) = settings().map_err(|e| failed(format!("{e:#}")))?;
        let store = Store::open_default().map_err(|e| failed(format!("{e:#}")))?;
        let id = || {
            args["id"]
                .as_i64()
                .ok_or_else(|| failed("which one? give its `id` (`list` numbers them)"))
        };
        let set = |id: i64, status: &str, until: Option<i64>, said: String| match store
            .set_status(id, status, until)
        {
            Ok(true) => {
                audit(
                    &self.hub,
                    "commitment",
                    json!({"id": id, "status": status, "by": "chat"}),
                );
                Ok(ToolOutput::ok(said))
            }
            Ok(false) => Err(failed(format!("there's no commitment #{id}"))),
            Err(e) => Err(failed(format!("{e:#}"))),
        };
        match args["action"].as_str().unwrap_or("") {
            "list" => {
                if let Err(e) = super::loops::sync(&self.hub, &store) {
                    tracing::warn!("proactive: the scheduler's loops weren't read: {e:#}");
                }
                let list = store
                    .commitments(false)
                    .map_err(|e| failed(format!("{e:#}")))?;
                if list.is_empty() {
                    return Ok(ToolOutput::ok("Nothing is tracked."));
                }
                let lines: Vec<String> = list.iter().map(|c| line(c, tz)).collect();
                Ok(ToolOutput::ok(lines.join("\n")))
            }
            "add" => {
                let text = args["text"].as_str().unwrap_or("").trim();
                if text.is_empty() {
                    return Err(failed("`text` is empty"));
                }
                let (due, due_day) = match args["due"].as_str().filter(|d| !d.trim().is_empty()) {
                    Some(d) => {
                        let (t, day) = parse_due(d, tz).map_err(failed)?;
                        (Some(t), day)
                    }
                    None => (None, false),
                };
                let unfinished = args["unfinished"].as_bool().unwrap_or(false);
                let c = store
                    .add_commitment(&NewCommitment {
                        kind: if unfinished { "loop" } else { "commitment" }.into(),
                        text: ferrule_gateway::health::clip(text, 300),
                        due,
                        due_day,
                        source: format!("chat:{}", self.session),
                    })
                    .map_err(|e| failed(format!("{e:#}")))?;
                audit(
                    &self.hub,
                    "noticed",
                    json!({"commitment": c.id, "kind": c.kind, "by": "chat"}),
                );
                let after = match offer_reminder(
                    &self.hub,
                    crate::self_service::offered(&self.hub, &self.session).as_ref(),
                    &store,
                    cfg.proactive.level,
                    tz,
                    &c,
                )
                .await {
                    Offered::Not(why) => format!("No reminder offered: {why}."),
                    Offered::Asked(q) => format!(
                        "The owner was sent a separate question with buttons (\"{q}\"); don't ask again."
                    ),
                    Offered::Done(said) => format!(
                        "Done: {said} (the owner got a receipt with Undo)."
                    ),
                    Offered::Failed(why) => format!("A reminder couldn't be offered: {why}."),
                };
                Ok(ToolOutput::ok(format!("Tracked as #{}. {after}", c.id)))
            }
            "done" => {
                let id = id()?;
                set(id, "done", None, format!("#{id} is done."))
            }
            "drop" => {
                let id = id()?;
                set(id, "dropped", None, format!("#{id} is dropped."))
            }
            "snooze" => {
                let id = id()?;
                let days = args["days"]
                    .as_i64()
                    .filter(|d| (1..=365).contains(d))
                    .unwrap_or(1);
                let until = store::now() + days * 86_400;
                set(
                    id,
                    "snoozed",
                    Some(until),
                    format!("#{id} is snoozed until {}.", super::when(until, tz)),
                )
            }
            other => Err(failed(format!(
                "`{other}` isn't an action: list, add, done, snooze or drop"
            ))),
        }
    }
}
