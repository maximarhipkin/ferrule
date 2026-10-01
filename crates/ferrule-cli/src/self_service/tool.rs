//! The `ferrule_admin` agent tool: typed ops, run in the host process
//! (outside the shell sandbox), each change only after the owner's tap.

use super::describe::{describe, resolve};
use super::run::run;
use super::{locks, promise, restart, update, Admin, Op, CHANGE_OPS, READ_OPS};
use crate::dashboard::Ctx;
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
        if !op.is_change() {
            let Some(ctx) = self.admin.ctx() else {
                return Err(failed(STARTING));
            };
            return read_op(&self.admin, ctx, &op, &self.here)
                .await
                .map(ToolOutput::ok)
                .map_err(failed);
        }
        ask_and_run(&self.admin, &self.here, &self.session, op)
            .await
            .map(ToolOutput::ok)
            .map_err(failed)
    }
}

const STARTING: &str = "Ferrule is still starting; ask again in a few seconds.";

/// A read-only op, at once.
async fn read_op(admin: &Admin, ctx: &Ctx, op: &Op, here: &ChatRef) -> Result<String, String> {
    if let Op::UpdateCheck = op {
        if let Some(why) = locks::refusal(op, ctx) {
            return Err(why);
        }
        let (apply, _) = admin.apply(ctx)?;
        let found = update::check(&apply).await?;
        return Ok(update::check_text(&apply.current, found.as_ref()));
    }
    run(op, ctx, "chat", here).await
}

/// Shows the owner what `op` changes, waits for their tap, then runs it.
/// `Err` is "not asked" (nothing was shown or run); `Ok` is the outcome,
/// including a refusal or a timeout. Shared by the tool and the chat's
/// `/update`, `/restart` and `/doctor`.
pub(super) async fn ask_and_run(
    admin: &Admin,
    here: &ChatRef,
    session: &str,
    op: Op,
) -> Result<String, String> {
    let Some(ctx) = admin.ctx() else {
        return Err(STARTING.into());
    };
    let hub = &admin.hub;
    if crate::trust::is_planning(session) {
        return Err("Not asked: plan mode is on; changes wait until the plan is approved.".into());
    }
    if hub.approvals().pending_in(here.clone()) > 0 {
        return Err(
            "Not asked: A question is already waiting in this chat; answer it first.".into(),
        );
    }
    let op = resolve(op, ctx)
        .await
        .map_err(|e| format!("Not asked: {e}"))?;
    if let Some(why) = locks::refusal(&op, ctx) {
        return Err(format!("Not asked: {why}"));
    }
    let card = match &op {
        Op::Update => {
            let (apply, units) = admin.apply(ctx).map_err(|e| format!("Not asked: {e}"))?;
            update::card(&apply, units)
                .await
                .map_err(|e| format!("Not asked: {e}"))?
        }
        Op::Restart => restart::card().map_err(|e| format!("Not asked: {e}"))?,
        _ => describe(&op, ctx, here)
            .await
            .map_err(|e| format!("Not asked: {e}"))?,
    };
    let bound = format!("{}:{}", op.name(), op.digest());
    let asked = hub
        .ask_in(
            "ferrule_admin",
            Some(here.clone()),
            Question {
                subject: "ferrule_admin",
                what: &card,
                text: &card,
                timeout: admin.ask_for,
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
            return Ok(format!(
                "The owner refused it (they said “{said}”); nothing changed."
            ));
        }
        Err(e) if e.starts_with("no answer in") => {
            return Ok(format!(
                "{}, so it was refused; nothing changed.",
                capital(&e)
            ));
        }
        Err(e) => return Ok(format!("Couldn't ask the owner ({e}); nothing changed.")),
    };
    let by = format!("{} chat {} (approved {code})", here.channel, here.chat);
    let done = match &op {
        Op::Update => start_update(admin, ctx, here, session),
        Op::Restart => start_restart(admin, ctx, here, session),
        _ => run(&op, ctx, &by, here).await,
    };
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
    Ok(match done {
        Ok(said) => format!("Approved and done: {said}"),
        Err(why) => format!("Approved, but not done: {why}. Nothing changed."),
    })
}

/// The install runs on its own once this turn is over, so the answer the
/// owner is reading isn't cut off and the install doesn't wait on itself.
fn start_update(admin: &Admin, ctx: &Ctx, here: &ChatRef, session: &str) -> Result<String, String> {
    let (apply, units) = admin.apply(ctx)?;
    let (router, hub) = (admin.router(), admin.hub.clone());
    let (chat, session) = (here.clone(), session.to_string());
    tokio::spawn(async move {
        restart::turn_over(&router, &session).await;
        match update::install(apply, units, &chat).await {
            update::Applied::Restarting { exe, .. } => {
                crate::lifecycle::set_reexec_path(exe);
                let hub = hub.clone();
                let chat = chat.clone();
                restart::after_turn(router, session, move || {
                    if let Err(e) = restart::now() {
                        hub.tell_in(&chat, e);
                    }
                });
            }
            update::Applied::Said(text) => hub.tell_in(&chat, text),
        }
    });
    Ok(
        "Installing the new Ferrule now; I restart when this turn ends and tell the owner here \
        when it's back. Finish your reply now."
            .into(),
    )
}

fn start_restart(
    admin: &Admin,
    ctx: &Ctx,
    here: &ChatRef,
    session: &str,
) -> Result<String, String> {
    let data = ctx.data.as_ref().ok_or("there's no data dir here")?;
    restart::card()?;
    promise::make(data, here, promise::Kind::Restart).map_err(|e| format!("{e:#}"))?;
    let (hub, chat) = (admin.hub.clone(), here.clone());
    let data = data.clone();
    restart::after_turn(admin.router(), session.to_string(), move || {
        if let Err(e) = restart::now() {
            let _ = promise::take(&data);
            hub.tell_in(&chat, e);
        }
    });
    Ok(
        "Restarting when this turn ends; the owner is told here when it's back. Finish your reply \
        now."
            .into(),
    )
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
