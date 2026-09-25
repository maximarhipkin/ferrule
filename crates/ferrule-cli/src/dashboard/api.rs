//! The page's JSON, one read per section and the owner's operations
//! (docs/m22-dashboard.md §5). Everything goes through the APIs Telegram
//! and the CLI use (M21's `Models`, M20's `Connections`, the trust hub,
//! [`TasksAdmin`](crate::tasks_admin::TasksAdmin)); nothing here calls a
//! model except the owner's explicit test of one. Every answer is redacted
//! by the caller.

use super::http::Request;
use super::Ctx;
use crate::config::Config;
use crate::model_eval;
use crate::models::catalog;
use crate::models::Retire;
use chrono::{DateTime, Utc};
use ferrule_core::LedgerRecord;
use ferrule_gateway::health::{clip, human, stamp};
use ferrule_gateway::{Channel, Health, RecentLog, Router};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

/// Who the audit log says made a change from the page.
pub const BY: &str = "dashboard";

/// What only the running gateway has.
pub struct Live {
    pub router: Weak<Router>,
    pub health: Arc<Health>,
    pub channels: Vec<Arc<dyn Channel>>,
    /// The gateway's `--provider`: every chat is on it until a restart.
    pub fixed: Option<String>,
    /// Retires one lane, or every chat's, after a model change.
    pub retire: Retire,
}

type Answer = Option<(u16, Value)>;

fn ok(v: Value) -> Answer {
    Some((200, v))
}

fn bad(status: u16, why: impl std::fmt::Display) -> Answer {
    Some((status, json!({ "error": why.to_string() })))
}

fn missing(what: &str) -> Answer {
    bad(503, format!("{what} isn't available in this process"))
}

/// A destructive operation's second step: `409` with the question until
/// the body says `"confirm": true`.
fn confirmed(body: &Value, question: String) -> Result<(), Answer> {
    if body.get("confirm").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(Some((409, json!({ "confirm": question }))))
    }
}

fn arg<'a>(body: &'a Value, key: &str) -> Result<&'a str, Answer> {
    body.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad(400, format!("`{key}` is missing")))
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

macro_rules! need {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(answer) => return answer,
        }
    };
}

pub async fn route(ctx: &Ctx, get: bool, req: &Request, body: &Value) -> Answer {
    let path = req.path.strip_prefix("/api/")?;
    if get {
        return match path {
            "health" => ok(health(ctx)),
            "connections" => connections(ctx),
            "models" => models(ctx),
            "catalog" => catalog_list(ctx, req).await,
            "recommend" => recommend(ctx).await,
            "usage" => usage(ctx, req),
            "tasks" => tasks(ctx),
            "logs" => logs(ctx, req),
            "extensions" => extensions(ctx),
            "agents" => agents(ctx),
            "eval" => ok(ctx.evals.view()),
            _ => None,
        };
    }
    match path {
        "turn/stop" => stop_turn(ctx, body),
        "kill/on" | "kill/off" => kill(ctx, path == "kill/on", body),
        "models/default" | "models/pin" | "models/unpin" | "models/fallback" | "models/add"
        | "models/remove" | "models/test" => model_op(ctx, path, body).await,
        "catalog/add" => catalog_add(ctx, body).await,
        "catalog/fill-prices" => fill_prices(ctx).await,
        "connections/connect" | "connections/disconnect" => connection_op(ctx, path, body).await,
        "tasks/pause" | "tasks/resume" | "tasks/run" | "tasks/delete" => task_op(ctx, path, body),
        "eval/estimate" | "eval/start" => eval_op(ctx, path == "eval/start", body).await,
        "eval/cancel" => eval_cancel(ctx, body),
        _ => None,
    }
}

// ---- Health -------------------------------------------------------------

/// Uptime, the lanes, the watchdog, the kill switch, the heartbeat,
/// today's spend against the caps, and what's wrong (the first thing the
/// page shows).
pub fn health(ctx: &Ctx) -> Value {
    let mut problems: Vec<Value> = Vec::new();
    let mut out = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "gateway": ctx.live.is_some(),
    });
    if let Some(live) = &ctx.live {
        let h = &live.health;
        let lanes = live
            .router
            .upgrade()
            .map(|r| r.snapshot())
            .unwrap_or_default();
        let stuck_after = h.settings().watchdog_after;
        let turns: Vec<Value> = lanes
            .iter()
            .filter(|l| l.busy_for.is_some() || l.queued > 0)
            .map(|l| {
                let quiet = l.since_progress.or(l.busy_for).unwrap_or_default();
                let stuck = l.busy_for.is_some()
                    && quiet >= stuck_after.unwrap_or(ferrule_gateway::health::HEARTBEAT_STUCK);
                if stuck {
                    problems.push(json!({
                        "what": ctx.redactor.redact(&h.stall_notice(l)),
                        "fix": "Stop it from the list of running turns.",
                        "section": "health",
                    }));
                }
                json!({
                    "session": l.session_id,
                    "place": l.place(),
                    "busy_secs": l.busy_for.map(|d| d.as_secs()),
                    "activity": l.activity,
                    "quiet_secs": quiet.as_secs(),
                    "queued": l.queued,
                    "text": clip(&ctx.redactor.redact(&l.text), 80),
                    "stuck": stuck,
                })
            })
            .collect();
        let stale = h.stale_channels(&live.channels);
        for c in &stale {
            problems.push(json!({
                "what": format!("{c} hasn't polled successfully for over {}", human(h.settings().poll_stale)),
                "fix": "Check the network and the bot token; the gateway keeps retrying.",
                "section": "health",
            }));
        }
        let channels: Vec<Value> = live
            .channels
            .iter()
            .map(|c| {
                json!({
                    "name": c.name(),
                    "polls": c.polls(),
                    "last_ok_poll": c.last_ok_poll().map(unix),
                    "stale": stale.iter().any(|s| s == c.name()),
                })
            })
            .collect();
        let watchdog = h.watchdog_ok(&live.channels);
        let heartbeat = h.settings().heartbeat.as_ref().map(|hb| {
            let last = h.last_heartbeat();
            json!({
                // The host only: the path is the check's secret.
                "host": url_host(&hb.url),
                "every_secs": hb.every.as_secs(),
                "last_at": last.as_ref().map(|(t, _)| unix(*t)),
                "last_error": last.and_then(|(_, e)| e),
            })
        });
        out["uptime_secs"] = json!(h.uptime().as_secs());
        out["uptime"] = json!(human(h.uptime()));
        out["started"] = json!(stamp(h.started()));
        out["last_start"] = json!(h.last_start());
        out["turns"] = json!(turns);
        out["channels"] = json!(channels);
        out["watchdog"] = json!({
            "after_secs": stuck_after.map(|d| d.as_secs()),
            "ok": watchdog.is_ok(),
            "why": watchdog.err(),
        });
        out["heartbeat"] = json!(heartbeat);
    }
    if let Some(hub) = &ctx.hub {
        let stop = hub.stopped();
        if let Some(s) = &stop {
            problems.insert(
                0,
                json!({
                    "what": format!("The kill switch is on (by {}, {}): no model is called.", s.by, s.at),
                    "fix": "Turn it off here, or send /resume.",
                    "action": "kill/off",
                    "section": "health",
                }),
            );
        }
        out["kill"] = json!({
            "on": stop.is_some(),
            "by": stop.as_ref().map(|s| s.by.clone()),
            "at": stop.as_ref().map(|s| s.at.clone()),
            "reason": stop.and_then(|s| s.reason),
        });
        out["spend"] = spend(hub, &mut problems);
    }
    if let Some(m) = &ctx.models {
        model_problems(m, ctx.live.as_ref(), &mut problems);
    }
    out["problems"] = json!(problems);
    out
}

