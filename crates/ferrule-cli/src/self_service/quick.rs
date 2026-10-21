//! M56.3: the changes the owner asks for most and can take back at once,
//! made without a card: scheduling, rescheduling, pausing or deleting one
//! of the owner's own tasks (one that answers in an owner's chat and runs
//! no gate), and switching the default or this chat's model to one that is
//! already connected. Only in a turn that has read nothing from outside
//! (`Hub::untrusted`, M54.5): an injected page or email can't schedule
//! itself. Each posts a receipt in the chat with an Undo button
//! (`/undo_change <id>`), kept for a day in this process.
//!
//! Every other change keeps its card.

use super::run::{route_of, said};
use super::{Admin, Op};
use crate::dashboard::{api, Ctx};
use ferrule_gateway::{Task, TaskKind, BUILTIN_CHANNEL};
use ferrule_trust::ChatRef;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a receipt's Undo works.
const KEEP_FOR: Duration = Duration::from_secs(24 * 3600);

/// What an Undo does.
#[derive(Clone)]
pub(super) enum Undo {
    /// Runs this op.
    Op(Op),
    /// Deletes the task the change added (its id is known once it ran).
    DeleteAdded,
    /// Adds a deleted task again, paused again if it was.
    Readd(Op, bool),
}

struct Kept {
    id: String,
    undo: Undo,
    at: Instant,
}

static KEPT: Mutex<Vec<Kept>> = Mutex::new(Vec::new());

/// The command a receipt's Undo button sends.
pub const COMMAND: &str = "undo_change";

/// Whether `op` may run without a card here, and what undoes it. `None`:
/// it asks, as before.
pub(super) fn tap_free(
    admin: &Admin,
    ctx: &Ctx,
    here: &ChatRef,
    session: &str,
    op: &Op,
) -> Option<Undo> {
    if admin.hub.untrusted(session).is_some() {
        return None;
    }
    let connected = |model: &str| {
        ctx.models
            .as_ref()
            .and_then(|m| m.resolve(model).ok())
            .is_some_and(|e| e.ready())
    };
    match op {
        Op::TaskAdd { model, .. } => model
            .as_deref()
            .is_none_or(connected)
            .then_some(Undo::DeleteAdded),
        Op::TaskSchedule { id, .. } => {
            let t = own_task(admin, ctx, id)?;
            Some(Undo::Op(Op::TaskSchedule {
                id: t.id,
                schedule: t.schedule,
                timezone: Some(t.timezone),
            }))
        }
        Op::TaskPause { id } => {
            let t = own_task(admin, ctx, id)?;
            t.enabled.then_some(Undo::Op(Op::TaskResume { id: t.id }))
        }
        Op::TaskDelete { id } => {
            let t = own_task(admin, ctx, id)?;
            let paused = !t.enabled;
            Some(Undo::Readd(
                Op::TaskAdd {
                    name: t.name,
                    prompt: t.prompt,
                    schedule: t.schedule,
                    kind: Some(
                        match t.kind {
                            TaskKind::Cron => "cron",
                            TaskKind::Once => "once",
                        }
                        .into(),
                    ),
                    timezone: Some(t.timezone),
                    model: t.model,
                },
                paused,
            ))
        }
        Op::ModelDefault { model } => {
            let now = ctx.models.as_ref()?.view().default?;
            connected(model).then_some(Undo::Op(Op::ModelDefault { model: now }))
        }
        Op::ModelHere { model } => {
            if model.as_deref().is_some_and(|m| !connected(m)) {
                return None;
            }
            // What this chat's pin is now goes back.
            let pinned = ctx
                .models
                .as_ref()?
                .view()
                .pins
                .into_iter()
                .find(|p| p.channel == here.channel && p.chat == here.chat)
                .map(|p| p.reference);
            Some(Undo::Op(Op::ModelHere { model: pinned }))
        }
        _ => None,
    }
}

/// A task the owner may change without a card: one of theirs, answering in
/// an owner's chat (or the dashboard's), with no gate script.
fn own_task(admin: &Admin, ctx: &Ctx, id: &str) -> Option<Task> {
    let t = ctx.tasks.as_ref()?.get(id).ok()??;
    let owners = admin.hub.owners();
    let theirs = owners.contains(&ChatRef::new(&t.channel, &t.chat_id))
        || (t.channel == crate::dashboard::chat::CHANNEL
            && t.chat_id == crate::dashboard::chat::CHAT);
    (theirs && t.channel != BUILTIN_CHANNEL && t.gate.is_none()).then_some(t)
}

