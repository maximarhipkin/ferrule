//! M44 §4: Telegram from the page. A managed bot has no terminal, so the
//! token, the test and the first allowed chat are the dashboard's job, in
//! the same steps `ferrule setup` takes: test the token (dropping a webhook
//! that would starve polling), save it, wait for a message, allow its chat.
//! Channels are built once at start, so every change here needs a restart.

use super::api::{bad, config, missing, need, ok, setup_place, Answer};
use super::channels::audit;
use super::Ctx;
use crate::probe::{self, Check};
use serde_json::{json, Value};

/// The token from the body, once the policy and its shape are checked.
fn token(body: &Value) -> Result<String, Answer> {
    if let Some(why) = crate::managed::channel_refusal("telegram") {
        return Err(bad(403, why));
    }
    let token = body
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if token.is_empty() {
        return Err(bad(400, "paste the bot token first"));
    }
    if !probe::plausible_bot_token(token) {
        return Err(bad(400, "that isn't a bot token (digits:letters)"));
    }
    Ok(token.to_string())
}

fn base_url(ctx: &Ctx) -> String {
    config(ctx)
        .map(|c| c.gateway.telegram_base_url)
        .unwrap_or_else(|| "https://api.telegram.org".to_string())
}

fn running(ctx: &Ctx) -> bool {
    ctx.live
        .as_ref()
        .is_some_and(|l| l.channels.iter().any(|c| c.name() == "telegram"))
}

const RUNNING: &str =
    "Telegram is running, so this page can't read its messages: add chats by id instead";

/// Test: is the token good, whose bot is it. A webhook is deleted: polling
/// gets nothing while one is set.
pub async fn test(ctx: &Ctx, body: &Value) -> Answer {
    let token = need!(token(body));
    let (client, base) = (probe::client(), base_url(ctx));
    let tg = probe::Telegram {
        http: &client,
        base_url: &base,
        token: &token,
    };
    let found = async {
        let name = tg.get_me().await?;
        if tg.webhook().await?.is_some() {
            tg.delete_webhook().await?;
        }
        Ok::<_, Check>(name)
    }
    .await;
    match found {
        Ok(name) => ok(json!({ "ok": true, "said": format!("Works: @{name}"), "name": name })),
        Err(e) => ok(json!({ "ok": false, "said": ctx.redactor.redact(&e.to_string()) })),
    }
}

/// Save: the token to `secrets.env`, its name to `[gateway]`.
pub fn save(ctx: &Ctx, body: &Value) -> Answer {
    let token = need!(token(body));
    let env = config(ctx)
        .and_then(|c| c.gateway.telegram_token_env)
        .unwrap_or_else(|| "TELEGRAM_BOT_TOKEN".to_string());
    let place = need!(setup_place(ctx));
    if let Err(e) = crate::secrets::set(&place.secrets, &env, &token) {
        return bad(500, format!("{e:#}"));
    }
    let saved = || -> anyhow::Result<()> {
        let mut t = crate::setup::Target::load(place.config.clone())?;
        crate::setup::put(
            crate::setup::table(t.root(), &["gateway"])?,
            "telegram_token_env",
            env,
        );
        t.save()
    };
    if let Err(e) = saved() {
        return bad(500, format!("{e:#}"));
    }
    audit(ctx, "channel.saved", "telegram");
    ok(json!({
        "ok": true,
        "said": if ctx.live.is_some() {
            "Telegram saved. Restart the gateway to start it with these settings."
        } else {
            "Telegram saved. It starts with the gateway."
        },
        "restart": ctx.live.is_some(),
    }))
}

/// Wait: the chats that messaged the bot since `offset`.
pub async fn wait(ctx: &Ctx, body: &Value) -> Answer {
    let token = need!(token(body));
    if running(ctx) {
        return bad(409, RUNNING);
    }
    let (client, base) = (probe::client(), base_url(ctx));
    let tg = probe::Telegram {
        http: &client,
        base_url: &base,
        token: &token,
    };
    match tg.updates(body["offset"].as_i64(), 25).await {
        Ok(updates) => {
            let (next, chats) = probe::seen_chats(&updates);
            let chats: Vec<Value> = chats
                .iter()
                .map(|c| json!({ "id": c.id, "kind": c.kind, "name": c.name }))
                .collect();
            ok(json!({ "ok": true, "chats": chats, "next": next }))
        }
        Err(Check::Conflict(_)) => bad(409, RUNNING),
        Err(e) => ok(json!({ "ok": false, "said": ctx.redactor.redact(&e.to_string()) })),
    }
}

/// Allow: the chat into `telegram_allowed_chats`, and a hello to it.
pub async fn allow(ctx: &Ctx, body: &Value) -> Answer {
    let token = need!(token(body));
    let Some(id) = body["chat"].as_i64() else {
        return bad(400, "which chat?");
    };
    let place = need!(setup_place(ctx));
    let (client, base) = (probe::client(), base_url(ctx));
    let tg = probe::Telegram {
        http: &client,
        base_url: &base,
        token: &token,
    };
    // The batch that showed this chat is confirmed, so the gateway won't
    // see those messages again.
    if let Some(next) = body["next"].as_i64() {
        if let Err(e) = tg.updates(Some(next), 0).await {
            return ok(json!({ "ok": false, "said": ctx.redactor.redact(&e.to_string()) }));
        }
    }
    let mut ids = match config(ctx) {
        Some(c) => c.gateway.telegram_allowed_chats,
        None => return missing("the config file"),
    };
    if !ids.contains(&id) {
        ids.push(id);
        let saved = || -> anyhow::Result<()> {
            let mut t = crate::setup::Target::load(place.config.clone())?;
            crate::setup::save_allowed(&mut t, &ids)
        };
        if let Err(e) = saved() {
            return bad(500, format!("{e:#}"));
        }
        audit(ctx, "channel.chat_allowed", "telegram");
    }
    let note = tg
        .send(
            id,
            "✅ Connected: this chat can talk to your ferrule agent.",
        )
        .await
        .err()
        .map(|e| {
            format!(
                " (The hello wasn't delivered: {}.)",
                ctx.redactor.redact(&e.to_string())
            )
        })
        .unwrap_or_default();
    ok(json!({
        "ok": true,
        "said": format!("Chat {id} is allowed. Restart the bot to start Telegram.{note}"),
        "restart": ctx.live.is_some(),
    }))
}
