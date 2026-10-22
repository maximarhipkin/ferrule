//! M64.5: the Proactive page (docs/m64-proactive.md §8): the pass's
//! settings and where it stands, the owner's commitments, what it noticed,
//! suggested and did with each outcome, and the Never list.
//!
//! Its settings are written the way the Config page writes a field: the
//! `base_hash` of the file the page loaded (a stale page gets a 409), the
//! real loader's check, the guard, the file kept as a version (M52.4),
//! audited. `/proactive on|off|level` comes here too, through `api::act`;
//! no chat op maps to these routes, so the chat model can't change them.
//! The pass's task is made to match at once: nothing needs a restart.

use super::api::{arg, bad, by, confirmed, missing, need, ok, Answer};
use super::config_page::{commit, fresh, hash_of, place, read_doc};
use super::Ctx;
use crate::config::Config;
use crate::proactive::status::{level_name, level_words};
use crate::proactive::store::{self, Commitment, Item, Store};
use crate::proactive::{door, due_words, feedback, heartbeat, ProactiveConfig};
use chrono_tz::Tz;
use serde_json::{json, Value};
use toml_edit::Item as TomlItem;

/// How far back the page lists what the pass noticed, said and did.
const ITEMS_SECS: i64 = 30 * 86_400;
const ITEMS_SHOWN: usize = 60;
const DONE_SHOWN: usize = 20;

/// The config as the file has it now, checked.
fn loaded(ctx: &Ctx) -> Result<Config, Answer> {
    let path = place(ctx)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(bad(500, format!("couldn't read {}: {e}", path.display()))),
    };
    let cfg: Config =
        toml::from_str(&text).map_err(|e| bad(422, format!("the config doesn't read: {e}")))?;
    cfg.finish().map_err(|e| bad(422, format!("{e:#}")))
}

fn zone(cfg: &Config) -> Result<Tz, Answer> {
    cfg.proactive
        .tz(&cfg.trust.timezone)
        .map_err(|e| bad(422, format!("{e:#}")))
}

fn open_store(ctx: &Ctx) -> Result<Store, Answer> {
    let data = ctx
        .data
        .as_ref()
        .ok_or_else(|| missing("the data directory"))?;
    Store::open(data).map_err(|e| bad(500, format!("the proactive store: {e:#}")))
}

fn commitment_json(c: &Commitment, tz: Tz) -> Value {
    json!({
        "id": c.id,
        "kind": c.kind,
        "text": c.text,
        "due": c.due,
        "due_words": due_words(c, tz),
        "status": c.status,
        "snoozed_until": c.snoozed_until,
        "reminder": c.reminder.is_some(),
        "updated": c.updated,
    })
}

fn item_json(i: &Item) -> Value {
    json!({
        "id": i.id,
        "at": i.at,
        "kind": i.kind,
        "words": door::kind_words(&i.kind),
        "mode": i.mode,
        "text": i.text,
        "basis": i.basis,
        "outcome": i.outcome,
        "outcome_at": i.outcome_at,
        "thumb": i.thumb,
        "undo": i.undo.is_some(),
    })
}

/// `GET /api/proactive`.
pub fn view(ctx: &Ctx) -> Answer {
    let cfg = need!(loaded(ctx));
    let tz = need!(zone(&cfg));
    let store = need!(open_store(ctx));
    let status = match crate::proactive::status::read(&cfg, tz, &store) {
        Ok(s) => s,
        Err(e) => return bad(500, format!("{e:#}")),
    };
    let now = store::now();
    let all = match store.commitments(true) {
        Ok(a) => a,
        Err(e) => return bad(500, format!("the proactive store: {e:#}")),
    };
    let mut open: Vec<&Commitment> = all.iter().filter(|c| c.is_open(now)).collect();
    open.sort_by_key(|c| (c.due.unwrap_or(i64::MAX), c.id));
    let snoozed: Vec<&Commitment> = all
        .iter()
        .filter(|c| c.status == "snoozed" && !c.is_open(now))
        .collect();
    let done: Vec<&Commitment> = all
        .iter()
        .filter(|c| c.status == "done" || c.status == "dropped")
        .take(DONE_SHOWN)
        .collect();
    let items: Vec<Value> = match store.items_since(now - ITEMS_SECS) {
        Ok(list) => list.iter().rev().take(ITEMS_SHOWN).map(item_json).collect(),
        Err(e) => return bad(500, format!("the proactive store: {e:#}")),
    };
    let path = need!(place(ctx));
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let list =
        |cs: &[&Commitment]| -> Vec<Value> { cs.iter().map(|c| commitment_json(c, tz)).collect() };
    ok(json!({
        "status": status,
        // Sent back with every settings write, so a stale page is refused.
        "hash": hash_of(&text),
        // In the gateway: this process keeps the owner's messages and
        // reads the reactions.
        "live": ctx.live.is_some(),
        "reactions": ctx.live.is_some().then(feedback::read),
        "commitments": { "open": list(&open), "snoozed": list(&snoozed), "done": list(&done) },
        "items": items,
    }))
}