/// `https://hc-ping.com/<uuid>` → `hc-ping.com`.
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host.rsplit('@').next().unwrap_or("").to_string()
}

/// Every `http(s)://host/path?query` in `text` as `http(s)://host/…`.
fn url_paths_hidden(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = ["http://", "https://"]
        .iter()
        .filter_map(|s| rest.find(s))
        .min()
    {
        out.push_str(&rest[..at]);
        let end = rest[at..]
            .find(|c: char| {
                c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '<' | '>' | ']' | '`')
            })
            .map_or(rest.len(), |e| at + e);
        let url = &rest[at..end];
        let scheme = &url[..url.find("://").unwrap_or(0) + 3];
        let host = url_host(url);
        out.push_str(scheme);
        out.push_str(&host);
        if url.len() > scheme.len() + host.len() {
            out.push_str("/…");
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn spend(hub: &ferrule_trust::Hub, problems: &mut Vec<Value>) -> Value {
    let c = hub.config();
    let (day, _) = match hub.today(None) {
        Ok(v) => v,
        Err(e) => return json!({ "error": e }),
    };
    let share = |used: f64, cap: f64| (cap > 0.0).then(|| used / cap);
    let caps = [
        ("usd_per_day", day.usd, c.max_usd_per_day),
        (
            "tokens_per_day",
            day.tokens as f64,
            c.max_tokens_per_day as f64,
        ),
    ];
    let mut rows = Vec::new();
    for (name, used, cap) in caps {
        let s = share(used, cap);
        if s.is_some_and(|s| s >= 1.0) {
            problems.push(json!({
                "what": format!("Today's cap {name} is used up: runs stop until tomorrow."),
                "fix": "Raise it in [trust] of the config, or wait for the day to turn.",
                "section": "usage",
            }));
        }
        rows.push(json!({ "cap": name, "used": used, "limit": cap, "share": s }));
    }
    json!({
        "today": { "tokens": day.tokens, "usd": day.usd },
        "caps": rows,
        "per_run": { "usd": c.max_usd_per_run, "tokens": c.max_tokens_per_run },
        "per_task": { "usd": c.max_usd_per_task, "tokens": c.max_tokens_per_task },
        "warn_at": c.warn_at,
        "timezone": c.timezone,
    })
}

/// The default that's down or gone first, with the fix next to it: a
/// working model to switch to.
fn model_problems(m: &crate::models::Models, live: Option<&Live>, problems: &mut Vec<Value>) {
    let view = m.view();
    let default = view.models.iter().find(|r| r.default);
    let healthy = view
        .models
        .iter()
        .find(|r| !r.default && r.key_present && r.down_secs.is_none())
        .map(|r| r.reference.clone());
    let fix = |what: String| {
        let mut p = json!({
            "what": what,
            "fix": "Make another model the default, or pick one from the catalog below.",
            "section": "models",
            "top": true,
        });
        if let Some(h) = &healthy {
            p["suggest"] = json!(h);
        }
        p
    };
    match default {
        Some(d) if d.down_secs.is_some() => problems.insert(
            0,
            fix(format!(
                "The default model {} is failing: {} (skipped for {}).",
                d.reference,
                d.down_reason.as_deref().unwrap_or("no answer"),
                human(Duration::from_secs(d.down_secs.unwrap_or(0)))
            )),
        ),
        Some(d) if !d.key_present => problems.insert(
            0,
            fix(format!(
                "The default model {} has no key ({} isn't set).",
                d.reference, d.key_env
            )),
        ),
        _ => {}
    }
    for p in &view.problems {
        problems.push(json!({ "what": p, "section": "models" }));
    }
    let down: Vec<&str> = view
        .models
        .iter()
        .filter(|r| !r.default && r.down_secs.is_some())
        .map(|r| r.reference.as_str())
        .collect();
    if !down.is_empty() {
        problems.push(json!({
            "what": format!("Skipped after failing: {}.", down.join(", ")),
            "section": "models",
        }));
    }
    if let Some(f) = live.and_then(|l| l.fixed.as_ref()) {
        problems.push(json!({
            "what": format!("The gateway was started with --provider {f}: every chat uses it, whatever the default says."),
            "fix": "Restart the gateway without --provider to follow the default.",
            "section": "models",
        }));
    }
    let unpriced = catalog::unpriced(&m.catalog());
    if !unpriced.is_empty() {
        problems.push(json!({
            "what": format!("No prices for {}: their cost shows as $0.", unpriced.join(", ")),
            "fix": "Fill missing prices from the catalog.",
            "action": "catalog/fill-prices",
            "section": "models",
        }));
    }
}

fn stop_turn(ctx: &Ctx, body: &Value) -> Answer {
    let Some(live) = &ctx.live else {
        return missing("the gateway");
    };
    let session = need!(arg(body, "session"));
    let Some(router) = live.router.upgrade() else {
        return missing("the gateway");
    };
    if router.stop(session, BY) {
        ok(json!({ "ok": true, "said": "Stopped." }))
    } else {
        bad(404, "nothing is running there now")
    }
}

fn kill(ctx: &Ctx, on: bool, body: &Value) -> Answer {
    let Some(hub) = &ctx.hub else {
        return missing("the trust hub");
    };
    need!(confirmed(
        body,
        if on {
            "Turn the kill switch on? Every run stops before its next model call, until it's off."
                .into()
        } else {
            "Turn the kill switch off? Runs and tasks go on.".into()
        }
    ));
    if on {
        let reason = body
            .get("reason")
            .and_then(Value::as_str)
            .map(|r| clip(r, 200));
        match hub.engage(BY, reason) {
            Ok(_) => ok(json!({ "ok": true, "said": "The kill switch is on." })),
            Err(e) => bad(500, e),
        }
    } else {
        match hub.clear(BY) {
            Ok(true) => ok(json!({ "ok": true, "said": "The kill switch is off." })),
            Ok(false) => ok(json!({ "ok": true, "said": "It was already off." })),
            Err(e) => bad(500, e),
        }
    }
}

// ---- Connections --------------------------------------------------------

fn connections(ctx: &Ctx) -> Answer {
    let Some(c) = &ctx.connections else {
        return ok(json!({ "available": false }));
    };
    match c.snapshot() {
        Ok(s) => ok(json!({
            "available": true,
            "connections": s.connections,
            "pending": s.pending,
            "asked": s.asked,
            "relay": s.relay.is_some(),
            "services": c.catalog().names(),
        })),
        Err(e) => bad(500, format!("{e:#}")),
    }
}

async fn connection_op(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    let Some(c) = &ctx.connections else {
        return missing("connections");
    };
    if path == "connections/disconnect" {
        let name = need!(arg(body, "name"));
        need!(confirmed(
            body,
            format!("Disconnect {name}? Its tools go away and its grant is revoked.")
        ));
        return match c.disconnect(name).await {
            Ok(said) => ok(json!({ "ok": true, "said": said })),
            Err(e) => bad(400, format!("{e:#}")),
        };
    }
    let service = need!(arg(body, "service"));
    let write = body.get("write").and_then(Value::as_bool) == Some(true);
    if c.catalog()
        .resolve(service)
        .is_ok_and(|s| s.auth == ferrule_connections::AuthKind::ApiKey)
    {
        return bad(
            400,
            format!("{service} takes an API key, which the page never handles: run `ferrule connections add {service}` on the server"),
        );
    }
    // As the owner in their chat: the flow's outcome goes there too.
    let actor = match ctx.owner_chat {
        Some(id) => ferrule_connections::Actor::Owner(ferrule_connections::Chat {
            channel: "telegram".into(),
            id: id.to_string(),
        }),
        None => ferrule_connections::Actor::Terminal,
    };
    match c.start(&actor, service, write).await {
        Ok(started) => {
            let links: Vec<Value> = started
                .reply
                .buttons
                .iter()
                .filter_map(|b| match &b.action {
                    ferrule_connections::Action::Url(u) => {
                        Some(json!({ "text": b.text, "url": u }))
                    }
                    ferrule_connections::Action::Command(_) => None,
                })
                .collect();
            ok(json!({ "ok": true, "said": started.reply.text, "links": links }))
        }
        Err(e) => bad(400, format!("{e:#}")),
    }
}

// ---- Models -------------------------------------------------------------

fn models(ctx: &Ctx) -> Answer {
    let Some(m) = &ctx.models else {
        return missing("the models");
    };
    let mut problems = Vec::new();
    model_problems(m, ctx.live.as_ref(), &mut problems);
    let view = m.view();
    ok(json!({
        "view": view,
        "problems": problems,
        "fixed": ctx.live.as_ref().and_then(|l| l.fixed.clone()),
        "unpriced": catalog::unpriced(&m.catalog()),
    }))
}

fn retire(ctx: &Ctx, session: Option<&str>) {
    if let Some(live) = &ctx.live {
        (live.retire)(session);
    }
}

async fn model_op(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    let Some(m) = &ctx.models else {
        return missing("the models");
    };
    let done = match path {
        "models/test" => {
            let word = need!(arg(body, "model"));
            return ok(json!(m.test(word).await));
        }
        "models/default" => {
            let word = need!(arg(body, "model"));
            let d = m.set_default(word, BY);
            if d.is_ok() {
                retire(ctx, None);
            }
            d
        }
        "models/pin" | "models/unpin" => {
            let chat = need!(arg(body, "chat"));
            let d = if path == "models/pin" {
                m.pin("telegram", chat, need!(arg(body, "model")), BY)
            } else {
                m.unpin("telegram", chat, BY)
            };
            if d.is_ok() {
                retire(ctx, Some(&format!("telegram__{chat}")));
            }
            d
        }
        "models/fallback" => {
            let words: Vec<String> = body
                .get("models")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            m.set_fallback(&words, BY)
        }
        "models/add" => {
            let provider = need!(arg(body, "provider"));
            let model = need!(arg(body, "model"));
            let alias = body
                .get("alias")
                .and_then(Value::as_str)
                .filter(|a| !a.trim().is_empty());
            m.add_model(provider, model, alias, BY)
        }
        _ => {
            let word = need!(arg(body, "model"));
            need!(confirmed(
                body,
                format!("Remove {word}? Pins and fallback entries naming it stop working.")
            ));
            let d = m.remove_model(word, BY);
            if d.is_ok() {
                retire(ctx, None);
            }
            d
        }
    };
    match done {
        Ok(d) => ok(json!({ "ok": true, "said": d.said, "view": d.view })),
        Err(e) => bad(400, format!("{e:#}")),
    }
}

fn config(ctx: &Ctx) -> Option<Config> {
    let text = std::fs::read_to_string(ctx.config_path.as_ref()?).ok()?;
    toml::from_str(&text).ok()
}

async fn listings(ctx: &Ctx, force: bool) -> Option<(Vec<catalog::Source>, Vec<catalog::Listing>)> {
    let cfg = config(ctx)?;
    let dir = ctx.data.as_ref()?.join("models").join("catalog");
    Some(catalog::listings_at(&cfg, &dir, force).await)
}

async fn catalog_list(ctx: &Ctx, req: &Request) -> Answer {
    let Some(m) = &ctx.models else {
        return missing("the models");
    };
    let q = |k: &str| req.query.get(k).map(String::as_str);
    let Some((_, listings)) = listings(ctx, q("refresh") == Some("1")).await else {
        return missing("the config");
    };
    let query = catalog::Query {
        search: q("search").map(str::to_string),
        all: q("tools") == Some("all"),
        sort: q("sort").unwrap_or("in").to_string(),
    };
    let mut f = catalog::filter(&listings, &m.catalog(), &query);
    let total = f.rows.len();
    f.rows.truncate(200);
    ok(json!({ "list": f, "total": total }))
}

async fn recommend(ctx: &Ctx) -> Answer {
    let Some(m) = &ctx.models else {
        return missing("the models");
    };
    let Some((sources, listings)) = listings(ctx, false).await else {
        return missing("the config");
    };
    let ledger = ctx.data.as_ref().map(|d| d.join("ledger.jsonl"));
    ok(json!(catalog::recommended_with(
        m,
        &sources,
        &listings,
        ledger.as_deref()
    )))
}

async fn catalog_add(ctx: &Ctx, body: &Value) -> Answer {
    let Some(m) = &ctx.models else {
        return missing("the models");
    };
    let id = need!(arg(body, "id"));
    let as_what = body.get("as").and_then(Value::as_str).unwrap_or("model");
    let Some((sources, listings)) = listings(ctx, false).await else {
        return missing("the config");
    };
    // The provider it's added to: the one asked for, else the connected
    // OpenRouter.
    let provider = match body.get("provider").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => match catalog::openrouter(&listings, &sources).and_then(|(_, p)| p) {
            Some(p) => p,
            None => {
                return bad(
                    400,
                    "OpenRouter isn't connected: add it with `ferrule setup` first, then pick the model here",
                )
            }
        },
    };
    let listing = listings
        .iter()
        .find(|l| l.provider.as_deref() == Some(provider.as_str()) && l.find(id).is_some())
        .or_else(|| {
            listings
                .iter()
                .find(|l| l.provider.is_none() && l.find(id).is_some())
        });
    match m
        .add_from_catalog(&provider, id, as_what, listing, BY)
        .await
    {
        Ok(d) => {
            if as_what == "default" {
                retire(ctx, None);
            }
            ok(json!({ "ok": true, "said": d.said, "view": d.view }))
        }
        Err(e) => bad(400, format!("{e:#}")),
    }
}

async fn fill_prices(ctx: &Ctx) -> Answer {
    let Some(m) = &ctx.models else {
        return missing("the models");
    };
    let Some((_, listings)) = listings(ctx, false).await else {
        return missing("the config");
    };
    match m.fill_prices(&listings, BY) {
        Ok(f) => {
            ok(json!({ "ok": true, "said": f.said, "filled": f.filled, "unknown": f.unknown }))
        }
        Err(e) => bad(400, format!("{e:#}")),
    }
}

// ---- Evaluating a candidate (M24 §2) -------------------------------------

/// The estimate, and on `start` (after the confirm) the run in the
/// background: `{model, provider?, suite: smoke|starter}`.
async fn eval_op(ctx: &Ctx, start: bool, body: &Value) -> Answer {
    let (Some(hub), Some(data)) = (&ctx.hub, &ctx.data) else {
        return missing("the trust hub");
    };
    let Some(cfg) = config(ctx) else {
        return missing("the config");
    };
    let model = need!(arg(body, "model"));
    let word = match body.get("provider").and_then(Value::as_str).map(str::trim) {
        Some(p) if !p.is_empty() && !model.starts_with(&format!("{p}/")) => format!("{p}/{model}"),
        _ => model.to_string(),
    };
    let subset = match model_eval::Subset::parse(
        body.get("suite").and_then(Value::as_str).unwrap_or("smoke"),
    ) {
        Ok(s) => s,
        Err(e) => return bad(400, format!("{e:#}")),
    };
    let mut c = match model_eval::candidate(&cfg, &word, None) {
        Ok(c) => c,
        Err(e) => return bad(400, format!("{e:#}")),
    };
    if c.pricing.is_none() {
        if let Some((_, listings)) = listings(ctx, false).await {
            if let Ok(priced) = model_eval::candidate(&cfg, &word, Some(&listings)) {
                c = priced;
            }
        }
    }
    let e = match model_eval::estimate(&cfg, hub, data, c, subset) {
        Ok(e) => e,
        Err(e) => return bad(400, format!("{e:#}")),
    };
    if !start {
        return ok(json!({ "estimate": e, "running": ctx.evals.running() }));
    }
    if let Some(why) = &e.refused {
        return bad(400, format!("It can't run now: {why}"));
    }
    if ctx.evals.running() {
        return bad(400, "An eval is already running: wait for it, or cancel it");
    }
    need!(confirmed(body, format!("{}Run it?", e.text)));
    let setup = model_eval::Setup {
        cfg,
        data: data.clone(),
        hub: hub.clone(),
        estimate: e,
        by: BY.into(),
    };
    match ctx.evals.start(setup) {
        Ok(()) => ok(
            json!({ "ok": true, "said": "Started: its progress is below.", "eval": ctx.evals.view() }),
        ),
        Err(_) => bad(400, "An eval is already running: wait for it, or cancel it"),
    }
}

fn eval_cancel(ctx: &Ctx, body: &Value) -> Answer {
    if !ctx.evals.running() {
        return bad(400, "No eval is running.");
    }
    need!(confirmed(
        body,
        "Cancel the eval? The task running now stops at its next model call; what finished is kept.".into()
    ));
    ctx.evals.cancel();
    ok(json!({ "ok": true, "said": "Cancelling: it stops at the next model call." }))
}

// ---- Usage --------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct Sum {
    calls: u64,
    errors: u64,
    retried: u64,
    input: u64,
    cached: u64,
    output: u64,
    usd: f64,
}

