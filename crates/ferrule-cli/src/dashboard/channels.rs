//! M39 §9: the Channels section, one card per chat channel. Its state
//! comes from the running gateway, the form from the channel's
//! [`card::Spec`](crate::channels::card::Spec); secrets go in and never
//! come back out: a card only says whether each one is set.

use super::api::{arg, bad, config, confirmed, missing, need, ok, setup_place, Answer};
use super::Ctx;
use crate::channels::card::{self, Kind, Values};
use crate::channels::{self, CHANNELS};
use serde_json::{json, Value};

/// Every channel's card.
pub fn list(ctx: &Ctx) -> Answer {
    let cfg = config(ctx);
    let place = setup_place(ctx).ok();
    let stale = ctx
        .live
        .as_ref()
        .map(|l| l.health.stale_channels(&l.channels))
        .unwrap_or_default();
    let cards: Vec<Value> = CHANNELS
        .iter()
        .map(|c| {
            let on = cfg.as_ref().is_some_and(|cfg| channels::configured(cfg, c.name));
            let running = ctx
                .live
                .as_ref()
                .and_then(|l| l.channels.iter().find(|ch| ch.name() == c.name));
            let (state, why) = match (on, running) {
                (_, Some(ch)) => match ch.problem() {
                    Some(p) => ("problem", Some(ctx.redactor.redact(&p))),
                    None if stale.iter().any(|s| s == c.name) => (
                        "stale",
                        Some("it hasn't heard from the service for a while".to_string()),
                    ),
                    None if !on => (
                        "restart",
                        Some("taken out of the config: a gateway restart stops it".into()),
                    ),
                    None => ("on", None),
                },
                (true, None) if ctx.live.is_some() => (
                    "restart",
                    Some("in the config, not running yet: restart the gateway".into()),
                ),
                (true, None) => ("set", None),
                (false, None) => ("off", None),
            };
            let spec = card::spec(c.name);
            let table = place
                .as_ref()
                .and_then(|p| card::current(&p.config, c.name));
            let fields: Vec<Value> = spec
                .map(|s| s.fields)
                .unwrap_or_default()
                .iter()
                .map(|f| {
                    let now = table.as_ref().and_then(|t| t.get(f.key));
                    let mut v = json!({
                        "name": f.key,
                        "label": f.label,
                        "hint": f.hint,
                        "optional": f.optional,
                    });
                    match f.kind {
                        Kind::Secret { env } => {
                            let env = now.and_then(toml::Value::as_str).unwrap_or(env);
                            v["secret"] = json!(true);
                            v["set"] = json!(place.as_ref().is_some_and(|p| p.get(env).is_some()));
                            v["env"] = json!(env);
                        }
                        Kind::List => {
                            v["kind"] = json!("list");
                            v["value"] = json!(now
                                .and_then(toml::Value::as_array)
                                .map(|a| a
                                    .iter()
                                    .filter_map(toml::Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join("\n"))
                                .unwrap_or_default());
                        }
                        Kind::Number => {
                            v["kind"] = json!("number");
                            v["value"] = json!(now.map(|n| n.to_string()).unwrap_or_default());
                        }
                        Kind::Choice(words) => {
                            v["choices"] = json!(words);
                            v["value"] = json!(now.and_then(toml::Value::as_str).unwrap_or_default());
                        }
                        Kind::Text => {
                            v["value"] = json!(now.and_then(toml::Value::as_str).unwrap_or_default());
                        }
                    }
                    v
                })
                .collect();
            json!({
                "name": c.name,
                "title": c.title,
                "icon": card::icon(c.name),
                "configured": on,
                "state": state,
                "why": why,
                // Telegram, Discord and Slack keep `ferrule setup`'s flow.
                "form": spec.is_some(),
                "fields": fields,
                "guide": spec.map(|s| s.guide).unwrap_or_default().iter().map(|g| json!({"text": g.text, "url": g.url})).collect::<Vec<_>>(),
                "doc": format!("docs/channels.md#{}", c.name),
                // The HTTP API's keys: what they are, never a key.
                "keys": (c.name == "http").then(|| keys_dir(ctx).map(|d| channels::http::keys_json(&d))).flatten(),
            })
        })
        .collect();
    ok(json!({ "channels": cards, "gateway": ctx.live.is_some() }))
}

fn values(body: &Value) -> Values {
    body.get("values")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn spec_of(body: &Value) -> Result<&'static card::Spec, Answer> {
    let name = arg(body, "name")?;
    card::spec(name).ok_or_else(|| {
        bad(
            400,
            format!("{name} has no form here: `ferrule setup` sets it up"),
        )
    })
}

/// Test: the typed values over the saved ones, against the service.
pub async fn test(ctx: &Ctx, body: &Value) -> Answer {
    let spec = need!(spec_of(body));
    let place = need!(setup_place(ctx));
    let settings = match card::settings(spec, &place, &values(body)) {
        Ok(s) => s,
        Err(e) => return bad(400, e),
    };
    let said = match (spec.probe)(settings).await {
        Ok(found) => json!({ "ok": true, "said": format!("Works: {found}") }),
        Err(why) => json!({ "ok": false, "said": ctx.redactor.redact(&why) }),
    };
    ok(said)
}

/// Save: the secrets to `secrets.env`, the rest to `[gateway.<name>]`.
pub fn save(ctx: &Ctx, body: &Value) -> Answer {
    let spec = need!(spec_of(body));
    if let Some(why) = crate::managed::channel_refusal(spec.name) {
        return bad(403, why);
    }
    let place = need!(setup_place(ctx));
    if let Err(e) = card::save(spec, &place, &values(body)) {
        return bad(400, e);
    }
    audit(ctx, "channel.saved", spec.name);
    let title = channels::CHANNELS
        .iter()
        .find(|c| c.name == spec.name)
        .map_or(spec.name, |c| c.title);
    ok(json!({
        "ok": true,
        "said": if ctx.live.is_some() {
            format!("{title} saved. Restart the gateway to start it with these settings.")
        } else {
            format!("{title} saved. It starts with the gateway.")
        },
        "restart": ctx.live.is_some(),
    }))
}

/// Remove: `[gateway.<name>]` out, and the secrets nothing else reads.
pub fn remove(ctx: &Ctx, body: &Value) -> Answer {
    let name = need!(arg(body, "name"));
    let Some(info) = CHANNELS.iter().find(|c| c.name == name) else {
        return bad(400, format!("there's no channel called {name}"));
    };
    if card::spec(name).is_none() {
        return bad(
            400,
            format!("{} is taken out with `ferrule setup`", info.title),
        );
    }
    let Some(cfg) = config(ctx) else {
        return missing("the config file");
    };
    if !channels::configured(&cfg, name) {
        return ok(json!({ "ok": true, "said": format!("{} isn't set up.", info.title) }));
    }
    let place = need!(setup_place(ctx));
    need!(confirmed(
        body,
        format!(
            "Take {} out of the config? Its saved tokens go too, unless something else uses them. \
             Its chats' history stays.",
            info.title
        )
    ));
    match card::remove(&place, name, &cfg) {
        Ok(forgot) => {
            audit(ctx, "channel.removed", name);
            ok(json!({
                "ok": true,
                "said": format!(
                    "{} removed{}.{}",
                    info.title,
                    if forgot.is_empty() { String::new() } else { format!(", and {} forgotten", forgot.join(", ")) },
                    if ctx.live.is_some() { " It stops at the next gateway restart." } else { "" }
                ),
                "restart": ctx.live.is_some(),
            }))
        }
        Err(e) => bad(500, format!("{e:#}")),
    }
}

/// The HTTP API's keys dir for this instance.
fn keys_dir(ctx: &Ctx) -> Option<std::path::PathBuf> {
    ctx.data.as_deref().map(channels::http::dir_in)
}

/// A new key for the HTTP API: shown in this answer, once.
pub fn key_add(ctx: &Ctx, body: &Value) -> Answer {
    let name = need!(arg(body, "name"));
    let Some(dir) = keys_dir(ctx) else {
        return missing("the data dir");
    };
    let webhook = body
        .get("webhook")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|w| !w.is_empty());
    match ferrule_gateway::channels::http::clients::add(&dir, name.trim(), webhook) {
        Ok(made) => {
            audit_key(ctx, "channel.key_added", name);
            ok(json!({
                "ok": true,
                "said": format!("The key for {name}. Copy it now: it isn't shown again."),
                "key": made.key,
                "webhook_secret": made.webhook_secret,
            }))
        }
        Err(e) => bad(400, e),
    }
}