/// What a field takes, in words.
fn takes(kind: &str) -> &'static str {
    match kind {
        "bool" => "true or false",
        "level" => "off, suggest or act",
        "hours" => "a whole number of hours from 1 to 24",
        "count" => "a whole number from 0 to 50",
        "minutes" => "a whole number of minutes from 0 to 1440",
        "usd" => "dollars from 0 (no cap of its own) to 100",
        _ => "a range like 22:00-07:00, or \"\" for none",
    }
}

/// The fields the page and `/proactive` set, and their kinds.
const FIELDS: &[(&str, &str)] = &[
    ("enabled", "bool"),
    ("level", "level"),
    ("every_hours", "hours"),
    ("quiet_hours", "quiet"),
    ("max_messages_per_day", "count"),
    ("min_gap_minutes", "minutes"),
    ("max_usd_per_day", "usd"),
];

/// What changed between two `[proactive]`s, as `key from → to`.
fn changes(a: &ProactiveConfig, b: &ProactiveConfig) -> Vec<String> {
    let rows = [
        ("enabled", a.enabled.to_string(), b.enabled.to_string()),
        (
            "level",
            level_name(a.level).into(),
            level_name(b.level).into(),
        ),
        (
            "every_hours",
            a.every_hours.to_string(),
            b.every_hours.to_string(),
        ),
        (
            "quiet_hours",
            format!("\"{}\"", a.quiet_hours),
            format!("\"{}\"", b.quiet_hours),
        ),
        (
            "max_messages_per_day",
            a.max_messages_per_day.to_string(),
            b.max_messages_per_day.to_string(),
        ),
        (
            "min_gap_minutes",
            a.min_gap_minutes.to_string(),
            b.min_gap_minutes.to_string(),
        ),
        (
            "max_usd_per_day",
            format!("${:.2}", a.max_usd_per_day),
            format!("${:.2}", b.max_usd_per_day),
        ),
    ];
    rows.into_iter()
        .filter(|(_, x, y)| x != y)
        .map(|(k, x, y)| format!("{k} {x} → {y}"))
        .collect()
}