impl Sum {
    fn add(&mut self, r: &LedgerRecord) {
        self.calls += 1;
        // As `ferrule ledger` counts them: anything but "ok".
        self.errors += u64::from(r.is_error());
        self.retried += u64::from(r.outcome == "retried");
        self.input += r.input_tokens;
        self.cached += r.cached_input_tokens;
        self.output += r.output_tokens;
        self.usd += r.cost_usd.unwrap_or(0.0);
    }

    fn json(&self, key: &str) -> Value {
        json!({
            "key": key,
            "calls": self.calls,
            "errors": self.errors,
            "retried": self.retried,
            "input_tokens": self.input,
            "cached_input_tokens": self.cached,
            "output_tokens": self.output,
            "usd": (self.usd * 1e6).round() / 1e6,
        })
    }
}

/// The ledger's last `days` (1, 7 or 30): totals, per day, model, task and
/// chat, the cache hit rate, latency and error/retry rates, the caps.
/// Built from the same rows and grouping as `ferrule ledger`.
fn usage(ctx: &Ctx, req: &Request) -> Answer {
    let Some(data) = &ctx.data else {
        return missing("the ledger");
    };
    let days: i64 = match req.query.get("days").map(String::as_str) {
        Some("1") => 1,
        Some("30") => 30,
        _ => 7,
    };
    let since = Utc::now() - chrono::Duration::days(days);
    let (records, malformed) =
        match crate::ledger::read_records(&data.join("ledger.jsonl"), Some(since)) {
            Ok(v) => v,
            Err(e) => return bad(500, format!("{e:#}")),
        };
    ok(usage_of(&records, malformed, days, ctx.hub.as_deref()))
}