/// Takes a key away; its next request gets 401.
pub fn key_revoke(ctx: &Ctx, body: &Value) -> Answer {
    let name = need!(arg(body, "name"));
    let Some(dir) = keys_dir(ctx) else {
        return missing("the data dir");
    };
    need!(confirmed(
        body,
        format!("Revoke the key {name}? The program using it gets 401 from its next request.")
    ));
    match ferrule_gateway::channels::http::clients::revoke(&dir, name) {
        Ok(true) => {
            audit_key(ctx, "channel.key_revoked", name);
            ok(json!({ "ok": true, "said": format!("{name} is revoked.") }))
        }
        Ok(false) => bad(404, format!("there's no key {name}")),
        Err(e) => bad(500, e),
    }
}

fn audit_key(ctx: &Ctx, event: &str, key: &str) {
    if let Some(hub) = &ctx.hub {
        hub.audit().record(
            chrono::Utc::now(),
            event,
            None,
            None,
            json!({ "channel": "http", "key": key, "by": super::api::BY }),
        );
    }
}

fn audit(ctx: &Ctx, event: &str, channel: &str) {
    if let Some(hub) = &ctx.hub {
        hub.audit().record(
            chrono::Utc::now(),
            event,
            None,
            None,
            json!({ "channel": channel, "by": super::api::BY }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_gateway::Redactor;
    use std::sync::Arc;

    #[test]
    fn every_channel_has_a_card_and_the_old_three_point_to_setup() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("ferrule.toml");
        std::fs::write(
            &config,
            "[gateway]\ntelegram_token_env = \"TELEGRAM_BOT_TOKEN\"\n[gateway.signal]\naccount = \"+15550001\"\n",
        )
        .unwrap();
        let mut ctx = Ctx::bare(Arc::new(Redactor::new(Vec::<String>::new())));
        ctx.config_path = Some(config);
        ctx.data = Some(dir.path().join("data"));
        let (status, v) = list(&ctx).unwrap();
        assert_eq!(status, 200);
        let cards = v["channels"].as_array().unwrap();
        assert_eq!(cards.len(), CHANNELS.len());
        let by = |n: &str| cards.iter().find(|c| c["name"] == n).unwrap().clone();
        assert_eq!(by("telegram")["state"], "set");
        assert_eq!(by("telegram")["form"], false);
        assert_eq!(by("signal")["configured"], true);
        assert_eq!(by("matrix")["state"], "off");
        assert_eq!(by("email")["form"], true);
        for c in cards {
            assert!(!c["icon"].as_str().unwrap().is_empty());
        }
        let (status, _) = remove(&ctx, &json!({ "name": "telegram" })).unwrap();
        assert_eq!(status, 400, "the old three stay with `ferrule setup`");
    }
}