/// `POST /api/proactive/settings {enabled?, level?, every_hours?,
/// quiet_hours?, max_messages_per_day?, min_gap_minutes?,
/// max_usd_per_day?, base_hash?, confirm?}`.
pub fn settings(ctx: &Ctx, body: &Value) -> Answer {
    let path = need!(place(ctx));
    need!(fresh(&path, body));
    let (_, old) = need!(read_doc(&path));
    let Some(old) = old else {
        return bad(
            422,
            "the config file doesn't read as TOML; fix it on the Config page or restore the last good copy",
        );
    };
    let before = need!(loaded(ctx)).proactive;
    let mut set: Vec<(&str, toml_edit::Value)> = Vec::new();
    for (key, kind) in FIELDS {
        let Some(v) = body.get(*key) else { continue };
        let value: Option<toml_edit::Value> = match *kind {
            "bool" => v.as_bool().map(Into::into),
            "level" => v
                .as_str()
                .filter(|s| matches!(*s, "off" | "suggest" | "act"))
                .map(Into::into),
            "hours" => v.as_i64().filter(|h| (1..=24).contains(h)).map(Into::into),
            "count" => v.as_i64().filter(|c| (0..=50).contains(c)).map(Into::into),
            "minutes" => v
                .as_i64()
                .filter(|m| (0..=1440).contains(m))
                .map(Into::into),
            "usd" => v
                .as_f64()
                .filter(|u| u.is_finite() && (0.0..=100.0).contains(u))
                .map(Into::into),
            _ => v
                .as_str()
                .map(str::trim)
                .filter(|s| crate::proactive::quiet(s).is_ok())
                .map(Into::into),
        };
        match value {
            Some(value) => set.push((key, value)),
            None => return bad(400, format!("`{key}` takes {}", takes(kind))),
        }
    }
    if set.is_empty() {
        return bad(
            400,
            "nothing to change: send enabled, level, every_hours, quiet_hours, max_messages_per_day, min_gap_minutes or max_usd_per_day",
        );
    }
    let to_act = body.get("level").and_then(Value::as_str) == Some("act")
        && level_name(before.level) != "act";
    if to_act {
        need!(confirmed(
            body,
            "At act, reminders for what you said and your own tasks from a pattern are made without asking, each with a receipt and Undo; anything built on calendar or mail text is still only asked. Switch to act?".into()
        ));
    }
    let mut new = old.clone();
    let Some(table) = new
        .entry("proactive")
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
    else {
        return bad(422, "`[proactive]` isn't a table");
    };
    for (key, value) in set {
        table.insert(key, TomlItem::Value(value));
    }
    let wrote = new.to_string() != old.to_string();
    let mut answer = if wrote {
        let answer = commit(ctx, &path, Some(&old), &new, "proactive", true);
        match answer {
            Some((200, v)) => v,
            other => return other,
        }
    } else {
        json!({ "ok": true, "sections": [] })
    };
    // The file is as asked: the pass's task and this process follow it.
    let cfg = need!(loaded(ctx));
    let p = &cfg.proactive;
    let pass = match heartbeat::apply(&cfg) {
        Ok(words) => words,
        Err(e) => format!("the pass's task wasn't updated: {e:#}"),
    };
    let changed = changes(&before, p);
    if !changed.is_empty() {
        if let Some(hub) = &ctx.hub {
            crate::proactive::audit(hub, "settings", json!({ "changes": changed, "by": by() }));
        }
    }
    let state = if p.enabled {
        format!(
            "Proactive is on, at level {}: {}",
            level_name(p.level),
            level_words(p.level)
        )
    } else {
        "Proactive is off: no pass runs and your messages aren't kept for it; reminders you accepted still come.".into()
    };
    let what = if changed.is_empty() {
        "Nothing changed.".to_string()
    } else {
        format!("Changed: {}.", changed.join(", "))
    };
    let apart = if p.enabled && ctx.live.is_none() {
        " This page runs apart from the bot: the bot keeps your messages for it from its next start."
    } else {
        ""
    };
    answer["said"] = json!(format!("{state} {what} Now {pass}.{apart}"));
    // Followed at once: no restart.
    answer["restart"] = json!(false);
    ok(answer)
}

/// `POST /api/proactive/clear {kind?, confirm?}`: one kind off the Never
/// list, or (no kind) all of them, after a yes.
pub fn clear(ctx: &Ctx, body: &Value) -> Answer {
    let store = need!(open_store(ctx));
    let kind = body
        .get("kind")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty());
    if kind.is_none() {
        need!(confirmed(
            body,
            "Clear the Never list? Everything on it can come again.".into()
        ));
    }
    match door::clear_never(ctx.hub.as_deref(), &store, kind, &by()) {
        Ok(said) => ok(json!({ "ok": true, "said": said })),
        Err(e) => bad(500, format!("the proactive store: {e:#}")),
    }
}

/// `POST /api/proactive/commitment {id, status: done|dropped|snoozed,
/// days?}`: a commitment closed, dropped or snoozed (a day by default).
pub fn commitment(ctx: &Ctx, body: &Value) -> Answer {
    let Some(id) = body.get("id").and_then(Value::as_i64) else {
        return bad(400, "`id` is a commitment's number");
    };
    let status = need!(arg(body, "status"));
    let now = store::now();
    let (until, said) = match status {
        "done" => (None, "is done".to_string()),
        "dropped" => (None, "is dropped".to_string()),
        "snoozed" => {
            let days = body
                .get("days")
                .and_then(Value::as_i64)
                .filter(|d| (1..=365).contains(d))
                .unwrap_or(1);
            (
                Some(now + days * 86_400),
                format!("is snoozed for {days} day(s)"),
            )
        }
        other => {
            return bad(
                400,
                format!("`status` is done, dropped or snoozed, not `{other}`"),
            )
        }
    };
    let store = need!(open_store(ctx));
    match store.set_status(id, status, until) {
        Ok(true) => {
            if let Some(hub) = &ctx.hub {
                crate::proactive::audit(
                    hub,
                    "commitment",
                    json!({ "id": id, "status": status, "by": by() }),
                );
            }
            ok(json!({ "ok": true, "said": format!("#{id} {said}.") }))
        }
        Ok(false) => bad(404, format!("there's no commitment #{id}")),
        Err(e) => bad(500, format!("the proactive store: {e:#}")),
    }
}