pub fn usage_of(
    records: &[LedgerRecord],
    malformed: usize,
    days: i64,
    hub: Option<&ferrule_trust::Hub>,
) -> Value {
    let rows = crate::ledger::aggregate(records);
    let calls: Vec<&LedgerRecord> = records
        .iter()
        .filter(|r| r.call_kind != "eval_result")
        .collect();
    let mut total = Sum::default();
    let mut per_day: BTreeMap<String, Sum> = BTreeMap::new();
    let mut per_task: BTreeMap<String, Sum> = BTreeMap::new();
    let mut per_chat: BTreeMap<String, Sum> = BTreeMap::new();
    let mut latencies: Vec<u64> = Vec::new();
    for r in &calls {
        total.add(r);
        latencies.push(r.latency_ms);
        let day = DateTime::parse_from_rfc3339(&r.timestamp)
            .map(|t| t.with_timezone(&Utc).format("%Y-%m-%d").to_string())
            .unwrap_or_else(|_| r.timestamp.chars().take(10).collect());
        per_day.entry(day).or_default().add(r);
        if r.eval.is_some() {
            per_task.entry("eval".into()).or_default().add(r);
        } else if r.task_shape == "scheduler" {
            let task = r.origin.clone().unwrap_or_else(|| "?".into());
            per_task.entry(format!("task {task}")).or_default().add(r);
        } else {
            per_chat.entry(r.session_id.clone()).or_default().add(r);
        }
    }
    latencies.sort_unstable();
    let pct = |n: u64| {
        if total.calls == 0 {
            0.0
        } else {
            (n as f64 * 1000.0 / total.calls as f64).round() / 10.0
        }
    };
    let models: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "shape": r.task_shape,
                "provider": r.provider,
                "model": r.model,
                "calls": r.calls,
                "errors": r.errors,
                "input_tokens": r.input_tokens,
                "cached_input_tokens": r.cached_input_tokens,
                "output_tokens": r.output_tokens,
                "cache_hit_pct": (r.cache_hit_pct() * 10.0).round() / 10.0,
                "p50_ms": r.p50_latency_ms,
                "p95_ms": r.p95_latency_ms,
                "usd": r.cost_usd,
                "priced_calls": r.priced_calls,
            })
        })
        .collect();
    let by = |m: BTreeMap<String, Sum>| -> Vec<Value> {
        let mut v: Vec<(String, Sum)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.usd.total_cmp(&a.1.usd).then(b.1.calls.cmp(&a.1.calls)));
        v.iter().map(|(k, s)| s.json(k)).collect()
    };
    let mut out = json!({
        "days": days,
        "malformed": malformed,
        "total": total.json("total"),
        "cache_hit_pct": if total.input == 0 { 0.0 } else { (total.cached as f64 * 1000.0 / total.input as f64).round() / 10.0 },
        "error_pct": pct(total.errors),
        "retry_pct": pct(total.retried),
        "p50_ms": crate::ledger::percentile(&latencies, 50.0),
        "p95_ms": crate::ledger::percentile(&latencies, 95.0),
        "per_day": per_day.iter().map(|(k, s)| s.json(k)).collect::<Vec<_>>(),
        "per_model": models,
        "per_task": by(per_task),
        "per_chat": by(per_chat),
    });
    if let Some(hub) = hub {
        out["caps"] = spend(hub, &mut Vec::new());
    }
    out
}

