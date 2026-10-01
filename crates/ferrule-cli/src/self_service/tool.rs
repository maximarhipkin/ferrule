//! The `ferrule_admin` agent tool: typed ops, run in the host process
//! (outside the shell sandbox), each change only after the owner's tap.

use super::describe::{describe, resolve};
use super::run::run;
use super::{Admin, Op, CHANGE_OPS, READ_OPS};
use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_trust::{ChatRef, Question};
use serde_json::{json, Value};
use std::sync::Arc;

pub struct AdminTool {
    admin: Arc<Admin>,
    session: String,
    here: ChatRef,
}

impl AdminTool {
    pub fn new(admin: Arc<Admin>, session: impl Into<String>, here: ChatRef) -> Self {
        Self {
            admin,
            session: session.into(),
            here,
        }
    }
}

fn failed(message: impl Into<String>) -> CoreError {
    CoreError::ToolFailed {
        tool: "ferrule_admin".into(),
        message: message.into(),
    }
}

#[async_trait::async_trait]
impl Tool for AdminTool {
    fn definition(&self) -> ToolDefinition {
        let ops: Vec<&str> = READ_OPS.iter().chain(CHANGE_OPS).copied().collect();
        ToolDefinition {
            name: "ferrule_admin".into(),
            description: ferrule_agents::prompts::ADMIN_DESCRIPTION.into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ops },
                    "model": { "type": "string" },
                    "models": { "type": "array", "items": { "type": "string" } },
                    "key": { "type": "string" },
                    "value": {},
                    "caps": { "type": "object", "additionalProperties": { "type": "number" } },
                    "name": { "type": "string" },
                    "sha": { "type": "string" },
                    "id": { "type": "string" },
                    "prompt": { "type": "string" },
                    "schedule": { "type": "string" },
                    "kind": { "type": "string" },
                    "timezone": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["op"],
                "additionalProperties": false
            }),
        }
    }

    fn changes_files(&self) -> bool {
        false
    }

    /// Turns that change the same thing never overlap.
    fn serial_group(&self) -> Option<String> {
        Some("ferrule_admin".into())
    }

    /// It asks for itself, with the exact change on the card.
    fn needs_approval(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let op = Op::from_args(&args).map_err(failed)?;
        let Some(ctx) = self.admin.ctx() else {
            return Err(failed(
                "Ferrule is still starting; ask again in a few seconds.",
            ));
        };
        let here = &self.here;
        if !op.is_change() {
            return run(&op, ctx, "chat", here)
                .await
                .map(ToolOutput::ok)
                .map_err(failed);
        }
        let hub = &self.admin.hub;
        if crate::trust::is_planning(&self.session) {
            return Err(failed(
                "Not asked: plan mode is on; changes wait until the plan is approved.",
            ));
        }
        if hub.approvals().pending_in(here.clone()) > 0 {
            return Err(failed(
                "Not asked: A question is already waiting in this chat; answer it first.",
            ));
        }
        let op = resolve(op, ctx)
            .await
            .map_err(|e| failed(format!("Not asked: {e}")))?;
        let card = describe(&op, ctx, here)
            .await
            .map_err(|e| failed(format!("Not asked: {e}")))?;
        let bound = format!("{}:{}", op.name(), op.digest());
        let asked = hub
            .ask_in(
                "ferrule_admin",
                Some(here.clone()),
                Question {
                    subject: "ferrule_admin",
                    what: &card,
                    text: &card,
                    timeout: self.admin.ask_for,
                    op: Some(&bound),
                },
            )
            .await;
        let code = match asked {
            Ok(code) => code,
            Err(e) if e.starts_with("the owner refused it") => {
                let said = e
                    .split_once('"')
                    .map(|(_, r)| r.trim_end_matches([')', '"']).to_string())
                    .unwrap_or_default();
                return Ok(ToolOutput::ok(format!(
                    "The owner refused it (they said “{said}”); nothing changed."
                )));
            }
            Err(e) if e.starts_with("no answer in") => {
                return Ok(ToolOutput::ok(format!(
                    "{}, so it was refused; nothing changed.",
                    capital(&e)
                )));
            }
            Err(e) => {
                return Ok(ToolOutput::ok(format!(
                    "Couldn't ask the owner ({e}); nothing changed."
                )));
            }
        };
        let by = format!("{} chat {} (approved {code})", here.channel, here.chat);
        let done = run(&op, ctx, &by, here).await;
        hub.audit().record(
            chrono::Utc::now(),
            "self_service.ran",
            None,
            None,
            json!({
                "op": op.name(), "digest": op.digest(),
                "ok": done.is_ok(), "by": by,
            }),
        );
        Ok(ToolOutput::ok(match done {
            Ok(said) => format!("Approved and done: {said}"),
            Err(why) => format!("Approved, but not done: {why}. Nothing changed."),
        }))
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
