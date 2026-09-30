//! M47: `GET /api/setup`, the first-run checklist on Home. Three steps, each
//! decided from state the page can see: a default model that can be called, a
//! Telegram channel that is on and healthy, and a first answer from the bot.
//! Nothing here calls a model.

use super::api::{config, ok, Answer};
use super::Ctx;
use crate::channels;
use serde_json::{json, Value};

pub fn view(ctx: &Ctx) -> Answer {
    let model = model_step(ctx);
    let telegram = telegram_step(ctx);
    let hello = hello_step(ctx, model["done"] == true);
    let steps = vec![model, telegram, hello];
    // Telegram is optional: the checklist is finished without it.
    let done = steps
        .iter()
        .all(|s| s["done"] == true || s["skippable"] == true);
    ok(json!({ "done": done, "steps": steps }))
}

fn model_step(ctx: &Ctx) -> Value {
    let label = "Give your bot a brain";
    let default = ctx
        .models
        .as_ref()
        .and_then(|m| m.view().models.into_iter().find(|r| r.default));
    match default {
        Some(d) if d.key_present && d.down_secs.is_none() => json!({
            "id": "model", "done": true, "label": label,
            "detail": format!("{} is the default.", d.reference),
            "model": d.reference,
        }),
        Some(d) if d.key_present => json!({
            "id": "model", "done": false, "label": label,
            "detail": format!("{} is failing right now. Pick another model.", d.reference),
        }),
        Some(d) => json!({
            "id": "model", "done": false, "label": label,
            "detail": format!("{} can't be called yet: {}.", d.reference, d.missing),
        }),
        None => json!({
            "id": "model", "done": false, "label": label,
            "detail": "Add a provider key, or sign in with a ChatGPT plan, so the bot can think.",
        }),
    }
}

fn telegram_step(ctx: &Ctx) -> Value {
    let label = "Connect Telegram";
    let on = config(ctx).is_some_and(|c| channels::configured(&c, "telegram"));
    let running = ctx
        .live
        .as_ref()
        .and_then(|l| l.channels.iter().find(|c| c.name() == "telegram"));
    let problem = running.and_then(|c| c.problem());
    let (done, detail) = match (on, problem) {
        (true, None) => (true, "Telegram is on.".to_string()),
        (true, Some(p)) => (false, ctx.redactor.redact(&p)),
        (false, _) => (
            false,
            "Talk to your bot from your phone. You can skip this and chat here.".to_string(),
        ),
    };
    json!({
        "id": "telegram", "done": done, "label": label, "detail": detail,
        "skippable": true,
    })
}

/// Done once the bot has answered anyone: a page chat reply or a ledger
/// record (every turn on any channel writes one).
fn hello_step(ctx: &Ctx, model_done: bool) -> Value {
    let answered_here = ctx.chat.as_ref().is_some_and(|c| {
        c.since(0)["entries"]
            .as_array()
            .is_some_and(|e| e.iter().any(|e| e["who"] == "agent"))
    });
    let in_ledger = ctx
        .data
        .as_ref()
        .map(|d| d.join("ledger.jsonl"))
        .or_else(|| crate::ledger::ledger_path().ok())
        .and_then(|p| std::fs::metadata(p).ok())
        .is_some_and(|m| m.len() > 0);
    let done = answered_here || (in_ledger && model_done);
    json!({
        "id": "hello", "done": done, "label": "Say hello",
        "detail": if done {
            "Your bot has answered."
        } else {
            "Send your bot a first message and watch it reply."
        },
    })
}
