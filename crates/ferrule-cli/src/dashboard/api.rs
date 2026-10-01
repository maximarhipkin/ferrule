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
use crate::models::routing_admin;
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

tokio::task_local! {
    /// Who a change made through `act` is audited as (M48: the chat tool
    /// acts as "telegram chat 42 (approved a3)", not as the page).
    static ACTOR: String;
}

/// Who the audit log says made the change in hand: the page, unless `act`
/// was called for someone else.
pub fn by() -> String {
    ACTOR
        .try_with(String::clone)
        .unwrap_or_else(|_| BY.to_string())
}

/// What only the running gateway has.
#[derive(Clone)]
pub struct Live {
    pub router: Weak<Router>,
    pub health: Arc<Health>,
    pub channels: Vec<Arc<dyn Channel>>,
    /// The gateway's `--provider`: every chat is on it until a restart.
    pub fixed: Option<String>,
    /// Retires one lane, or every chat's, after a model change.
    pub retire: Retire,
    /// M37: a channel's loop started again from the page.
    pub restarts: Arc<ferrule_gateway::ChannelRestarts>,
    /// Why commands are refused here (managed mode), for /healthz.
    pub commands_off: Option<String>,
}

pub(crate) type Answer = Option<(u16, Value)>;

pub(crate) fn ok(v: Value) -> Answer {
    Some((200, v))
}

pub(crate) fn bad(status: u16, why: impl std::fmt::Display) -> Answer {
    Some((status, json!({ "error": why.to_string() })))
}

pub(super) fn missing(what: &str) -> Answer {
    bad(503, format!("{what} isn't available in this process"))
}

/// A destructive operation's second step: `409` with the question until
/// the body says `"confirm": true`.
pub(super) fn confirmed(body: &Value, question: String) -> Result<(), Answer> {
    if body.get("confirm").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(Some((409, json!({ "confirm": question }))))
    }
}

pub(crate) fn arg<'a>(body: &'a Value, key: &str) -> Result<&'a str, Answer> {
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
pub(super) use need;

pub async fn route(ctx: &Ctx, get: bool, req: &Request, body: &Value) -> Answer {
    let path = req.path.strip_prefix("/api/")?;
    if get {
        return match path {
            "health" => ok(health(ctx)),
            "managed" => ok(managed_view(ctx)),
            "setup" => super::setup::view(ctx),
            "connections" => connections(ctx).await,
            "connections/checklist" => connections_checklist(ctx).await,
            "models" => models(ctx),
            "models/choices" => super::models_page::choices(ctx),
            "models/provider/list" => super::models_page::provider_list(ctx, req).await,
            "plans/chatgpt/poll" => super::models_page::chatgpt_poll(ctx),
            "routing" => routing(ctx, req),
            "catalog" => catalog_list(ctx, req).await,
            "recommend" => recommend(ctx).await,
            "usage" => usage(ctx, req),
            "tasks" => tasks(ctx),
            "logs" => logs(ctx, req),
            "extensions" | "settings" => settings_view(ctx),
            "agents" => agents(ctx),
            "eval" => ok(ctx.evals.view()),
            "run" => run_view(ctx, req),
            "runs" => ok(json!({ "runs": ctx.runs.list() })),
            "console/job" => super::console::job(ctx, req),
            "console/complete" => super::console::complete(ctx, req),
            "console/parity" => super::console::parity(),
            "chat" => super::chat::view(ctx, req),
            "approvals" => super::chat::approvals(ctx),
            "config" => super::config_page::get(ctx),
            "channels" => super::channels::list(ctx),
            "memory" => super::memory::list(ctx, req).await,
            "backups" => super::backup_page::list(ctx),
            _ => None,
        };
    }
    act(ctx, path, body, BY).await
}

/// A page's GET, for the chat tool: the same view, as JSON.
pub(crate) async fn read(ctx: &Ctx, path: &str) -> Result<Value, String> {
    let req = Request {
        method: "GET".into(),
        path: format!("/api/{path}"),
        query: BTreeMap::new(),
        headers: BTreeMap::new(),
        body: Vec::new(),
    };
    match route(ctx, true, &req, &Value::Null).await {
        Some((200, v)) => Ok(v),
        Some((_, v)) => Err(v["error"].as_str().unwrap_or("it failed").to_string()),
        None => Err(format!("`{path}` isn't available")),
    }
}

/// Every change the page can make, as `by` (M48: the chat's admin tool
/// calls this after the owner's approval, so a change is one code path
/// whoever asks, and the audit row names who did).
pub(crate) async fn act(ctx: &Ctx, path: &str, body: &Value, by: &str) -> Answer {
    ACTOR.scope(by.to_string(), post(ctx, path, body)).await
}

async fn post(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    match path {
        "turn/stop" => stop_turn(ctx, body),
        "kill/on" | "kill/off" => kill(ctx, path == "kill/on", body),
        "models/default" | "models/pin" | "models/unpin" | "models/fallback" | "models/add"
        | "models/remove" | "models/test" => model_op(ctx, path, body).await,
        "models/provider" => super::models_page::provider_save(ctx, body).await,
        "plans/chatgpt/start" => match crate::managed::kind_refusal("chatgpt", "chatgpt") {
            Some(why) => bad(403, why),
            None => super::models_page::chatgpt_start(ctx).await,
        },
        "plans/chatgpt/cancel" => super::models_page::chatgpt_cancel(ctx),
        "plans/claude" if crate::managed::on() => bad(403, crate::managed::NO_CLAUDE_PLAN),
        "plans/claude" => super::models_page::claude_token(ctx, body),
        "routing/set" | "routing/unset" => routing_op(ctx, path, body),
        "catalog/add" => catalog_add(ctx, body).await,
        "catalog/fill-prices" => fill_prices(ctx).await,
        "connections/connect" | "connections/disconnect" => connection_op(ctx, path, body).await,
        "connections/key" | "connections/test" | "connections/cancel" => {
            connection_key_op(ctx, path, body).await
        }
        "connections/relay/deploy"
        | "connections/relay/use"
        | "connections/relay/check"
        | "connections/google-client" => connection_setup_op(ctx, path, body).await,
        "tasks/pause" | "tasks/resume" | "tasks/run" | "tasks/delete" => task_op(ctx, path, body),
        "tasks/schedule" | "tasks/model" => task_edit(ctx, path, body),
        "settings/caps" | "mcp/disable" | "mcp/enable" | "mcp/remove" | "skills/disable"
        | "skills/enable" | "hooks/trust" | "hooks/untrust" => settings_op(ctx, path, body).await,
        "eval/estimate" | "eval/start" => eval_op(ctx, path == "eval/start", body).await,
        "eval/cancel" => eval_cancel(ctx, body),
        "notices/dismiss" | "notices/restore" => notices_op(ctx, path, body),
        "doctor/run" => doctor_run(ctx, body),
        "console/run" => super::console::run(ctx, body),
        "console/cancel" => super::console::cancel(ctx, body),
        "chat/send" => super::chat::send(ctx, body).await,
        "chat/photo" => super::chat::photo(ctx, body).await,
        "backup" => super::backup_page::start(ctx),
        "backups/delete" => super::backup_page::delete(ctx, body),
        "memory/forget" => super::memory::forget(ctx, body).await,
        "tasks/preview" => task_preview(body),
        "tasks/add" => task_add(ctx, body).await,
        "approvals/answer" => super::chat::answer(ctx, body),
        "config/check" => super::config_page::check(ctx, body),
        "config/save" => super::config_page::save(ctx, body),
        "config/set" => super::config_page::set(ctx, body),
        "config/undo" => super::config_page::undo(ctx),
        "run/cancel" => {
            let id = need!(arg(body, "id"));
            ok(json!({ "ok": ctx.runs.cancel(id) }))
        }
        "channels/restart" => channel_restart(ctx, body),
        "channels/test" => super::channels::test(ctx, body).await,
        "channels/save" => super::channels::save(ctx, body),
        "channels/remove" => super::channels::remove(ctx, body),
        "channels/keys/add" => super::channels::key_add(ctx, body),
        "channels/keys/revoke" => super::channels::key_revoke(ctx, body),
        "telegram/test" => super::telegram::test(ctx, body).await,
        "telegram/save" => super::telegram::save(ctx, body),
        "telegram/wait" => super::telegram::wait(ctx, body).await,
        "telegram/allow" => super::telegram::allow(ctx, body).await,
        "config/restore" => config_restore(ctx, body),
        "gateway/restart" => gateway_restart(ctx, body),
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
    if let Some(name) = crate::instance::current() {
        out["instance"] = json!(name);
    }
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
                        "id": format!("stuck:{}", l.session_id),
                        "fixes": [{ "label": "Stop it", "action": "turn/stop", "body": { "session": l.session_id } }],
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
                "id": format!("channel:{c}"),
                "fixes": [{ "label": format!("Restart {c}"), "action": "channels/restart", "body": { "name": c } }],
                "what": format!("{c} hasn't polled successfully for over {}", human(h.settings().poll_stale)),
                "fix": "Check the network and the bot token; the gateway keeps retrying.",
                "section": "health",
            }));
        }
        for c in &live.channels {
            if let Some(p) = c.problem() {
                problems.push(json!({
                    "id": format!("channel-problem:{}", c.name()),
                    "fixes": [
                        { "label": format!("Restart {}", c.name()), "action": "channels/restart", "body": { "name": c.name() } },
                        { "label": "Run doctor", "action": "doctor/run", "body": {} },
                    ],
                    "what": format!("{}: {}", c.name(), ctx.redactor.redact(&p)),
                    "fix": format!("`ferrule doctor` checks the {} token and setup; docs/{}.md has the steps.", c.name(), c.name()),
                    "section": "health",
                }));
            }
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
                    // A dead socket, a rejected token (M31).
                    "problem": c.problem().map(|p| ctx.redactor.redact(&p)),
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
                    "id": "kill",
                    "fixes": [{ "label": "Turn it off", "action": "kill/off", "body": {} }],
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
    // M34: the remote workspace's link and the local model's server.
    if ctx.live.is_some() {
        if let Some(line) = crate::remote::status_lines().into_iter().next() {
            out["workspace"] = json!(ctx.redactor.redact(&line));
        }
        if let Some(down) = crate::remote::probe() {
            problems.push(json!({
                "id": "workspace",
                "fixes": [{ "label": "Run doctor", "action": "doctor/run", "body": {} }],
                "what": ctx.redactor.redact(&down),
                "fix": "The link reconnects by itself; `ferrule ssh test <name>` says why it can't.",
                "section": "health",
            }));
        }
        for line in crate::local::problems() {
            problems.push(json!({
                "fixes": [{ "label": "Run doctor with model pings", "action": "doctor/run", "body": { "ping_models": true } }],
                "what": ctx.redactor.redact(&line),
                "fix": "`ferrule doctor --ping-models` has the fix; docs/local-models.md explains it.",
                "section": "health",
            }));
        }
    }
    // M36: the update state; what went wrong is a problem too.
    if let Some(data) = ctx.data.as_deref() {
        let auto = crate::config::Config::load()
            .ok()
            .and_then(|(c, _)| c.update.auto);
        let report = crate::update::report(data, auto, crate::update::units_installed());
        for (_, line) in report
            .iter()
            .filter(|(t, _)| *t == crate::update::Tone::Warn)
        {
            problems.push(json!({
                "fixes": [{ "label": "Check for an update", "action": "console/run", "body": { "line": "update --check" } }],
                "what": ctx.redactor.redact(line),
                "fix": "`ferrule doctor` shows the update state; docs/updates.md explains it.",
                "section": "health",
            }));
        }
        out["updates"] = json!(report.into_iter().map(|(_, l)| l).collect::<Vec<_>>());
        // M36 §7: the self-check's problems and the last ten repairs.
        if let Some((_, found)) = crate::selfcheck::last(data) {
            for (key, line) in &found {
                problems.push(json!({
                    "id": format!("selfcheck:{key}"),
                    "fixes": [{ "label": "Run doctor", "action": "doctor/run", "body": {} }],
                    "what": ctx.redactor.redact(line),
                    "fix": "The self-check tells the owner when it's fixed; `ferrule doctor` shows it too.",
                    "section": "health",
                }));
            }
        }
        out["repairs"] = json!(ferrule_core::repairs::recent(data, 10)
            .iter()
            .rev()
            .map(|r| ctx.redactor.redact(&ferrule_core::repairs::line(r)))
            .collect::<Vec<_>>());
    }
    // M37 §3.5: a connection that stopped, or whose key is about to.
    if let Some(c) = &ctx.connections {
        for a in c.attention(7) {
            problems.push(json!({
                "id": format!("connection:{}", a.name),
                "what": ctx.redactor.redact(&a.text),
                "fix": "Connections has its guide and a Test button.",
                "section": "connections",
                "fixes": [{ "label": format!("Reconnect {}", a.title), "section": "connections", "tile": a.tile }],
            }));
        }
    }
    // M37 §1: every strip closes but two, and one closed less than a day
    // ago while still true is under `hidden`.
    let (shown, hidden) = match ctx.data.as_deref() {
        Some(data) => super::notices::Notices::at(data).split(&owner_key(ctx), problems, now()),
        None => (problems, Vec::new()),
    };
    out["problems"] = json!(shown);
    out["hidden"] = json!(hidden);
    out
}

