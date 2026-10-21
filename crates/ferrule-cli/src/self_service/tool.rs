//! The `ferrule_admin` agent tool: typed ops, run in the host process
//! (outside the shell sandbox), each change only after the owner's tap,
//! or at once in owner mode (M54 §5).

use super::describe::{describe, resolve};
use super::run::run;
use super::{keys, locks, promise, restart, update, Admin, Op, CHANGE_OPS, READ_OPS};
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
            description: if self.admin.autonomous() {
                ferrule_agents::prompts::ADMIN_DESCRIPTION_AUTONOMOUS
            } else {
                ferrule_agents::prompts::ADMIN_DESCRIPTION
            }
            .into(),
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
                    "limit": { "type": "integer" },
                    "packages": { "type": "array", "items": { "type": "string" } },
                    "command": { "type": "array", "items": { "type": "string" } },
                    "url": { "type": "string" },
                    "env": { "type": "array", "items": { "type": "string" } },
                    "headers": { "type": "array", "items": { "type": "string" } },
                    "enabled_tools": { "type": "array", "items": { "type": "string" } },
                    "replace": { "type": "boolean" },
                    "hosts": { "type": "array", "items": { "type": "string" } },
                    "why": { "type": "string" }
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

    /// It asks for itself, with the exact change on the card; in owner
    /// mode a change runs without one (M54 §5).
    fn needs_approval(&self) -> bool {
        false
    }

    async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let op = Op::from_args(&args).map_err(failed)?;
        if !op.is_change() {
            let Some(ctx) = self.admin.ctx() else {
                return Err(failed(STARTING));
            };
            return read_op(&self.admin, ctx, &op, &self.here, &self.session)
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
async fn read_op(
    admin: &Admin,
    ctx: &Ctx,
    op: &Op,
    here: &ChatRef,
    session: &str,
) -> Result<String, String> {
    if let Op::DashboardLink = op {
        return dashboard_link(admin, here)
            .await
            .map_err(|e| ctx.redactor.redact(&e));
    }
    if let Op::SecretRequest { name, hosts, why } = op {
        return keys::request(admin, ctx, here, session, name, hosts, why)
            .await
            .map_err(|e| ctx.redactor.redact(&e));
    }
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

/// `dashboard_link`: a one-time login link, sent by the gateway straight to
/// the owner's private chat (`door::send_link`). It is never in the answer,
/// so it never reaches the model or the session's transcript. Each link
/// minted this way is audited, so the owner sees that the bot asked.
async fn dashboard_link(admin: &Admin, here: &ChatRef) -> Result<String, String> {
    let Some(dash) = admin.page() else {
        return Err("The dashboard isn't on in this process.".into());
    };
    crate::dashboard::door::send_link(dash, &admin.hub, here, "")
        .await
        .map_err(|e| format!("No link was sent: {e}"))?;
    admin.hub.audit().record(
        chrono::Utc::now(),
        "self_service.link",
        None,
        None,
        json!({
            "op": "dashboard_link",
            "by": format!("{} chat {}", here.channel, here.chat),
        }),
    );
    // The send itself is the channel's, after this answer.
    Ok(format!(
        "Sending the owner a one-time dashboard link (valid {} min) in their private chat.",
        dash.settings().link_minutes.max(1)
    ))
}

/// Shows the owner what `op` changes, waits for their tap, then runs it.
/// `Err` is "not asked" (nothing was shown or run); `Ok` is the outcome,
/// including a refusal or a timeout. Shared by the tool and the chat's
/// `/update`, `/restart` and `/doctor`. In owner mode there is no card:
/// the same checks run, then the op, and the answer starts "Done:" or
/// "Not done:" (M54 §5).
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
    let auto = admin.autonomous();
    // What a check that stops the op says first: nothing was asked in the
    // default mode, nothing was done in owner mode.
    let lead = if auto { "Not done" } else { "Not asked" };
    if crate::trust::is_planning(session) {
        return Err(format!(
            "{lead}: plan mode is on; changes wait until the plan is approved."
        ));
    }
    if !auto && hub.approvals().pending_in(here.clone()) > 0 {
        return Err(
            "Not asked: A question is already waiting in this chat; answer it first.".into(),
        );
    }
    let op = resolve(op, ctx).await.map_err(|e| format!("{lead}: {e}"))?;
    if let Some(why) = locks::refusal(&op, ctx) {
        return Err(format!("{lead}: {why}"));
    }
    let card = match &op {
        Op::Update => {
            let (apply, units) = admin.apply(ctx).map_err(|e| format!("{lead}: {e}"))?;
            update::card(&apply, units)
                .await
                .map_err(|e| format!("{lead}: {e}"))?
        }
        Op::Restart => restart::card().map_err(|e| format!("{lead}: {e}"))?,
        // Owner mode keeps the card's checks and drops only its text.
        _ => describe(&op, ctx, here)
            .await
            .map_err(|e| format!("{lead}: {e}"))?,
    };
    // M56.3: a change the owner can take back at once, in a turn that read
    // nothing from outside, runs without the card and leaves a receipt.
    if !auto {
        if let Some(undo) = super::quick::tap_free(admin, ctx, here, session, &op) {
            return super::quick::run(admin, ctx, here, &op, undo)
                .await
                .or_else(|why| Ok(format!("{why}{}", stop(&why))));
        }
    }
    let by = if auto {
        format!("{} chat {} (owner mode)", here.channel, here.chat)
    } else {
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
        format!("{} chat {} (approved {code})", here.channel, here.chat)
    };
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
    // apt may have done part of the work before it failed: its own last
    // lines say what, so the answer doesn't claim nothing changed.
    let nothing = match op {
        Op::PackageInstall { .. } | Op::PackageRemove { .. } => "",
        _ => " Nothing changed.",
    };
    Ok(match (done, auto) {
        (Ok(said), true) => format!("Done: {said}"),
        (Err(why), true) => format!("Not done: {why}{}{nothing}", stop(&why)),
        (Ok(said), false) => format!("Approved and done: {said}"),
        (Err(why), false) => format!("Approved, but not done: {why}{}{nothing}", stop(&why)),
    })
}

/// The full stop `text` still needs.
fn stop(text: &str) -> &'static str {
    if text.ends_with(['.', '!', '?']) {
        ""
    } else {
        "."
    }
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