/// Runs `op` at once, keeps its Undo, and posts the receipt in `here`.
/// The answer is for the model; it says no card was shown.
pub(super) async fn run(
    admin: &Admin,
    ctx: &Ctx,
    here: &ChatRef,
    op: &Op,
    undo: Undo,
) -> Result<String, String> {
    let by = format!("{} chat {} (no card: M56.3)", here.channel, here.chat);
    let (path, mut body) = route_of(op, here).ok_or("that change has no page action")?;
    body["confirm"] = json!(true);
    let answer = api::act(ctx, path, &body, &by).await;
    let added = answer
        .as_ref()
        .and_then(|(_, v)| v["id"].as_str().map(str::to_string));
    let done = said(answer);
    let undo = match (&done, undo) {
        (Ok(_), Undo::DeleteAdded) => added.map(|id| Undo::Op(Op::TaskDelete { id })),
        (Ok(_), u) => Some(u),
        (Err(_), _) => None,
    };
    let kept = undo.map(keep);
    admin.hub.audit().record(
        chrono::Utc::now(),
        "self_service.ran",
        None,
        None,
        json!({
            "op": op.name(), "digest": op.digest(),
            "ok": done.is_ok(), "by": by, "undo": kept,
        }),
    );
    let said = done.map_err(|why| format!("Not done: {why}"))?;
    let said = ctx.redactor.redact(&said);
    match &kept {
        Some(id) => admin.hub.tell_in_choices(
            here,
            format!("Done without asking: {said}"),
            vec![("Undo".into(), format!("/{COMMAND} {id}"))],
        ),
        None => admin
            .hub
            .tell_in(here, format!("Done without asking: {said}")),
    }
    let receipt = if kept.is_some() {
        "they got a receipt with Undo"
    } else {
        "they got a receipt; it has no Undo"
    };
    Ok(format!(
        "Done, without a card (a change the owner can take back; {receipt}): {said}"
    ))
}

fn keep(undo: Undo) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    kept.retain(|k| k.at.elapsed() < KEEP_FOR);
    kept.push(Kept {
        id: id.clone(),
        undo,
        at: Instant::now(),
    });
    id
}

/// `/undo_change <id>`, the receipt's button, from the owner: puts back
/// what that change replaced, once.
pub(super) async fn undo(admin: &Admin, here: &ChatRef, id: &str) -> String {
    let found = {
        let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
        kept.retain(|k| k.at.elapsed() < KEEP_FOR);
        kept.iter()
            .position(|k| k.id == id)
            .map(|i| kept.remove(i).undo)
    };
    let Some(undo) = found else {
        return "That change can't be undone from here any more (it was undone already, is over a day old, or Ferrule restarted since).".into();
    };
    let Some(ctx) = admin.ctx() else {
        return "Ferrule is still starting; try again in a few seconds.".into();
    };
    let by = format!("{} chat {} (undo {id})", here.channel, here.chat);
    let done = match undo {
        Undo::Op(op) => act(ctx, here, &op, &by).await.map(|(s, _)| s),
        Undo::Readd(add, paused) => match act(ctx, here, &add, &by).await {
            Ok((said, Some(new))) if paused => act(ctx, here, &Op::TaskPause { id: new }, &by)
                .await
                .map(|_| format!("{said} It's paused, as it was.")),
            other => other.map(|(s, _)| s),
        },
        Undo::DeleteAdded => Err("nothing was added".into()),
    };
    admin.hub.audit().record(
        chrono::Utc::now(),
        "self_service.undone",
        None,
        None,
        json!({ "undo": id, "ok": done.is_ok(), "by": by }),
    );
    match done {
        Ok(said) => format!("Undone: {}", ctx.redactor.redact(&said)),
        Err(why) => format!("Not undone: {}", ctx.redactor.redact(&why)),
    }
}

/// One op through the page's handler: what it said, and a new task's id.
async fn act(
    ctx: &Ctx,
    here: &ChatRef,
    op: &Op,
    by: &str,
) -> Result<(String, Option<String>), String> {
    let (path, mut body) = route_of(op, here).ok_or("that change has no page action")?;
    body["confirm"] = json!(true);
    let answer = api::act(ctx, path, &body, by).await;
    let id = answer
        .as_ref()
        .and_then(|(_, v): &(u16, Value)| v["id"].as_str().map(str::to_string));
    said(answer).map(|s| (s, id))
}