fn now() -> u64 {
    unix(SystemTime::now()).max(0) as u64
}

/// Whose dismissals these are: the owner's primary chat, else "owner".
fn owner_key(ctx: &Ctx) -> String {
    ctx.owner_chat.as_ref().map_or_else(
        || "owner".to_string(),
        |c| format!("{}:{}", c.channel, c.chat),
    )
}

/// `POST /api/notices/dismiss {id}`: only one of today's problems closes,
/// and not the two that never do.
fn notices_op(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    let Some(data) = ctx.data.as_deref() else {
        return missing("the data directory");
    };
    let store = super::notices::Notices::at(data);
    let owner = owner_key(ctx);
    if path == "notices/restore" {
        let id = body.get("id").and_then(Value::as_str);
        return match store.restore(&owner, id, now()) {
            Ok(n) => ok(json!({ "ok": true, "restored": n, "said": format!("{n} shown again.") })),
            Err(e) => bad(500, e),
        };
    }
    let id = need!(arg(body, "id"));
    if let Some(why) = super::notices::unclosable(id) {
        return bad(409, format!("This one can't be hidden: {why}."));
    }
    let health = health(ctx);
    let Some(problem) = health["problems"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(health["hidden"].as_array().into_iter().flatten())
        .find(|p| p["id"] == id)
    else {
        return bad(404, "That notice isn't showing any more.");
    };
    match store.dismiss(&owner, problem, now()) {
        Ok(until) => ok(
            json!({ "ok": true, "until": until, "said": "Hidden for a day; it comes back if it's still true." }),
        ),
        Err(e) => bad(409, e),
    }
}

/// `https://hc-ping.com/<uuid>` → `hc-ping.com`.
pub(crate) fn url_host(url: &str) -> String {
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
                "id": format!("cap:{name}"),
                "fixes": [{ "label": "Edit caps", "section": "usage" }],
                "what": format!("Today's cap {name} is used up: runs stop until tomorrow."),
                "fix": "Raise it under Edit caps below (or /caps in Telegram), or wait for the day to turn.",
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
    let fix = |what: String| {
        let mut p = fix(what);
        let d = default.map_or("none", |d| d.reference.as_str());
        let can = view
            .models
            .iter()
            .any(|r| r.key_present && r.down_secs.is_none());
        p["id"] = json!(if can {
            format!("model-down:{d}")
        } else {
            "models-none".to_string()
        });
        if let Some(h) = &healthy {
            p["fixes"] = json!([{ "label": format!("Make {h} the default"), "action": "models/default", "body": { "model": h } }]);
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
                "The default model {} can't be called: {}.",
                d.reference, d.missing
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
            "id": "provider-fixed",
            "what": format!("The gateway was started with --provider {f}: every chat uses it, whatever the default says."),
            "fix": "Restart the gateway without --provider to follow the default.",
            "section": "models",
        }));
    }
    let unpriced = catalog::unpriced(&m.catalog());
    if !unpriced.is_empty() {
        problems.push(json!({
            "id": "prices",
            "fixes": [{ "label": "Fill prices", "action": "catalog/fill-prices", "body": {} }],
            // Each line is already a sentence ("x has no prices, so …").
            "what": format!("{}.", unpriced.join("; ")),
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
    if router.stop(session, &by()) {
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
        match hub.engage(&by(), reason) {
            Ok(_) => ok(json!({ "ok": true, "said": "The kill switch is on." })),
            Err(e) => bad(500, e),
        }
    } else {
        match hub.clear(&by()) {
            Ok(true) => ok(json!({ "ok": true, "said": "The kill switch is off." })),
            Ok(false) => ok(json!({ "ok": true, "said": "It was already off." })),
            Err(e) => bad(500, e),
        }
    }
}

// ---- Connections --------------------------------------------------------

/// Connections: what's connected, the flows waiting, what needs
/// attention, the relay, and the catalog as tiles, each way in with its
/// guide and fields (never a value) and, when it can't work yet, why.
async fn connections(ctx: &Ctx) -> Answer {
    let Some(c) = &ctx.connections else {
        return ok(json!({ "available": false }));
    };
    let s = match c.snapshot() {
        Ok(s) => s,
        Err(e) => return bad(500, format!("{e:#}")),
    };
    let relay_live = c.live_relay().await.is_some();
    let mut tiles: Vec<Value> = Vec::new();
    for svc in c.catalog().services() {
        let connected = s.connections.iter().any(|x| x.service == svc.name);
        let blocked = c.blocked(svc, relay_live).map(|r| r.text);
        // What the owner may have seen on Google's or Atlassian's page,
        // and what to do: for the ways in that go through their servers.
        let symptoms: Vec<Value> = if svc.native.is_none()
            && (ferrule_connections::explain::is_google(svc)
                || ferrule_connections::explain::is_atlassian(svc))
        {
            ferrule_connections::explain::symptoms(ferrule_connections::explain::is_google(svc))
                .into_iter()
                .map(|(id, saw, fix)| json!({ "id": id, "saw": saw, "fix": fix }))
                .collect()
        } else {
            Vec::new()
        };
        let option = json!({
            "name": svc.name,
            "title": svc.title(),
            "option": svc.option,
            "covers": svc.covers,
            "guide": svc.guide,
            "auth": svc.auth,
            "fields": svc.key_fields(),
            "preview": svc.preview,
            "fixed_callback": svc.fixed_callback,
            "connected": connected,
            "blocked": blocked,
            "symptoms": symptoms,
            // The last try's plain explanation, and the simpler way in to
            // switch to (never the provider's raw error).
            "attempt": c.last_attempt(&svc.name).map(|a| json!({
                "at": a.at,
                "text": ctx.redactor.redact(&a.text),
                "switch_to": a.switch_to,
            })),
        });
        match tiles.iter_mut().find(|t| t["tile"] == svc.tile()) {
            Some(t) => t["options"].as_array_mut().unwrap().push(option),
            None => tiles.push(json!({
                "tile": svc.tile(),
                "title": svc.title(),
                "options": [option],
            })),
        }
    }
    for t in &mut tiles {
        let any = t["options"]
            .as_array()
            .is_some_and(|o| o.iter().any(|x| x["connected"] == true));
        t["connected"] = json!(any);
    }
    let callbacks: Vec<Value> = crate::connections_setup::callbacks(c)
        .into_iter()
        .map(|(service, how)| json!({ "service": service, "callback": how }))
        .collect();
    let relay_url = c.relay_url();
    ok(json!({
        "available": true,
        "connections": s.connections,
        "pending": s.pending,
        "pending_flows": s.pending_flows,
        "asked": s.asked,
        "relay": s.relay.is_some(),
        "relay_url": relay_url,
        "relay_live": relay_live,
        "callback": relay_url.as_deref().map(|u| format!("{}/cb", u.trim_end_matches('/'))),
        "callbacks": callbacks,
        "attention": c.attention(7),
        "services": c.catalog().names(),
        "tiles": tiles,
    }))
}

async fn connections_checklist(ctx: &Ctx) -> Answer {
    let Some(c) = &ctx.connections else {
        return missing("connections");
    };
    ok(serde_json::to_value(c.checklist().await).unwrap_or_default())
}

/// Where setup from the page writes: this process's config and secrets.
pub(super) fn setup_place(ctx: &Ctx) -> Result<crate::connections_setup::Place, Answer> {
    match (&ctx.config_path, &ctx.data) {
        (Some(config), Some(data)) => Ok(crate::connections_setup::Place {
            config: config.clone(),
            secrets: data.join("private").join("secrets.env"),
        }),
        _ => Err(missing("the config file")),
    }
}

/// A key-based way in (write-only: the values go in, only the outcome
/// comes back), a connection's test, a waiting flow cancelled.
async fn connection_key_op(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    let Some(c) = &ctx.connections else {
        return missing("connections");
    };
    match path {
        "connections/key" => {
            let service = need!(arg(body, "service"));
            let write = body.get("write").and_then(Value::as_bool) == Some(true);
            let fields: std::collections::BTreeMap<String, String> = body
                .get("fields")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect();
            match c.connect_key(service, fields, write, "dashboard").await {
                Ok(said) => ok(json!({ "ok": true, "said": said })),
                Err(why) => bad(400, ctx.redactor.redact(&why)),
            }
        }
        "connections/test" => {
            let name = need!(arg(body, "name"));
            match c.test(name).await {
                Ok(said) => ok(json!({ "ok": true, "said": said })),
                Err(why) => ok(json!({ "ok": false, "said": ctx.redactor.redact(&why) })),
            }
        }
        _ => {
            let id = need!(arg(body, "id"));
            match c.cancel(id) {
                Ok(said) => ok(json!({ "ok": true, "said": said })),
                Err(e) => bad(404, format!("{e:#}")),
            }
        }
    }
}

/// The fixed callback address (deploy a relay, use one, check it) and
/// Google's OAuth client. Tokens and keys go in; none comes back.
async fn connection_setup_op(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    use crate::connections_setup as setup;
    let Some(c) = &ctx.connections else {
        return missing("connections");
    };
    let place = need!(setup_place(ctx));
    let text = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim);
    let steps_json = |steps: &[(String, bool)]| -> Value {
        json!(steps
            .iter()
            .map(|(s, ok)| json!({ "step": s, "ok": ok }))
            .collect::<Vec<_>>())
    };
    let done = match path {
        "connections/relay/deploy" => {
            match setup::deploy_relay(
                c,
                &place,
                &ctx.cf_api,
                text("token"),
                text("account"),
                &crate::instance::relay_worker(crate::instance::current().as_deref()),
                5,
            )
            .await
            {
                Ok(setup::Deployed::Done {
                    url,
                    callback,
                    steps,
                }) => Ok(json!({
                    "ok": true,
                    "said": format!("The relay is at {url} and works."),
                    "relay_url": url,
                    "callback": callback,
                    "steps": steps_json(&steps),
                })),
                Ok(setup::Deployed::ChooseAccount { accounts }) => Ok(json!({
                    "ok": false,
                    "choose": accounts,
                    "said": "The token reaches several Cloudflare accounts: pick the one the relay goes in.",
                })),
                Err(e) => Err(e),
            }
        }
        "connections/relay/use" => {
            let url = need!(arg(body, "url"));
            let key = need!(arg(body, "key"));
            setup::use_relay(c, &place, url, key)
                .await
                .map(|(callback, steps)| {
                    json!({
                        "ok": true,
                        "said": "The relay works and is used from now on.",
                        "callback": callback,
                        "steps": steps_json(&steps),
                    })
                })
        }
        "connections/relay/check" => setup::check_relay(c, &place).await.map(|steps| {
            let all = steps.iter().all(|(_, ok)| *ok);
            json!({
                "ok": all,
                "said": if all { "The relay works." } else { "The relay doesn't work: the failed step is marked." },
                "steps": steps_json(&steps),
            })
        }),
        _ => {
            let id = need!(arg(body, "id"));
            let secret = need!(arg(body, "secret"));
            setup::save_google_client(c, &place, id, secret)
                .map(|said| json!({ "ok": true, "said": said }))
        }
    };
    match done {
        Ok(v) => ok(v),
        Err(e) => bad(400, ctx.redactor.redact(&format!("{e:#}"))),
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
    if let Ok(svc) = c.catalog().resolve(service) {
        if svc.auth == ferrule_connections::AuthKind::ApiKey {
            return bad(
                400,
                format!(
                    "{} takes a key: fill in its form (it's sent once and never shown again)",
                    svc.title()
                ),
            );
        }
        // Never a sign-in bound to fail: say why and what to do first.
        if let Some(why) = c.blocked(&svc, c.live_relay().await.is_some()) {
            return bad(400, why.text);
        }
    }
    // As the owner in their chat: the flow's outcome goes there too.
    let actor = match &ctx.owner_chat {
        Some(owner) => ferrule_connections::Actor::Owner(ferrule_connections::Chat {
            channel: owner.channel.clone(),
            id: owner.chat.clone(),
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

// ---- Fixes (M37 §1.3) ----------------------------------------------------

/// The env a child `ferrule` needs to read this process's config.
pub(super) fn child_env(ctx: &Ctx) -> Vec<(String, std::ffi::OsString)> {
    let mut env = Vec::new();
    if let Some(c) = &ctx.config_path {
        env.push(("FERRULE_CONFIG".to_string(), c.clone().into_os_string()));
    }
    if let Some(d) = &ctx.data {
        env.push(("FERRULE_DATA_DIR".to_string(), d.clone().into_os_string()));
    }
    // The page's own instance: a fix button or a console line acts on it,
    // never on the default (M38).
    if let Some(name) = crate::instance::current() {
        env.push((crate::instance::ENV.to_string(), name.into()));
    }
    env
}

/// `ferrule doctor --json`, as a run the page polls; its report comes
/// back with a fix button on each item that has one.
fn doctor_run(ctx: &Ctx, body: &Value) -> Answer {
    let mut args = vec!["doctor".into(), "--json".into()];
    if body.get("ping_models").and_then(Value::as_bool) == Some(true) {
        args.push("--ping-models".into());
    }
    let label = format!(
        "ferrule {}",
        args.iter()
            .map(|a: &std::ffi::OsString| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    );
    match ctx.runs.start(label, args, child_env(ctx)) {
        Ok(run) => ok(json!({ "ok": true, "id": run.id })),
        Err(e) => bad(500, format!("couldn't start doctor: {e}")),
    }
}

/// `GET /api/run?id=&from=`: a run's output after `from`; a finished
/// doctor also has its report.
fn run_view(ctx: &Ctx, req: &Request) -> Answer {
    let Some(id) = req.query.get("id") else {
        return bad(400, "`id` is missing");
    };
    let Some(run) = ctx.runs.get(id) else {
        return bad(404, "no such run (the page keeps the last 20)");
    };
    let from = req
        .query
        .get("from")
        .and_then(|f| f.parse().ok())
        .unwrap_or(0);
    let mut v = run.view(from);
    if run.done() && run.label.starts_with("ferrule doctor") {
        v["report"] = doctor_report(&run.output()).unwrap_or(Value::Null);
    }
    ok(v)
}

/// The doctor's JSON line, each item with the fixes the page can run.
pub fn doctor_report(out: &str) -> Option<Value> {
    let mut report: Value = out
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(|v| v["items"].is_array())?;
    for item in report["items"].as_array_mut().into_iter().flatten() {
        if !matches!(item["level"].as_str(), Some("warn" | "fail")) {
            continue;
        }
        let what = item["what"].as_str().unwrap_or("");
        let fixes = match what {
            w if crate::trust::is_chat_channel(w) => json!([
                { "label": format!("Restart {what}"), "action": "channels/restart", "body": { "name": what } }
            ]),
            "config" => json!([
                { "label": "Restore the last good config", "action": "config/restore", "body": {} }
            ]),
            "updates" => json!([
                { "label": "Check for an update", "action": "console/run", "body": { "line": "update --check" } }
            ]),
            "models" | "provider" | "keys" | "routing" => {
                json!([{ "label": "Open Models", "section": "models" }])
            }
            "connect" | "mcp" => json!([{ "label": "Open Connections", "section": "connections" }]),
            "hooks" | "plugins" | "browser" | "search" => {
                json!([{ "label": "Open Extensions", "section": "extensions" }])
            }
            "gateway" | "service" => json!([
                { "label": "Restart the gateway", "action": "gateway/restart", "body": {} }
            ]),
            _ => continue,
        };
        item["fixes"] = fixes;
    }
    Some(report)
}

fn channel_restart(ctx: &Ctx, body: &Value) -> Answer {
    let Some(live) = &ctx.live else {
        return missing("the gateway");
    };
    let name = need!(arg(body, "name"));
    match live.restarts.restart(name) {
        Ok(()) => ok(json!({
            "ok": true,
            "said": format!("{name} started again. A new token or setting needs a gateway restart."),
        })),
        Err(e) => bad(400, e),
    }
}

/// Puts the last config that read back in place of the file, keeping the
/// file as `<config>.prev`.
fn config_restore(ctx: &Ctx, body: &Value) -> Answer {
    let (Some(data), Some(file)) = (ctx.data.as_deref(), ctx.config_path.as_deref()) else {
        return missing("the config file");
    };
    let copy = crate::last_good::path(data);
    let Ok(good) = std::fs::read_to_string(&copy) else {
        return bad(
            404,
            "There's no last good config yet: it's kept after each good start.",
        );
    };
    let now = std::fs::read_to_string(file).unwrap_or_default();
    if now == good {
        return ok(json!({ "ok": true, "said": "The config is already the last good one." }));
    }
    need!(confirmed(
        body,
        format!(
            "Replace {} with the last config that read? The current file is kept as {}.prev.",
            file.display(),
            file.display()
        )
    ));
    let prev = crate::dashboard::config_prev(file);
    let done = crate::secrets::write_private(&prev, &now)
        .and_then(|()| crate::secrets::write_private(file, &good));
    match done {
        Ok(()) => ok(json!({
            "ok": true,
            "said": "Restored. The gateway follows MCP servers and secrets now; the rest after a restart.",
            "restart": true,
        })),
        Err(e) => bad(500, format!("{e:#}")),
    }
}

/// Only under a service that starts it again: the process ends cleanly
/// and the service brings it back on the file as it is now.
fn gateway_restart(ctx: &Ctx, body: &Value) -> Answer {
    if ctx.live.is_none() {
        return missing("the gateway");
    }
    if !crate::last_good::supervised() {
        return bad(
            409,
            "The gateway runs in a terminal, not as a service, so nothing would start it again: restart it there.",
        );
    }
    let managed = crate::managed::on();
    need!(confirmed(
        body,
        if managed {
            "Restart the bot? Running turns get a few seconds to finish, then they stop; reload \
             this page in a few seconds."
                .to_string()
        } else {
            "Restart the gateway? Running turns stop. If you're on the tunnel address, this page \
             stops working and a new link comes to your chat within a minute."
                .to_string()
        }
    ));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        tracing::info!("restart asked for from the dashboard");
        if managed {
            crate::lifecycle::request_restart();
        } else {
            terminate_self();
        }
    });
    ok(json!({ "ok": true, "said": "Restarting…" }))
}

/// The same clean shutdown as `systemctl stop`, which the service follows
/// with a start.
fn terminate_self() {
    #[cfg(unix)]
    // SAFETY: signalling our own pid; the gateway's handler shuts down.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGTERM);
    }
    #[cfg(not(unix))]
    std::process::exit(0);
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

pub(super) fn retire(ctx: &Ctx, session: Option<&str>) {
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
            let d = m.set_default(word, &by());
            if d.is_ok() {
                retire(ctx, None);
            }
            d
        }
        "models/pin" | "models/unpin" => {
            let chat = need!(arg(body, "chat"));
            // A chat on Telegram unless the page names another channel.
            let channel = arg(body, "channel").unwrap_or("telegram");
            let d = if path == "models/pin" {
                m.pin(channel, chat, need!(arg(body, "model")), &by())
            } else {
                m.unpin(channel, chat, &by())
            };
            if d.is_ok() {
                retire(
                    ctx,
                    Some(&ferrule_gateway::session::session_id(channel, chat)),
                );
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
            if let Err(e) = super::models_page::check_fallback(&m.catalog(), &words) {
                return bad(400, e);
            }
            m.set_fallback(&words, &by())
        }
        "models/add" => {
            let provider = need!(arg(body, "provider"));
            let model = need!(arg(body, "model"));
            let alias = body
                .get("alias")
                .and_then(Value::as_str)
                .filter(|a| !a.trim().is_empty());
            m.add_model(provider, model, alias, &by())
        }
        _ => {
            let word = need!(arg(body, "model"));
            need!(confirmed(
                body,
                format!("Remove {word}? Pins and fallback entries naming it stop working.")
            ));
            let d = m.remove_model(word, &by());
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

// ---- Routing (M25) ------------------------------------------------------

/// `[routing]`, the escalations a day and their reasons and the spend per
/// tier over the last `days` (1, 7 or 30), and a pair to route over from
/// the connected models' prices.
fn routing(ctx: &Ctx, req: &Request) -> Answer {
    let Some(m) = &ctx.models else {
        return missing("the models");
    };
    let days: i64 = match req.query.get("days").map(String::as_str) {
        Some("1") => 1,
        Some("30") => 30,
        _ => 7,
    };
    let since = Utc::now() - chrono::Duration::days(days);
    let stats = ctx
        .data
        .as_ref()
        .and_then(|d| crate::ledger::read_records(&d.join("ledger.jsonl"), Some(since)).ok())
        .map(|(rows, _)| routing_admin::stats(&rows))
        .unwrap_or_default();
    ok(json!({
        "routing": m.view().routing,
        "days": days,
        "stats": stats,
        "suggestion": routing_admin::suggest(&m.catalog(), &[], None),
    }))
}

fn routing_op(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    let Some(m) = &ctx.models else {
        return missing("the models");
    };
    let done = if path == "routing/unset" {
        m.unset_routing(&by())
    } else {
        let Some(tiers) = body.get("tiers").and_then(Value::as_array) else {
            return bad(400, "`tiers` is missing: the models, cheap first");
        };
        let tiers: Vec<String> = tiers
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let de_escalate = body.get("de_escalate").and_then(Value::as_bool);
        // Absent: unchanged; null or 0: no cap.
        let cap = match body.get("strong_daily_usd") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => match v.as_f64() {
                Some(0.0) => Some(None),
                Some(c) => Some(Some(c)),
                None => return bad(400, "`strong_daily_usd` is dollars a day, or null"),
            },
        };
        m.set_routing(&tiers, de_escalate, cap, &by())
    };
    match done {
        Ok(d) => {
            retire(ctx, None);
            ok(json!({ "ok": true, "said": d.said, "view": d.view }))
        }
        Err(e) => bad(400, format!("{e:#}")),
    }
}

/// `GET /api/managed`: whether this bot is run by a panel, and what the
/// panel's policy locks.
fn managed_view(ctx: &Ctx) -> Value {
    use crate::managed;
    let state = managed::state();
    let Some(policy) = managed::policy() else {
        return json!({ "on": false });
    };
    let protection = match super::api::config(ctx).map(|c| crate::shared_sandbox(&c)) {
        Some(Ok(s)) => managed::protection(&s),
        Some(Err(e)) => format!("unknown: {e}"),
        None => "unknown: no config".into(),
    };
    json!({
        "on": true,
        "source": state.source,
        "bot_id": state.bot_id,
        "reason": policy.why(),
        "policy": policy,
        "locks": policy.locks(),
        "protection": protection,
        "panel_secret": managed::panel_secret().is_some(),
        "claude_plan": managed::NO_CLAUDE_PLAN,
    })
}

pub(super) fn config(ctx: &Ctx) -> Option<Config> {
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
        .add_from_catalog(&provider, id, as_what, listing, &by())
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
    match m.fill_prices(&listings, &by()) {
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
        by: by(),
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
    let calls: Vec<&LedgerRecord> = records.iter().filter(|r| !r.is_bookkeeping()).collect();
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
                "notional_usd": r.notional_usd,
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
        "egress_refused": egress_refused(records),
    });
    if let Some(hub) = hub {
        out["caps"] = spend(hub, &mut Vec::new());
    }
    out
}

/// M33: the proxy's refusals in the window, and the hosts refused most.
fn egress_refused(records: &[LedgerRecord]) -> Value {
    let (count, top) = crate::egress::recent_denials(records, DateTime::<Utc>::default());
    json!({
        "count": count,
        "hosts": top.iter().map(|(h, n)| json!({ "host": h, "count": n })).collect::<Vec<_>>(),
    })
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
        "tasks/pause" => t.pause(id, &by()),
        "tasks/resume" => t.resume(id, &by()),
        "tasks/run" => t.run_now(id, &by()),
        _ => {
            need!(confirmed(
                body,
                format!("Delete task {id} and its run history? This can't be undone.")
            ));
            t.delete(id, &by())
        }
    };
    match done {
        Ok(said) => ok(json!({ "ok": true, "said": said })),
        Err(e) => bad(400, format!("{e:#}")),
    }
}

/// A task's schedule (and zone), or its model (`default`: back to the
/// default).
fn task_edit(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    let Some(t) = &ctx.tasks else {
        return missing("the tasks");
    };
    let id = need!(arg(body, "id"));
    let done = if path == "tasks/schedule" {
        let schedule = need!(arg(body, "schedule"));
        let tz = body.get("timezone").and_then(Value::as_str);
        t.set_schedule(id, schedule, tz, &by())
    } else {
        let word = need!(arg(body, "model"));
        let model = if word == "default" {
            None
        } else {
            let Some(m) = &ctx.models else {
                return missing("the models");
            };
            match m.resolve(word) {
                Ok(_) => Some(word),
                Err(why) => return bad(400, format!("{word}: {why}")),
            }
        };
        t.set_model(id, model, &by())
    };
    match done {
        Ok(said) => ok(json!({ "ok": true, "said": said })),
        Err(e) => bad(400, format!("{e:#}")),
    }
}

fn task_kind(body: &Value) -> Result<ferrule_gateway::TaskKind, Answer> {
    match body.get("kind").and_then(Value::as_str).unwrap_or("cron") {
        "cron" => Ok(ferrule_gateway::TaskKind::Cron),
        "once" => Ok(ferrule_gateway::TaskKind::Once),
        other => Err(bad(
            400,
            format!("`kind` is `{other}`; use `cron` or `once`"),
        )),
    }
}

/// The next few times a schedule fires, by the same parser `ferrule tasks
/// add` uses, so the page can say them before anything is saved.
fn task_preview(body: &Value) -> Answer {
    let kind = need!(task_kind(body));
    let schedule = need!(arg(body, "schedule"));
    let tz = body
        .get("timezone")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|z| !z.is_empty())
        .unwrap_or("UTC");
    match crate::tasks_admin::TasksAdmin::preview(kind, schedule, tz, 3) {
        Ok(next) => ok(json!({ "ok": true, "next": next })),
        Err(e) => bad(400, format!("{e:#}")),
    }
}

/// A task made on the page: what to do, when, and where the answer goes.
/// Never a gate (a shell command); that stays `ferrule tasks add --gate`.
async fn task_add(ctx: &Ctx, body: &Value) -> Answer {
    let Some(t) = &ctx.tasks else {
        return missing("the tasks");
    };
    if body.get("gate").is_some_and(|g| !g.is_null()) {
        return bad(
            400,
            "Tasks from the page can't run a gate command; use `ferrule tasks add --gate` for that.",
        );
    }
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
    };
    if text("name").is_empty() {
        return bad(400, "Give the task a name.");
    }
    if text("prompt").is_empty() {
        return bad(400, "Tell the bot what to do in this task.");
    }
    if text("schedule").is_empty() {
        return bad(400, "Pick when it runs.");
    }
    let kind = need!(task_kind(body));
    let tz = match text("timezone") {
        "" => "UTC",
        z => z,
    };
    let model = match text("model") {
        "" | "default" => None,
        word => {
            let Some(m) = &ctx.models else {
                return missing("the models");
            };
            if let Err(why) = m.resolve(word) {
                return bad(400, format!("The model {word} isn't connected ({why})."));
            }
            Some(word.to_string())
        }
    };
    let (channel, chat_id) = match (text("to"), &ctx.owner_chat) {
        ("chat", _) | ("", None) => (
            super::chat::CHANNEL.to_string(),
            super::chat::CHAT.to_string(),
        ),
        (_, Some(owner)) => (owner.channel.clone(), owner.chat.clone()),
        (_, None) => (
            super::chat::CHANNEL.to_string(),
            super::chat::CHAT.to_string(),
        ),
    };
    let new = crate::tasks_admin::NewPageTask {
        name: text("name").to_string(),
        prompt: text("prompt").to_string(),
        kind,
        schedule: text("schedule").to_string(),
        timezone: tz.to_string(),
        channel,
        chat_id,
        model,
    };
    match t.add(new, &by()) {
        Ok((task, next)) => {
            let when = next
                .and_then(|n| chrono::DateTime::from_timestamp(n, 0))
                .map(|n| match task.timezone.parse::<chrono_tz::Tz>() {
                    Ok(z) => format!(
                        " Next run: {} ({z}).",
                        n.with_timezone(&z).format("%a %-d %b, %H:%M")
                    ),
                    Err(_) => format!(" Next run: {} UTC.", n.format("%a %-d %b, %H:%M")),
                })
                .unwrap_or_default();
            ok(json!({
                "ok": true,
                "id": task.id,
                "said": format!("Added \u{201c}{}\u{201d}.{when}", task.name),
            }))
        }
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

/// The shared settings operations (M24) over this gateway's config,
/// data dir, hub and workspace.
fn settings(ctx: &Ctx) -> Option<crate::settings_admin::Settings> {
    Some(crate::settings_admin::Settings::new(
        ctx.config_path.clone()?,
        ctx.data.clone(),
        ctx.hub.clone(),
        ctx.workspace.clone(),
    ))
}

/// The caps, MCP servers, skills and hooks (M24: each editable). Names,
/// hosts and file names only: never a server's args, env, headers or URL
/// path.
fn settings_view(ctx: &Ctx) -> Answer {
    let Some(s) = settings(ctx) else {
        return missing("the config");
    };
    match s.view() {
        Ok(v) => ok(json!(v)),
        Err(e) => bad(500, format!("{e:#}")),
    }
}

async fn settings_op(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    let Some(s) = settings(ctx) else {
        return missing("the config");
    };
    let done = match path {
        "settings/caps" => {
            let Some(caps) = body.get("caps").and_then(Value::as_object) else {
                return bad(400, "`caps` is missing");
            };
            let mut changes = Vec::new();
            for (key, v) in caps {
                let Some(v) = v.as_f64() else {
                    return bad(400, format!("{key} must be a number"));
                };
                changes.push((key.clone(), v));
            }
            if let Some(why) = crate::settings_admin::caps_refusal(&changes) {
                return bad(403, why);
            }
            match s.caps_question(&changes) {
                Ok(Some(q)) => need!(confirmed(body, q)),
                Ok(None) => {}
                Err(e) => return bad(400, format!("{e:#}")),
            }
            s.set_caps(&changes, &by())
        }
        "mcp/disable" | "mcp/enable" => {
            let name = need!(arg(body, "name"));
            let off = path == "mcp/disable";
            if off {
                need!(confirmed(
                    body,
                    format!("Turn off `{name}`? Running agents lose its tools within seconds.")
                ));
            }
            s.mcp_set_disabled(name, off, &by())
        }
        "mcp/remove" => {
            let name = need!(arg(body, "name"));
            need!(confirmed(
                body,
                format!("Remove `{name}`? Adding it back means `ferrule mcp add` again.")
            ));
            s.mcp_remove(name, &by()).await
        }
        "skills/disable" | "skills/enable" => {
            let name = need!(arg(body, "name"));
            let d = s.skill_set_disabled(name, path == "skills/disable", &by());
            if d.is_ok() {
                // The skill catalog is in the prompt: chats get a new one.
                retire(ctx, None);
            }
            d
        }
        "hooks/trust" => {
            let sha = need!(arg(body, "sha"));
            need!(confirmed(
                body,
                "These hooks run as you, outside the sandbox, whenever an agent works here. \
                 Trust the file with exactly this hash?"
                    .to_string()
            ));
            s.hooks_trust(sha, &by())
        }
        _ => s.hooks_untrust(&by()),
    };
    match done {
        Ok(d) => ok(json!({ "ok": true, "said": d.said, "view": d.view })),
        Err(e) => bad(400, format!("{e:#}")),
    }
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
#[path = "../../../ferrule-connections/tests/it/common/mod.rs"]
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
    async fn the_config_is_edited_from_the_page_checked_guarded_and_undone() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = bare(dir.path());
        let file = dir.path().join("config.toml");
        let first = format!(
            "# mine\n[agent]\nstream = true\nauto_commit_author = \"{SECRET}\"\n\n[[mcp.servers]]\nname = \"fs\"\ncommand = \"npx\"\nargs = [\"server\"]\n"
        );
        std::fs::write(&file, &first).unwrap();
        ctx.config_path = Some(file.clone());
        ctx.hub = Some(hub(dir.path()));

        let (s, v) = call(&ctx, "config", json!({})).await;
        assert_eq!(s, 200);
        let text = v["text"].as_str().unwrap().to_string();
        assert!(!text.contains(SECRET) && text.contains("# mine"), "{text}");
        assert_eq!(v["prev"], false);
        let stream = v["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "agent.stream")
            .unwrap()
            .clone();
        assert_eq!(stream["value"], true);

        // A file that doesn't read is never saved, and the line is named.
        let broken = "[agent]\nstream = \"yes\"\n";
        let (s, v) = call(&ctx, "POST config/check", json!({ "text": broken })).await;
        assert_eq!(
            (s, v["ok"].clone(), v["line"].clone()),
            (200, json!(false), json!(2))
        );
        let (s, _) = call(&ctx, "POST config/save", json!({ "text": broken })).await;
        assert_eq!(s, 422);
        let (s, _) = call(&ctx, "POST config/save", json!({ "text": "[agent" })).await;
        assert_eq!(s, 422);

        // A command-bearing change is refused with the field named.
        let raw = std::fs::read_to_string(&file).unwrap();
        let args = raw.replace("[\"server\"]", "[\"server\", \"--evil\"]");
        let (s, v) = call(&ctx, "POST config/save", json!({ "text": args })).await;
        assert_eq!(s, 403, "{v}");
        assert!(v["field"].as_str().unwrap().contains("args"));
        let (s, v) = call(
            &ctx,
            "POST config/set",
            json!({ "key": "agent.verify_command", "value": "x" }),
        )
        .await;
        assert_eq!(s, 400, "{v}");

        // The form sets one field; the editor saves the rest.
        let (s, v) = call(
            &ctx,
            "POST config/set",
            json!({ "key": "agent.stream", "value": "no" }),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        let (s, v) = call(
            &ctx,
            "POST config/set",
            json!({ "key": "agent.stream", "value": false }),
        )
        .await;
        assert_eq!((s, v["restart"].clone()), (200, json!(true)), "{v}");
        let now = std::fs::read_to_string(&file).unwrap();
        assert!(
            now.contains("stream = false") && now.contains("# mine"),
            "{now}"
        );
        assert_eq!(
            std::fs::read_to_string(crate::dashboard::config_prev(&file)).unwrap(),
            first
        );
        let edited = text.replace("stream = true", "stream = true\nparallel_tools = 2");
        let (s, v) = call(&ctx, "POST config/save", json!({ "text": edited })).await;
        assert_eq!(s, 200, "{v}");
        let now = std::fs::read_to_string(&file).unwrap();
        assert!(
            now.contains("parallel_tools = 2") && now.contains(SECRET),
            "the hidden value came back"
        );

        // Undo puts the file before the last save back.
        let (s, _) = call(&ctx, "POST config/undo", json!({})).await;
        assert_eq!(s, 200);
        let now = std::fs::read_to_string(&file).unwrap();
        assert!(
            now.contains("stream = false") && !now.contains("parallel_tools"),
            "{now}"
        );
        let events = ctx.hub.as_ref().unwrap().audit().read(None).unwrap();
        assert_eq!(
            events.iter().filter(|e| e.event == "config_saved").count(),
            3
        );
    }

    #[tokio::test]
    async fn the_page_chats_in_its_own_session_and_answers_any_waiting_question() {
        use super::super::chat::DashboardChannel;
        use ferrule_gateway::Channel;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = bare(dir.path());
        // Outside the gateway the page says so instead of offering a box
        // that can't work.
        let (s, v) = call(&ctx, "chat?from=0", json!({})).await;
        assert_eq!((s, v["listening"].clone()), (200, json!(false)));
        let (s, _) = call(&ctx, "POST chat/send", json!({ "text": "hi" })).await;
        assert_eq!(s, 503);

        let ch = Arc::new(DashboardChannel::default());
        ctx.chat = Some(ch.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let run = {
            let ch = ch.clone();
            tokio::spawn(async move { ch.run(tx).await })
        };
        while !ch.listening() {
            tokio::task::yield_now().await;
        }
        let (s, _) = call(&ctx, "POST chat/send", json!({ "text": "  " })).await;
        assert_eq!(s, 400);
        let long = "x".repeat(super::super::chat::MAX_TEXT + 1);
        let (s, _) = call(&ctx, "POST chat/send", json!({ "text": long })).await;
        assert_eq!(s, 413);
        let (s, _) = call(&ctx, "POST chat/send", json!({ "text": "מה קורה?" })).await;
        assert_eq!(s, 200);
        let got = rx.recv().await.unwrap();
        assert_eq!(
            ferrule_gateway::session::session_id(&got.channel, &got.chat_id),
            "dashboard__owner"
        );
        assert!(matches!(
            crate::trust::route_for("dashboard__owner"),
            ferrule_trust::Route::Owner { .. }
        ));
        ch.send(ferrule_gateway::OutboundMessage {
            channel: "dashboard".into(),
            chat_id: "owner".into(),
            text: format!("the key is {SECRET}"),
            reply_to: None,
            attachments: vec![],
        })
        .await
        .unwrap();
        let (_, v) = call(&ctx, "chat?from=0", json!({})).await;
        let e = v["entries"].as_array().unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0]["text"], "מה קורה?");
        assert!(
            !v.to_string().contains(SECRET),
            "redacted on the way out: {v}"
        );
        let next = v["next"].as_u64().unwrap();
        let (_, v) = call(&ctx, &format!("chat?from={next}"), json!({})).await;
        assert!(v["entries"].as_array().unwrap().is_empty());

        // A question asked in Telegram is answered from the page.
        let h = hub(dir.path());
        ctx.hub = Some(h.clone());
        let (code, mut answer) = h.approvals().open(
            ferrule_trust::ChatRef::new("telegram", "5"),
            "run `rm -rf build`",
        );
        let (_, v) = call(&ctx, "approvals", json!({})).await;
        assert_eq!(v["approvals"][0]["code"], json!(code));
        assert_eq!(v["approvals"][0]["chat"], "telegram:5");
        let (s, _) = call(&ctx, "POST approvals/answer", json!({ "code": code })).await;
        assert_eq!(s, 400, "allow or refuse, never a guess");
        let (s, v) = call(
            &ctx,
            "POST approvals/answer",
            json!({ "code": code, "allow": false }),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert!(matches!(
            answer.try_recv().unwrap(),
            ferrule_trust::approval::Answer::No(_)
        ));
        let (s, _) = call(
            &ctx,
            "POST approvals/answer",
            json!({ "code": code, "allow": true }),
        )
        .await;
        assert_eq!(s, 404, "answered once");
        let events = h.audit().read(None).unwrap();
        assert!(events
            .iter()
            .any(|e| e.event == "approval_refused" && e.detail["by"] == "dashboard"));
        drop(rx);
        run.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_console_runs_a_parsed_line_confirms_changes_and_audits_each_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = bare(dir.path());
        // `echo` stands in for ferrule: the output is the argv it was given.
        ctx.runs = Arc::new(super::super::runs::Runs::new("/bin/echo".into()));
        let h = hub(dir.path());
        ctx.hub = Some(h.clone());

        let (s, v) = call(
            &ctx,
            "POST console/run",
            json!({ "line": "doctor; rm -rf ~" }),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("not a shell"), "{v}");
        let (s, v) = call(&ctx, "POST console/run", json!({ "line": "sandbox -- sh" })).await;
        assert_eq!(s, 403, "{v}");
        let (s, v) = call(
            &ctx,
            "POST console/run",
            json!({ "line": "doctor --config /etc/x" }),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        let (s, v) = call(
            &ctx,
            "POST console/run",
            json!({ "line": "tasks delete 3" }),
        )
        .await;
        assert_eq!(s, 409, "{v}");
        assert_eq!(v["class"], "destructive");

        let (s, v) = call(
            &ctx,
            "POST console/run",
            json!({ "line": "ferrule model --help" }),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert!(v["job"]["text"].as_str().unwrap().contains("Usage"));
        assert_eq!(v["job"]["code"], 0);

        let (s, v) = call(
            &ctx,
            "POST console/run",
            json!({ "line": "memory search 'a b'" }),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        let id = v["job"]["id"].as_str().unwrap().to_string();
        let mut job = json!({});
        for _ in 0..400 {
            job = call(&ctx, &format!("console/job?id={id}&from=0"), json!({}))
                .await
                .1;
            if job["done"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(job["code"], 0, "{job}");
        assert_eq!(job["text"], "memory search a b\n");

        let (s, _) = call(
            &ctx,
            "POST console/run",
            json!({ "line": "tasks delete 3", "confirm": true }),
        )
        .await;
        assert_eq!(s, 200);
        for _ in 0..400 {
            if !ctx.runs.busy() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let events: Vec<_> = h.audit().read(None).unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(
            kinds.iter().filter(|k| **k == "console_refused").count(),
            3,
            "{kinds:?}"
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == "console_run").count(),
            2,
            "{kinds:?}"
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == "console_done").count(),
            2,
            "{kinds:?}"
        );
        let done = events
            .iter()
            .rev()
            .find(|e| e.event == "console_done")
            .unwrap();
        assert_eq!(done.detail["by"], "dashboard");
        assert_eq!(done.detail["code"], 0);
        assert_eq!(done.detail["class"], "destructive");

        let (_, v) = call(&ctx, "console/complete?line=model%20ro", json!({})).await;
        assert_eq!(v["items"][0]["word"], "route", "{v}");
        let (_, v) = call(&ctx, "console/complete?line=update%20--ch", json!({})).await;
        assert_eq!(v["items"][0]["word"], "--check", "{v}");
        let (_, v) = call(&ctx, "console/parity", json!({})).await;
        assert!(v["rows"].as_array().unwrap().len() > 80);
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
        ctx.owner_chat = Some(42.into());

        let (s, v) = call(
            &ctx,
            "POST connections/connect",
            json!({"service": "keyed"}),
        )
        .await;
        assert_eq!(s, 400);
        assert!(v["error"].as_str().unwrap().contains("takes a key"));

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

    #[tokio::test]
    async fn a_notice_closes_for_a_day_comes_back_on_restore_and_the_kill_switch_never_closes() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = bare(dir.path());
        let data = dir.path().join("data");
        std::fs::write(
            data.join(crate::selfcheck::FILE),
            r#"{"problems":{"disk":"The disk is nearly full: 1 GB left."},"at":1}"#,
        )
        .unwrap();
        let (_, health) = call(&ctx, "health", json!({})).await;
        let p = &health["problems"][0];
        assert_eq!(p["id"], "selfcheck:disk", "{health}");
        assert_eq!(p["closable"], true);
        assert_eq!(p["fixes"][0]["action"], "doctor/run");

        let (s, v) = call(
            &ctx,
            "POST notices/dismiss",
            json!({"id": "selfcheck:disk"}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        let (_, health) = call(&ctx, "health", json!({})).await;
        assert_eq!(health["problems"], json!([]), "{health}");
        assert_eq!(health["hidden"][0]["id"], "selfcheck:disk");
        let (s, _) = call(&ctx, "POST notices/dismiss", json!({"id": "h:gone"})).await;
        assert_eq!(s, 404, "only a notice that's showing closes");

        let (s, v) = call(&ctx, "POST notices/restore", json!({})).await;
        assert_eq!((s, v["restored"].clone()), (200, json!(1)));
        let (_, health) = call(&ctx, "health", json!({})).await;
        assert_eq!(health["problems"][0]["id"], "selfcheck:disk");

        let h = hub(dir.path());
        ctx.hub = Some(h.clone());
        h.engage(BY, None).unwrap();
        let (_, health) = call(&ctx, "health", json!({})).await;
        assert_eq!(health["problems"][0]["id"], "kill");
        assert_eq!(health["problems"][0]["closable"], false);
        let (s, v) = call(&ctx, "POST notices/dismiss", json!({"id": "kill"})).await;
        assert_eq!(s, 409, "{v}");
        assert!(v["error"].as_str().unwrap().contains("can't be hidden"));
    }

    #[test]
    fn the_doctors_report_gets_a_fix_per_item_that_has_one() {
        let out = "some log line\n".to_string()
            + &json!({"version": "x", "ok": false, "warnings": 1, "failures": 1, "items": [
                {"level": "fail", "what": "telegram", "text": "token refused", "hints": []},
                {"level": "warn", "what": "config", "text": "unknown key", "hints": []},
                {"level": "ok", "what": "models", "text": "fine", "hints": []},
                {"level": "warn", "what": "memory", "text": "big", "hints": []},
            ]})
            .to_string();
        let r = doctor_report(&out).unwrap();
        let items = r["items"].as_array().unwrap();
        assert_eq!(items[0]["fixes"][0]["action"], "channels/restart");
        assert_eq!(items[0]["fixes"][0]["body"]["name"], "telegram");
        assert_eq!(items[1]["fixes"][0]["action"], "config/restore");
        assert!(
            items[2].get("fixes").is_none(),
            "an ok line has nothing to fix"
        );
        assert!(items[3].get("fixes").is_none(), "no known repair");
        assert!(doctor_report("not json").is_none());
    }

    #[tokio::test]
    async fn the_last_good_config_goes_back_after_a_confirm_keeping_the_file_as_prev() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = bare(dir.path());
        let file = dir.path().join("config.toml");
        ctx.config_path = Some(file.clone());
        std::fs::write(&file, "broken = [").unwrap();
        let (s, _) = call(&ctx, "POST config/restore", json!({})).await;
        assert_eq!(s, 404, "no copy yet");
        let copy = crate::last_good::path(&dir.path().join("data"));
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::write(&copy, "[gateway]\n").unwrap();
        let (s, v) = call(&ctx, "POST config/restore", json!({})).await;
        assert_eq!(s, 409, "{v}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "broken = [");
        let (s, v) = call(&ctx, "POST config/restore", json!({"confirm": true})).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "[gateway]\n");
        let prev = super::super::config_prev(&file);
        assert_eq!(std::fs::read_to_string(prev).unwrap(), "broken = [");
        let (_, v) = call(&ctx, "POST config/restore", json!({})).await;
        assert!(v["said"].as_str().unwrap().contains("already"));
    }

    #[tokio::test]
    async fn restarts_need_the_gateway_and_a_service_behind_it() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = bare(dir.path());
        let (s, _) = call(&ctx, "POST channels/restart", json!({"name": "telegram"})).await;
        assert_eq!(s, 503);
        let (s, _) = call(&ctx, "POST gateway/restart", json!({"confirm": true})).await;
        assert_eq!(s, 503);
        let (s, _) = call(&ctx, "run?id=nope", json!({})).await;
        assert_eq!(s, 404);
    }

    /// Connections that read their secrets from `file`, as the process
    /// does from the secrets file.
    fn conns_reading(
        dir: &Path,
        file: std::path::PathBuf,
        relay: Option<String>,
    ) -> Arc<Connections> {
        Connections::new(
            &dir.join("private"),
            ConnectionsConfig {
                relay_url: relay,
                cloudflared: Some("off".into()),
                ..Default::default()
            },
            Arc::new(move |name: &str| {
                crate::secrets::read(&file)
                    .ok()?
                    .into_iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v)
            }),
            Arc::new(Recorder::default()),
            None,
        )
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_page_sets_up_a_relay_and_a_google_client_and_never_gets_a_secret_back() {
        const KEY: &str = "relay-key-for-tests-0123456789";
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data/private/secrets.env");
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "# mine\n[connections]\ngate_writes = true\n").unwrap();
        let conns = conns_reading(dir.path(), file.clone(), None);
        let mut ctx = bare(dir.path());
        ctx.connections = Some(conns.clone());
        ctx.config_path = Some(config.clone());

        // Nothing set up: the checklist says so, each with its next step.
        let (_, v) = call(&ctx, "connections/checklist", json!({})).await;
        assert_eq!(v["checks"][0]["id"], "relay", "{v}");
        assert_eq!(v["checks"][0]["state"], "missing");
        assert_eq!(v["checks"][0]["action"]["action"], "relay_setup");
        // …and Atlassian's sign-in isn't offered as a button bound to fail.
        let (s, v) = call(
            &ctx,
            "POST connections/connect",
            json!({"service": "atlassian"}),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        assert!(!v.to_string().contains("/connect atlassian"), "{v}");
        let (_, v) = call(&ctx, "connections", json!({})).await;
        let tile = v["tiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["tile"] == "atlassian")
            .expect("the Atlassian tile")
            .clone();
        let names: Vec<&str> = tile["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["jira", "atlassian_token", "atlassian"], "{tile}");
        assert!(tile["options"][2]["blocked"].is_string());
        assert!(tile["options"][0]["blocked"].is_null());
        assert_eq!(tile["options"][0]["fields"][0]["name"], "site", "{tile}");

        // A wrong relay key: checked first, nothing saved.
        let relay = MockRelay::start(KEY).await;
        let (s, v) = call(
            &ctx,
            "POST connections/relay/use",
            json!({"url": relay.url, "key": "not-the-key"}),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        assert!(
            v["error"].as_str().unwrap().contains("nothing was saved"),
            "{v}"
        );
        assert!(
            !file.exists()
                || !std::fs::read_to_string(&file)
                    .unwrap()
                    .contains("not-the-key")
        );
        assert!(conns.relay_url().is_none());

        let (s, v) = call(
            &ctx,
            "POST connections/relay/use",
            json!({"url": format!("{}/cb", relay.url), "key": KEY}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["callback"], format!("{}/cb", relay.url));
        assert!(!v.to_string().contains(KEY));
        assert_eq!(conns.relay_url().as_deref(), Some(relay.url.as_str()));
        let written = std::fs::read_to_string(&config).unwrap();
        assert!(
            written.starts_with("# mine\n") && written.contains(&relay.url),
            "{written}"
        );
        let (_, v) = call(&ctx, "POST connections/relay/check", json!({})).await;
        assert_eq!(v["ok"], true, "{v}");
        let (_, v) = call(&ctx, "connections", json!({})).await;
        assert_eq!(v["relay_live"], true, "{v}");
        assert_eq!(v["callback"], format!("{}/cb", relay.url));

        // Google's client: checked for shape, stored, never echoed.
        let (s, v) = call(
            &ctx,
            "POST connections/google-client",
            json!({"id": "123", "secret": "shh"}),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        let (s, v) = call(
            &ctx,
            "POST connections/google-client",
            json!({"id": "123-abc.apps.googleusercontent.com", "secret": "GOCSPX-test-only"}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert!(
            v["said"]
                .as_str()
                .unwrap()
                .contains(&format!("{}/cb", relay.url)),
            "{v}"
        );
        assert!(!v.to_string().contains("GOCSPX"));
        let (_, v) = call(&ctx, "connections/checklist", json!({})).await;
        let google = v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "google_client")
            .unwrap()
            .clone();
        assert_eq!(google["state"], "ready", "{v}");
        let (_, v) = call(&ctx, "connections", json!({})).await;
        assert!(!v.to_string().contains("GOCSPX"));
        assert!(!v.to_string().contains(KEY));
    }

    #[tokio::test]
    async fn a_relay_deploy_asks_which_account_and_a_refused_token_says_what_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data/private/secrets.env");
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let api = mock::serve(Arc::new(|r: mock::Req| {
            let good = r.headers.get("authorization").map(String::as_str) == Some("Bearer cf-good");
            match (good, r.path.as_str()) {
                (false, _) => mock::Resp::json(
                    401,
                    json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}]}),
                ),
                (true, "/client/v4/accounts") => mock::Resp::json(
                    200,
                    json!({"success": true, "result": [
                        {"id": "a1", "name": "Max"}, {"id": "a2", "name": "Work"}
                    ]}),
                ),
                _ => mock::Resp::status(404),
            }
        }))
        .await;
        let mut ctx = bare(dir.path());
        ctx.connections = Some(conns_reading(dir.path(), file.clone(), None));
        ctx.config_path = Some(config);
        ctx.cf_api = format!("{api}/client/v4");

        let (s, v) = call(
            &ctx,
            "POST connections/relay/deploy",
            json!({"token": "cf-bad"}),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        let why = v["error"].as_str().unwrap();
        assert!(
            why.contains("Edit Cloudflare Workers") && !why.contains("cf-bad"),
            "{v}"
        );
        assert!(!file.exists(), "a refused token isn't kept");

        let (s, v) = call(
            &ctx,
            "POST connections/relay/deploy",
            json!({"token": "cf-good"}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["choose"][1]["name"], "Work", "{v}");
        assert!(!v.to_string().contains("cf-good"));
    }

    #[tokio::test]
    async fn a_key_form_is_checked_before_anything_is_saved_and_its_values_never_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data/private/secrets.env");
        let mut ctx = bare(dir.path());
        let conns = conns_reading(dir.path(), file, None);
        ctx.connections = Some(conns.clone());
        let (s, v) = call(
            &ctx,
            "POST connections/key",
            json!({"service": "jira", "fields": {"site": "acme.atlassian.net", "token": "ATATT-page-secret"}}),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("required"), "{v}");
        assert!(!v.to_string().contains("ATATT-page-secret"));
        let (s, v) = call(
            &ctx,
            "POST connections/key",
            json!({"service": "jira", "fields": {"site": "not a site", "email": "max@example.com", "token": "ATATT-page-secret"}}),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        assert!(!v.to_string().contains("ATATT-page-secret"));
        assert!(conns.snapshot().unwrap().connections.is_empty());
        let (s, _) = call(&ctx, "POST connections/cancel", json!({"id": "nope"})).await;
        assert_eq!(s, 404);
        let (_, v) = call(&ctx, "POST connections/test", json!({"name": "nope"})).await;
        assert_eq!(v["ok"], false, "{v}");
    }

    /// A models page over a config with one provider, `a`, whose model
    /// list is the mock at `base` (a key `good-key-for-tests-01` works).
    fn models_ctx(dir: &Path, base: &str, extra: &str) -> Ctx {
        let mut ctx = bare(dir);
        let config = dir.join("ferrule.toml");
        std::fs::write(
            &config,
            format!(
                "default_provider = \"a\"\n\n[providers.a]\nbase_url = \"{base}/v1\"\n\
                 api_key_env = \"M37_TEST_A_KEY\"\nmodel = \"a-one\"\n\
                 [providers.a.models.a-two]\n[providers.a.models.a-three]\n{extra}"
            ),
        )
        .unwrap();
        let cfg: Config = toml::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        ctx.models = Some(Arc::new(crate::models::Models::new(
            config.clone(),
            Some(dir.join("data/pins.json")),
            &cfg,
        )));
        ctx.config_path = Some(config);
        ctx
    }

    async fn model_list_mock() -> String {
        mock::serve(Arc::new(|r: mock::Req| {
            let auth = r.headers.get("authorization").cloned().unwrap_or_default();
            match r.path.as_str() {
                "/v1/models" if auth == "Bearer good-key-for-tests-01" => mock::Resp::json(
                    200,
                    json!({"data": [{"id": "a-one"}, {"id": "a-two"}, {"id": "a-four"}]}),
                ),
                "/v1/models" => mock::Resp::json(401, json!({"error": "bad key"})),
                _ => mock::Resp::status(404),
            }
        }))
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_fallback_list_refuses_an_unknown_model_a_repeat_and_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = models_ctx(dir.path(), "http://127.0.0.1:9", "");
        let (s, v) = call(&ctx, "models/choices", json!({})).await;
        assert_eq!(s, 200, "{v}");
        let refs: Vec<&str> = v["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["reference"].as_str().unwrap())
            .collect();
        assert_eq!(refs.len(), 3, "{v}");
        assert!(refs.contains(&"a/a-two"), "{v}");
        assert_eq!(v["default"], "a/a-one");
        let names: Vec<&str> = v["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"openai") && names.contains(&"chatgpt"),
            "{v}"
        );

        for (list, why) in [
            (json!(["a/nope"]), "can't be a fallback"),
            (json!(["a/a-two", "a/a-two"]), "twice"),
            (json!(["a/a-one"]), "is the default"),
        ] {
            let (s, v) = call(&ctx, "POST models/fallback", json!({ "models": list })).await;
            assert_eq!(s, 400, "{v}");
            assert!(v["error"].as_str().unwrap().contains(why), "{v}");
        }
        let text = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
        assert!(!text.contains("fallback"), "nothing written: {text}");
        let (s, v) = call(
            &ctx,
            "POST models/fallback",
            json!({ "models": ["a/a-three", "a/a-two"] }),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["view"]["fallback"], json!(["a/a-three", "a/a-two"]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_provider_key_is_tested_before_it_is_saved_and_never_comes_back() {
        const GOOD: &str = "good-key-for-tests-01";
        let dir = tempfile::tempdir().unwrap();
        let base = model_list_mock().await;
        let mut ctx = models_ctx(dir.path(), &base, "");
        ctx.redactor = Arc::new(Redactor::new([SECRET.to_string()]));
        let secrets = dir.path().join("data/private/secrets.env");

        let (s, v) = call(
            &ctx,
            "POST models/provider",
            json!({"provider": "a", "key": "wrong-key-for-tests"}),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        assert!(
            v["error"].as_str().unwrap().contains("nothing was saved"),
            "{v}"
        );
        assert!(!v.to_string().contains("wrong-key"), "{v}");
        assert!(!secrets.exists(), "a refused key isn't written");

        let (s, v) = call(
            &ctx,
            "POST models/provider",
            json!({"provider": "a", "key": "two words"}),
        )
        .await;
        assert_eq!(s, 400, "{v}");

        let (s, v) = call(
            &ctx,
            "POST models/provider",
            json!({"provider": "a", "key": GOOD}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert!(!v.to_string().contains(GOOD), "{v}");
        assert_eq!(v["models"], json!(["a-four", "a-one", "a-two"]));
        let file = std::fs::read_to_string(&secrets).unwrap();
        assert!(file.contains(&format!("M37_TEST_A_KEY={GOOD}")), "{file}");
        let config = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
        assert!(!config.contains(GOOD), "the key never goes in the config");
        // The running process uses it at once: the model is ready.
        let a_one = v["view"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["reference"] == "a/a-one")
            .cloned()
            .unwrap();
        assert_eq!(a_one["key_present"], true, "{a_one}");

        // The list, with the key saved, and what's connected already.
        let (s, v) = call(&ctx, "models/provider/list?provider=a", json!({})).await;
        assert_eq!(s, 200, "{v}");
        assert!(v["models"].as_array().unwrap().contains(&json!("a-four")));
        assert!(v["connected"]
            .as_array()
            .unwrap()
            .contains(&json!("a-three")));

        // A new OpenAI-compatible server: its address and the key's name
        // go in the config, the key in the secrets file.
        let (s, v) = call(
            &ctx,
            "POST models/provider",
            json!({"provider": "lan", "key": GOOD, "base_url": format!("{base}/v1")}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["added"], "lan/a-four", "the first listed: {v}");
        let config = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
        assert!(config.contains("[providers.lan]"), "{config}");
        assert!(config.contains("LAN_API_KEY"), "{config}");
        assert!(!config.contains(GOOD), "{config}");

        // A plan takes no key; an unknown name without an address is said.
        let (s, v) = call(
            &ctx,
            "POST models/provider",
            json!({"provider": "chatgpt", "key": GOOD}),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("Sign in"), "{v}");
        let (s, v) = call(
            &ctx,
            "POST models/provider",
            json!({"provider": "zz", "key": GOOD}),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        assert!(
            v["error"].as_str().unwrap().contains("give its address"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_chatgpt_plan_signs_in_from_the_page_and_a_claude_token_is_checked() {
        use ferrule_plans::mock as plans;
        let dir = tempfile::tempdir().unwrap();
        let state = plans::Shared::default();
        state.lock().unwrap().device_pending = 1;
        let issuer = plans::serve(state.clone()).await;
        let ctx = models_ctx(
            dir.path(),
            "http://127.0.0.1:9",
            &format!("\n[plans.chatgpt]\nissuer = \"{issuer}\"\n"),
        );
        let (_, v) = call(&ctx, "plans/chatgpt/poll", json!({})).await;
        assert_eq!(v["state"], "none");
        let (s, v) = call(&ctx, "POST plans/chatgpt/start", json!({})).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["user_code"], "ABCD-1234");
        assert_eq!(v["page"], format!("{issuer}/codex/device"));
        let mut v = json!({});
        for _ in 0..100 {
            v = call(&ctx, "plans/chatgpt/poll", json!({})).await.1;
            if v["state"] != "waiting" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(v["state"], "done", "{v}");
        assert!(v["said"].as_str().unwrap().contains("ChatGPT plan"), "{v}");
        let config = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
        assert!(config.contains("plan = \"chatgpt\""), "{config}");

        let (s, v) = call(
            &ctx,
            "POST plans/claude",
            json!({"token": "sk-ant-api03-NOTAKEY"}),
        )
        .await;
        assert_eq!(s, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("API key"), "{v}");
        assert!(!v.to_string().contains("NOTAKEY"), "{v}");
        let token = "sk-ant-oat01-NOTAKEY-for-tests";
        let (s, v) = call(&ctx, "POST plans/claude", json!({ "token": token })).await;
        assert_eq!(s, 200, "{v}");
        assert!(!v.to_string().contains(token), "{v}");
        for f in std::fs::read_dir(dir.path().join("data/private")).unwrap() {
            let text = std::fs::read_to_string(f.unwrap().path()).unwrap_or_default();
            assert!(!text.contains(token), "sealed on disk");
        }
        let config = std::fs::read_to_string(dir.path().join("ferrule.toml")).unwrap();
        assert!(config.contains("plan = \"claude-code\""), "{config}");
    }
}