// ---- Tasks --------------------------------------------------------------

fn tasks(ctx: &Ctx) -> Answer {
    let Some(t) = &ctx.tasks else {
        return missing("the tasks");
    };
    match t.view(5) {
        Ok(rows) => ok(
            json!({ "tasks": rows, "paused": ctx.hub.as_ref().is_some_and(|h| h.stopped().is_some()) }),
        ),
        Err(e) => bad(500, format!("{e:#}")),
    }
}

fn task_op(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    let Some(t) = &ctx.tasks else {
        return missing("the tasks");
    };
    let id = need!(arg(body, "id"));
    let done = match path {
        "tasks/pause" => t.pause(id, BY),
        "tasks/resume" => t.resume(id, BY),
        "tasks/run" => t.run_now(id, BY),
        _ => {
            need!(confirmed(
                body,
                format!("Delete task {id} and its run history? This can't be undone.")
            ));
            t.delete(id, BY)
        }
    };
    match done {
        Ok(said) => ok(json!({ "ok": true, "said": said })),
        Err(e) => bad(400, format!("{e:#}")),
    }
}

// ---- Logs ---------------------------------------------------------------

const PAGE: usize = 50;

/// The audit log and the process's warnings and errors, newest first,
/// filtered by kind (`audit`, `warn`, or both) and text, 50 a page. Never a
/// transcript.
fn logs(ctx: &Ctx, req: &Request) -> Answer {
    let kind = req.query.get("kind").map(String::as_str).unwrap_or("all");
    let needle = req
        .query
        .get("q")
        .map(|q| q.trim().to_lowercase())
        .unwrap_or_default();
    let page: usize = req
        .query
        .get("page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    let mut rows: Vec<(String, Value)> = Vec::new();
    if kind != "warn" {
        if let Some(hub) = &ctx.hub {
            let since = Utc::now() - chrono::Duration::days(30);
            for e in hub.audit().read(Some(since)).unwrap_or_default() {
                let detail = clip(&e.detail.to_string(), 400);
                rows.push((
                    e.at.clone(),
                    json!({
                        "at": e.at,
                        "kind": "audit",
                        "level": e.event,
                        "text": detail,
                        "tree": e.tree,
                    }),
                ));
            }
        }
    }
    if kind != "audit" {
        for (at, level, msg) in RecentLog::global().entries() {
            let at = DateTime::<Utc>::from(at).to_rfc3339();
            // A URL's path can be its secret (a heartbeat check, an MCP
            // server's token): errors quote them whole.
            let msg = url_paths_hidden(&msg);
            rows.push((
                at.clone(),
                json!({ "at": at, "kind": "log", "level": level, "text": msg }),
            ));
        }
    }
    // Both sources come oldest first, and lines pushed in the same
    // microsecond (macOS's clock) tie: reversed, the stable sort keeps
    // them newest first.
    rows.reverse();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|(_, mut v)| {
            super::redact_value(&mut v, &ctx.redactor);
            v
        })
        .filter(|v| needle.is_empty() || v.to_string().to_lowercase().contains(&needle))
        .collect();
    let total = rows.len();
    let page_rows: Vec<Value> = rows.into_iter().skip(page * PAGE).take(PAGE).collect();
    ok(json!({ "rows": page_rows, "total": total, "page": page, "per_page": PAGE }))
}

// ---- Extensions and agents ---------------------------------------------

/// MCP servers (a command's name or a URL's host, never its env or
/// headers), skills and hooks. Read-only: they change in the config.
fn extensions(ctx: &Ctx) -> Answer {
    let Some(cfg) = config(ctx) else {
        return missing("the config");
    };
    let mcp: Vec<Value> = cfg
        .mcp
        .servers
        .iter()
        .map(|s| {
            let target = match &s.url {
                Some(u) => format!("{} (remote)", url_host(u)),
                None => std::path::Path::new(&s.command)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            };
            json!({ "name": s.name, "runs": target })
        })
        .collect();
    let skills: Vec<Value> = if cfg.skills.enabled {
        let ws = ctx.workspace.clone().unwrap_or_default();
        crate::discover_skills(&cfg.skills, &ws)
            .skills
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": clip(&s.description, 160),
                    "scope": format!("{:?}", s.scope).to_lowercase(),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    let hooks: Vec<Value> = cfg
        .hooks
        .entries()
        .iter()
        .map(|(event, h)| {
            json!({
                "event": event.name(),
                "matcher": h.matcher,
                "command": clip(&h.command, 80),
            })
        })
        .collect();
    ok(json!({
        "mcp": mcp,
        "skills": skills,
        "skills_enabled": cfg.skills.enabled,
        "skills_disabled": cfg.skills.disabled,
        "hooks": hooks,
    }))
}

/// Sub-agents that aren't closed. Read-only.
fn agents(ctx: &Ctx) -> Answer {
    let Some(data) = &ctx.data else {
        return missing("the agents");
    };
    let path = data.join("agents.db");
    if !path.exists() {
        return ok(json!({ "agents": [] }));
    }
    let rows = match ferrule_agents::AgentStore::open(path).and_then(|s| s.all()) {
        Ok(r) => r,
        Err(e) => return bad(500, e),
    };
    let agents: Vec<Value> = rows
        .iter()
        .filter(|r| r.parent.is_some() && r.status != ferrule_agents::Status::Closed)
        .map(|r| {
            json!({
                "id": r.id,
                "tree": r.tree,
                "parent": r.parent,
                "depth": r.depth,
                "name": r.name,
                "role": r.role,
                "task": clip(&ctx.redactor.redact(&r.task), 120),
                "status": r.status.as_str(),
                "tokens": r.tokens,
                "model": r.model,
                "branch": r.branch,
                "created_at": r.created_at,
                "updated_at": r.updated_at,
            })
        })
        .collect();
    ok(json!({ "agents": agents }))
}

#[cfg(test)]
#[path = "../../../ferrule-connections/tests/common/mod.rs"]
mod mock;

#[cfg(test)]
mod tests {
    use super::mock::{url_button, MockRelay, Provider, Recorder};
    use super::*;
    use ferrule_connections::catalog::{ReadOnly, Service};
    use ferrule_connections::{AuthKind, Connections, ConnectionsConfig};
    use ferrule_gateway::Redactor;
    use std::collections::HashMap;
    use std::path::Path;

    const SECRET: &str = "sk-SEEDED-0123456789abcdef";

    fn bare(dir: &Path) -> Ctx {
        let mut c = Ctx::bare(Arc::new(Redactor::new([SECRET.to_string()])));
        c.data = Some(dir.join("data"));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        c
    }

    fn hub(dir: &Path) -> Arc<ferrule_trust::Hub> {
        Arc::new(
            ferrule_trust::Hub::new(
                Default::default(),
                &dir.join("data"),
                &dir.join("data/ledger.jsonl"),
                Arc::new(ferrule_trust::SystemClock),
                vec![],
            )
            .unwrap(),
        )
    }

    fn request(path: &str) -> Request {
        // `path` may carry a query.
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        Request {
            method: "GET".into(),
            path: path.into(),
            query: url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>()
                .into_iter()
                .collect(),
            headers: Default::default(),
            body: vec![],
        }
    }

    async fn call(ctx: &Ctx, path: &str, body: Value) -> (u16, Value) {
        let (get, path) = match path.strip_prefix("POST ") {
            Some(p) => (false, p),
            None => (true, path),
        };
        let mut req = request(&format!("/api/{path}"));
        if !get {
            req.method = "POST".into();
        }
        let (status, mut v) = route(ctx, get, &req, &body).await.expect("an endpoint");
        super::super::redact_value(&mut v, &ctx.redactor);
        (status, v)
    }

    #[tokio::test]
    async fn the_kill_switch_asks_first_and_then_shows_on_top_in_the_log_and_the_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = bare(dir.path());
        let h = hub(dir.path());
        ctx.hub = Some(h.clone());
        ctx.tasks = Some(crate::tasks_admin::TasksAdmin::new(
            dir.path().join("data/tasks.db"),
            Some(h.clone()),
        ));

        let (s, v) = call(&ctx, "POST kill/on", json!({})).await;
        assert_eq!(s, 409, "{v}");
        assert!(v["confirm"].as_str().unwrap().contains("kill switch"));
        assert!(h.stopped().is_none(), "nothing happens before the confirm");

        let (s, v) = call(
            &ctx,
            "POST kill/on",
            json!({"confirm": true, "reason": "testing"}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        let (_, health) = call(&ctx, "health", json!({})).await;
        assert_eq!(health["kill"]["on"], true);
        assert_eq!(health["kill"]["by"], BY);
        assert!(
            health["problems"][0]["what"]
                .as_str()
                .unwrap()
                .contains("kill switch is on"),
            "{health}"
        );
        assert_eq!(health["problems"][0]["action"], "kill/off");
        let (_, tasks) = call(&ctx, "tasks", json!({})).await;
        assert_eq!(tasks["paused"], true);
        let (_, logs) = call(&ctx, "logs?kind=audit", json!({})).await;
        assert!(logs["rows"].to_string().contains("dashboard"), "{logs}");

        let (s, _) = call(&ctx, "POST kill/off", json!({"confirm": true})).await;
        assert_eq!(s, 200);
        assert!(h.stopped().is_none());
    }

    #[tokio::test]
    async fn logs_are_redacted_filtered_and_paged_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = bare(dir.path());
        ctx.hub = Some(hub(dir.path()));
        let log = RecentLog::global();
        log.push("WARN", &format!("the key {SECRET} was refused"));
        for i in 0..60 {
            log.push("ERROR", &format!("paging-probe {i}"));
        }
        let (_, v) = call(&ctx, "logs?kind=warn&q=paging-probe", json!({})).await;
        // The log keeps its last 50.
        let total = v["total"].as_u64().unwrap();
        assert!(total > 0 && total <= 50, "{v}");
        assert!(v["rows"][0]["text"]
            .as_str()
            .unwrap()
            .contains("paging-probe 59"));
        let (_, v) = call(&ctx, "logs?kind=warn", json!({})).await;
        assert!(!v.to_string().contains(SECRET), "{v}");
    }

    #[tokio::test]
    async fn extensions_show_names_and_hosts_never_env_headers_or_url_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = bare(dir.path());
        let config = dir.path().join("ferrule.toml");
        std::fs::write(
            &config,
            r#"
[skills]
enabled = false

[[mcp.servers]]
name = "local"
command = "/usr/local/bin/some-mcp"
args = ["--token", "ARG-SECRET-1"]
env = { TOKEN = "ENV-SECRET-2" }

[[mcp.servers]]
name = "remote"
url = "https://mcp.example.com/u/PATH-SECRET-3?key=Q-SECRET-4"
headers = { Authorization = "Bearer HEADER-SECRET-5" }

[hooks]
[[hooks.PreToolUse]]
command = "echo hi"
"#,
        )
        .unwrap();
        ctx.config_path = Some(config);
        let (s, v) = call(&ctx, "extensions", json!({})).await;
        assert_eq!(s, 200, "{v}");
        let text = v.to_string();
        assert!(
            text.contains("some-mcp") && text.contains("mcp.example.com"),
            "{text}"
        );
        assert!(!text.contains("SECRET"), "{text}");
        assert_eq!(v["hooks"][0]["event"], "PreToolUse", "{v}");
    }

    #[tokio::test]
    async fn deleting_a_task_asks_first() {
        use ferrule_gateway::{NewTask, TaskKind, TaskStore};
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = bare(dir.path());
        let path = dir.path().join("data/tasks.db");
        let id = TaskStore::open(&path)
            .unwrap()
            .add(
                NewTask {
                    name: "digest".into(),
                    kind: TaskKind::Cron,
                    schedule: "0 9 * * *".into(),
                    timezone: "UTC".into(),
                    channel: "telegram".into(),
                    chat_id: "42".into(),
                    prompt: "p".into(),
                    gate: None,
                    model: None,
                },
                "t-1".into(),
                0,
                Some(4_000_000_000),
            )
            .unwrap()
            .id;
        ctx.tasks = Some(crate::tasks_admin::TasksAdmin::new(path, None));
        let (s, v) = call(&ctx, "POST tasks/delete", json!({ "id": id })).await;
        assert_eq!(s, 409, "{v}");
        assert!(v["confirm"].as_str().unwrap().contains("can't be undone"));
        let (_, v) = call(&ctx, "tasks", json!({})).await;
        assert_eq!(v["tasks"].as_array().unwrap().len(), 1);
        let (s, _) = call(&ctx, "POST tasks/pause", json!({ "id": id })).await;
        assert_eq!(s, 200, "pausing needs no confirm");
        let (s, _) = call(
            &ctx,
            "POST tasks/delete",
            json!({ "id": id, "confirm": true }),
        )
        .await;
        assert_eq!(s, 200);
        let (_, v) = call(&ctx, "tasks", json!({})).await;
        assert!(v["tasks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn usage_totals_are_the_ledgers_and_evals_dont_count() {
        let rec = |shape: &str, outcome: &str, cost: f64, eval: bool| -> LedgerRecord {
            let mut v = json!({
                "timestamp": Utc::now().to_rfc3339(),
                "session_id": if shape == "scheduler" { "scheduler__t1" } else { "telegram__42" },
                "provider": "p", "model": "m", "task_shape": shape,
                "iteration": 0, "tool_calls": 0,
                "input_tokens": 1000, "cached_input_tokens": 400, "output_tokens": 50,
                "latency_ms": 120, "outcome": outcome, "cost_usd": cost,
                "origin": if shape == "scheduler" { Some("t1") } else { None },
            });
            if eval {
                v["call_kind"] = json!("eval_result");
            }
            serde_json::from_value(v).unwrap()
        };
        let records = vec![
            rec("chat", "ok", 0.01, false),
            rec("chat", "retried", 0.02, false),
            rec("scheduler", "error", 0.0, false),
            rec("chat", "ok", 5.0, true),
        ];
        let u = usage_of(&records, 1, 7, None);
        let rows = crate::ledger::aggregate(&records);
        let calls: u64 = rows.iter().map(|r| r.calls as u64).sum();
        let usd: f64 = rows.iter().filter_map(|r| r.cost_usd).sum();
        assert_eq!(u["total"]["calls"], calls);
        assert!(
            (u["total"]["usd"].as_f64().unwrap() - usd).abs() < 1e-9,
            "{u}"
        );
        assert_eq!(u["total"]["calls"], 3);
        assert_eq!(u["cache_hit_pct"], 40.0);
        assert_eq!(
            u["total"]["errors"],
            rows.iter().map(|r| r.errors as u64).sum::<u64>()
        );
        assert_eq!(u["error_pct"], 66.7);
        assert_eq!(u["retry_pct"], 33.3);
        assert_eq!(u["per_task"][0]["key"], "task t1");
        assert_eq!(u["per_chat"][0]["key"], "telegram__42");
        assert_eq!(u["malformed"], 1);
    }

    #[test]
    fn log_lines_keep_a_urls_host_and_hide_its_path() {
        assert_eq!(
            url_paths_hidden("error sending request for url (http://h:9/mcp/TOKEN?k=v): refused"),
            "error sending request for url (http://h:9/…): refused"
        );
        assert_eq!(
            url_paths_hidden("https://u:pw@hc-ping.com/uuid and https://x.io"),
            "https://hc-ping.com/… and https://x.io"
        );
        assert_eq!(url_paths_hidden("no url here"), "no url here");
    }

    #[test]
    fn the_heartbeat_shows_its_host_only() {
        assert_eq!(url_host("https://hc-ping.com/abc-uuid"), "hc-ping.com");
        assert_eq!(
            url_host("https://user:pw@h.example:8443/x?y"),
            "h.example:8443"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_and_disconnect_a_mock_service_through_the_connections_api() {
        const KEY: &str = "relay-key-for-tests-0123456789";
        let dir = tempfile::tempdir().unwrap();
        let provider = Provider::start().await;
        let relay = MockRelay::start(KEY).await;
        let mut mock = Service::from_url(&provider.mcp_url()).unwrap();
        mock.name = "mock".into();
        mock.title = "Mock".into();
        mock.scopes = vec!["read".into()];
        mock.write_scopes = vec!["read".into(), "write".into()];
        mock.read_only = ReadOnly::Scope;
        let mut keyed = mock.clone();
        keyed.name = "keyed".into();
        keyed.auth = AuthKind::ApiKey;
        let events = Arc::new(Recorder::default());
        let conns = Connections::new(
            &dir.path().join("private"),
            ConnectionsConfig {
                relay_url: Some(relay.url.clone()),
                cloudflared: Some("off".into()),
                custom: vec![mock, keyed],
                ..Default::default()
            },
            Arc::new(|name: &str| (name == "FERRULE_RELAY_KEY").then(|| KEY.to_string())),
            events.clone(),
            None,
        )
        .unwrap()
        .with_timing(Duration::from_secs(20), Duration::from_millis(20));
        let mut ctx = bare(dir.path());
        ctx.connections = Some(conns.clone());
        ctx.owner_chat = Some(42);

        let (s, v) = call(
            &ctx,
            "POST connections/connect",
            json!({"service": "keyed"}),
        )
        .await;
        assert_eq!(s, 400);
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("ferrule connections add keyed"));

        let (s, v) = call(&ctx, "POST connections/connect", json!({"service": "mock"})).await;
        assert_eq!(s, 200, "{v}");
        let link = v["links"][0]["url"]
            .as_str()
            .expect("a sign-in link")
            .to_string();
        let (_, v) = call(&ctx, "connections", json!({})).await;
        assert_eq!(v["pending"], json!(["mock"]), "{v}");
        let location = provider.approve(&link).await;
        for _ in 0..200 {
            if relay.visit(&location).await == 200 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        events.wait_told("is connected").await;
        let (_, v) = call(&ctx, "connections", json!({})).await;
        assert_eq!(v["connections"][0]["name"], "mock", "{v}");
        assert!(!v.to_string().contains(&provider.tokens_issued()[0]));

        let (s, v) = call(&ctx, "POST connections/disconnect", json!({"name": "mock"})).await;
        assert_eq!(s, 409, "{v}");
        assert_eq!(conns.snapshot().unwrap().connections.len(), 1);
        let (s, v) = call(
            &ctx,
            "POST connections/disconnect",
            json!({"name": "mock", "confirm": true}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert!(v["said"].as_str().unwrap().contains("revoked"), "{v}");
        assert!(conns.snapshot().unwrap().connections.is_empty());
        let _ = url_button;
    }

    #[tokio::test]
    async fn every_section_says_when_it_isnt_available() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::bare(Arc::new(Redactor::new([SECRET.to_string()])));
        for p in ["models", "tasks", "extensions", "agents", "usage"] {
            let (s, _) = call(&ctx, p, json!({})).await;
            assert_eq!(s, 503, "{p}");
        }
        let (s, v) = call(&ctx, "connections", json!({})).await;
        assert_eq!((s, v["available"].clone()), (200, json!(false)));
        let (s, _) = call(&ctx, "POST turn/stop", json!({"session": "x"})).await;
        assert_eq!(s, 503);
        drop(dir);
    }
}
