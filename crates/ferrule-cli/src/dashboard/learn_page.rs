//! The Learning page (`docs/m16-learning-loop.md`, "The learning page"):
//! whether the nightly pass runs and when, the last passes and every
//! change each made (kept, rejected, proposed or reverted), the lessons
//! and learned skills with what they were used in, the owner's feedback,
//! the recent failure notes, and the pass's caps. Its operations: the two
//! switches (written as the Config page writes a field: `base_hash`, the
//! loader's check, the guard, a kept version, audited), a pass now or a
//! dry run, a change's Undo, and Keep or Drop of a proposed one. No chat
//! op maps to these routes. Every text that may hold chat text goes
//! through the redactor; the numbers are signals, not proof.

use super::api::{arg, bad, by, confirmed, need, ok, Answer};
use super::config_page::{commit, fresh, hash_of, place, read_doc};
use super::Ctx;
use crate::config::Config;
use ferrule_learn::files::{Change, PassRecord, Proposal, DROPPED, KEPT, PROPOSED, REVERTED};
use ferrule_learn::signals;
use ferrule_learn::usage::{LessonUses, SkillUses};
use ferrule_learn::LearnDir;
use ferrule_memory::{MemoryStore, FAILURE_NOTE};
use serde_json::{json, Value};
use std::path::PathBuf;
use toml_edit::Item as TomlItem;

/// Passes the page lists, newest first.
const PASSES_SHOWN: usize = 10;
/// Failure notes it lists, newest first.
const NOTES_SHOWN: usize = 10;
const WEEK: i64 = 7 * 86_400;

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

fn data(ctx: &Ctx) -> Result<PathBuf, Answer> {
    ctx.data
        .clone()
        .ok_or_else(|| super::api::missing("the data directory"))
}

fn rfc(at: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(at, 0).map(|d| d.to_rfc3339())
}

/// `0 3 * * *` in `UTC` as "every night at 03:00 (UTC)".
fn schedule_words(cron: &str, tz: &str) -> String {
    let f: Vec<&str> = cron.split_whitespace().collect();
    let clock = match f.as_slice() {
        [m, h, ..] => match (m.parse::<u32>(), h.parse::<u32>()) {
            (Ok(m), Ok(h)) if m < 60 && h < 24 => Some((h, format!("{h:02}:{m:02}"))),
            _ => None,
        },
        _ => None,
    };
    match (clock, f.get(2..5)) {
        (Some((h, at)), Some(["*", "*", "*"])) => {
            let when = if !(6..22).contains(&h) {
                "every night"
            } else {
                "every day"
            };
            format!("{when} at {at} ({tz})")
        }
        (Some((_, at)), Some(["*", "*", "1-5"])) => format!("on weekdays at {at} ({tz})"),
        (Some((_, at)), Some(["*", "*", d])) if d.len() == 1 => {
            let day = [
                "Sunday",
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
                "Sunday",
            ]
            .get(d.parse::<usize>().unwrap_or(9));
            match day {
                Some(day) => format!("every {day} at {at} ({tz})"),
                None => format!("on the schedule `{cron}` ({tz})"),
            }
        }
        _ => format!("on the schedule `{cron}` ({tz})"),
    }
}

fn lesson_kind(op: &str) -> &'static str {
    match op {
        "add" => "lesson_add",
        "retire" => "lesson_retire",
        _ => "lesson_edit",
    }
}

/// Every change of `p` the page shows: what it kept (lessons and skills;
/// memory merges are on the Memory page), what it proposed, what it
/// rejected.
fn changes_of(ctx: &Ctx, p: &PassRecord) -> Vec<Value> {
    let r = |s: &str| ctx.redactor.redact(s);
    let mut out = Vec::new();
    for (i, c) in p.changes.iter().enumerate() {
        let n = i + 1;
        let outcome = if p.status == REVERTED || p.is_undone(n) {
            "reverted"
        } else {
            "kept"
        };
        let (kind, scope, summary, why) = match c {
            Change::Memory { .. } => continue,
            Change::Playbook {
                applied,
                reason,
                gate,
                ..
            } => {
                let text = applied
                    .new
                    .as_deref()
                    .or(applied.old.as_deref())
                    .unwrap_or_default();
                let why = if reason.starts_with("made room for ") {
                    reason.clone()
                } else {
                    format!("{reason} — {gate}")
                };
                (
                    lesson_kind(&applied.op),
                    applied.scope.clone(),
                    format!("pb-{}: {text}", applied.id),
                    why,
                )
            }
            Change::Skill {
                name,
                description,
                scope,
                reason,
                gate,
                op,
                ..
            } => (
                "skill",
                scope.clone(),
                format!("{op} `{name}`: {description}"),
                format!("{reason} — {gate}"),
            ),
        };
        out.push(json!({
            "id": format!("{}:{n}", p.id), "kind": kind, "scope": scope,
            "summary": r(&summary), "outcome": outcome, "why": r(&why),
        }));
    }
    for x in p.proposed.iter().filter(|x| x.status != KEPT) {
        let (kind, summary) = match &x.change {
            Proposal::Lesson {
                op, target, text, ..
            } => (
                lesson_kind(op),
                match (target, text) {
                    (Some(t), Some(text)) => format!("pb-{t}: {text}"),
                    (Some(t), None) => format!("{op} pb-{t}"),
                    (None, text) => text.clone().unwrap_or_default(),
                },
            ),
            Proposal::Skill {
                name, description, ..
            } => ("skill", format!("`{name}`: {description}")),
        };
        let (outcome, why) = if x.status == DROPPED {
            (
                "rejected",
                format!("dropped by you; not proven: {}", x.gate),
            )
        } else if p.status == REVERTED {
            (
                "rejected",
                format!("its pass was reverted; not proven: {}", x.gate),
            )
        } else {
            debug_assert_eq!(x.status, PROPOSED);
            ("proposed", format!("{} — not proven: {}", x.reason, x.gate))
        };
        out.push(json!({
            "id": format!("{}:{}", p.id, x.id), "kind": kind, "scope": x.scope,
            "summary": r(&summary), "outcome": outcome, "why": r(&why),
        }));
    }
    for (k, x) in p.rejected.iter().enumerate() {
        let kind = match x.op.as_str() {
            "skill" => "skill",
            "add" | "edit" | "promote" | "retire" => lesson_kind(&x.op),
            // An unusable answer or a memory merge: no change to show.
            _ => continue,
        };
        let summary = match (&x.id, &x.text) {
            (Some(i), Some(t)) => format!("pb-{i}: {t}"),
            (Some(i), None) => format!("{} pb-{i}", x.op),
            (None, t) => t.clone().unwrap_or_default(),
        };
        out.push(json!({
            "id": format!("{}:r{}", p.id, k + 1), "kind": kind, "scope": Value::Null,
            "summary": r(&summary), "outcome": "rejected", "why": r(&x.reason),
        }));
    }
    out
}

fn count(changes: &[Value], outcome: &str) -> usize {
    changes.iter().filter(|c| c["outcome"] == outcome).count()
}

/// When a pass kept lesson `n`, if one did.
fn lesson_added(passes: &[PassRecord], n: u32) -> Option<String> {
    passes.iter().find_map(|p| {
        p.changes.iter().enumerate().find_map(|(i, c)| match c {
            Change::Playbook { applied, .. } if applied.op == "add" && applied.id == n => p
                .proposed
                .iter()
                .find(|x| x.kept_as == Some(i + 1))
                .and_then(|x| x.decided_at.clone())
                .or_else(|| p.finished_at.clone())
                .or_else(|| Some(p.started_at.clone())),
            _ => None,
        })
    })
}

fn lesson_uses(u: Option<&LessonUses>) -> Value {
    let u = u.copied().unwrap_or_default();
    json!({ "sessions": u.sessions, "failed": u.failed, "thumbs_down": u.thumbs_down })
}

fn skill_uses(u: Option<&SkillUses>) -> Value {
    let u = u.copied().unwrap_or_default();
    json!({ "activations": u.activations, "sessions": u.sessions, "failed": u.failed })
}

/// The learned skills, and those the agent wrote itself (`skill_keep`).
fn skills(
    ctx: &Ctx,
    data: &std::path::Path,
    dir: &LearnDir,
    stats: &ferrule_learn::usage::Stats,
) -> Vec<Value> {
    let found = |root: PathBuf, scope| {
        ferrule_skills::discover(&[ferrule_skills::SkillRoot { dir: root, scope }], &[]).skills
    };
    let mut out = Vec::new();
    let index = dir.skills_index();
    for s in found(dir.skills_root(), ferrule_skills::Scope::Learned) {
        let Some(e) = index.skills.get(&s.name) else {
            continue;
        };
        out.push(json!({
            "name": s.name, "scope": e.scope, "added": e.at, "origin": "learned",
            "description": ctx.redactor.redact(&s.description),
            "uses": skill_uses(stats.skills.get(&s.name)),
        }));
    }
    let layout = ferrule_extensions::Layout::new(data);
    let lock = ferrule_extensions::LockStore::new(layout.lock_path())
        .load()
        .unwrap_or_default();
    for s in found(layout.skills_dir(), ferrule_skills::Scope::User) {
        let Some(e) = lock
            .skills
            .get(&s.name)
            .filter(|e| e.origin == ferrule_extensions::Origin::SelfWritten)
        else {
            continue;
        };
        out.push(json!({
            "name": s.name, "scope": Value::Null, "added": e.installed_at, "origin": "agent",
            "description": ctx.redactor.redact(&s.description),
            "uses": skill_uses(stats.skills.get(&s.name)),
        }));
    }
    out
}

/// The owner's feedback in the last week: thumbs on replies (Telegram's
/// and the page's), and every signal, the audit log's too (approvals the
/// owner refused, the proactive agent's Not now, Never and Undo).
fn feedback(ctx: &Ctx, dir: &LearnDir, now: i64) -> Value {
    let since = now - WEEK;
    let thumbs: Vec<_> = dir.thumbs().into_iter().filter(|t| t.at >= since).collect();
    let up = thumbs.iter().filter(|t| t.up).count();
    let others = dir
        .signals()
        .iter()
        .filter(|s| s.at >= since && !s.is_thumb())
        .count();
    let audited = ctx
        .hub
        .as_ref()
        .and_then(|h| {
            h.audit()
                .read(chrono::DateTime::from_timestamp(since, 0))
                .ok()
        })
        .unwrap_or_default()
        .iter()
        .filter(|e| audit_signal(&e.event, &e.detail))
        .count();
    json!({
        "up_7d": up,
        "down_7d": thumbs.len() - up,
        "signals_7d": thumbs.len() + others + audited,
    })
}

/// Whether an audit event is one of the owner's signals.
fn audit_signal(event: &str, detail: &Value) -> bool {
    match event {
        "approval_answered" => detail["answer"] == "no",
        "proactive_answered" => matches!(detail["answer"].as_str(), Some("later" | "never")),
        "proactive_undone" => true,
        _ => false,
    }
}

/// The newest failure notes in the owner's memory.
fn notes(ctx: &Ctx, data: &std::path::Path) -> Vec<Value> {
    let db = data.join("memory.db");
    if !db.exists() {
        return Vec::new();
    }
    let mut all = MemoryStore::open(&db)
        .and_then(|s| s.live_tagged(&[FAILURE_NOTE]))
        .unwrap_or_default();
    all.reverse();
    all.iter()
        .take(NOTES_SHOWN)
        .map(|m| {
            let scope = m.tags.iter().find_map(|t| t.strip_prefix("scope:"));
            json!({
                "at": rfc(m.created_at), "scope": scope, "id": m.id,
                "text": ctx.redactor.redact(&m.content),
            })
        })
        .collect()
}

/// `GET /api/learn`.
pub fn view(ctx: &Ctx) -> Answer {
    let cfg = need!(loaded(ctx));
    let data = need!(data(ctx));
    let dir = LearnDir::new(&data);
    let l = &cfg.learning;
    let now = chrono::Utc::now().timestamp();
    let all = dir.passes();
    let passes: Vec<Value> = all
        .iter()
        .rev()
        .take(PASSES_SHOWN)
        .map(|p| {
            json!({
                "id": p.id, "at": p.finished_at.as_ref().unwrap_or(&p.started_at),
                "status": p.status, "spent_usd": p.spent.usd, "changes": changes_of(ctx, p),
            })
        })
        .collect();
    let last_pass = passes.first().map(|p| {
        let c = p["changes"].as_array().cloned().unwrap_or_default();
        json!({
            "at": p["at"], "status": p["status"], "spent_usd": p["spent_usd"],
            "kept": count(&c, "kept"), "rejected": count(&c, "rejected"),
            "proposed": count(&c, "proposed"),
        })
    });
    let stats = dir.usage_stats();
    let text = dir.read_playbook().unwrap_or_default();
    let lessons: Vec<Value> = ferrule_learn::Playbook::parse(&text)
        .lessons()
        .into_iter()
        .map(|x| {
            json!({
                "id": x.id.map(|n| format!("pb-{n}")),
                "scope": x.scope,
                "text": ctx.redactor.redact(&x.text),
                "added": x.id.and_then(|n| lesson_added(&all, n)),
                "uses": lesson_uses(x.id.and_then(|n| stats.lessons.get(&n))),
            })
        })
        .collect();
    let path = need!(place(ctx));
    let file = std::fs::read_to_string(&path).unwrap_or_default();
    ok(json!({
        "enabled": l.enabled,
        "needs_restart": needs_restart(&cfg),
        "schedule": schedule_words(&l.schedule, &l.timezone),
        "failure_notes": l.failure_notes,
        "self_written": cfg.skills.self_written,
        "eval": crate::learn::weekly::view(&cfg, &dir),
        "last_pass": last_pass,
        "passes": passes,
        "lessons": lessons,
        "skills": skills(ctx, &data, &dir, &stats),
        "feedback": feedback(ctx, &dir, now),
        "notes": notes(ctx, &data),
        "caps": {
            "usd_per_pass": l.max_usd_per_pass,
            "usd_per_day": l.max_usd_per_day,
            "tokens_per_pass": l.max_tokens_per_pass,
            "tokens_per_day": l.max_tokens_per_day,
            "max_episodes": l.max_episodes,
            "max_bullets": l.max_bullets,
        },
        // Beyond the page's contract: a pass running now, whether this is
        // the gateway, and the file's hash for a settings write.
        "running": dir.lock_holder().is_some_and(|(_, age)| age < ferrule_learn::files::STALE_LOCK),
        "live": ctx.live.is_some(),
        "hash": hash_of(&file),
    }))
}

/// A saved change the gateway in this process doesn't follow yet: it
/// started reading Telegram's 👍/👎 for learning, or not, by the old
/// setting. The nightly pass and failure notes follow at once.
fn needs_restart(cfg: &Config) -> bool {
    crate::learn::telegram_started().is_some_and(|on| on != cfg.learning.enabled)
}

/// The learning page's writes: `learn/settings`, `learn/run`,
/// `learn/revert`, `learn/keep`, `learn/drop`.
pub async fn post(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    match path {
        "learn/settings" => settings(ctx, body),
        "learn/run" => run(ctx, body).await,
        "learn/revert" => revert(ctx, body),
        "learn/keep" => keep(ctx, body),
        "learn/drop" => drop(ctx, body),
        "learn/eval" => eval(ctx).await,
        _ => None,
    }
}

/// The switches `learn/settings` writes, and their sections.
const SWITCHES: &[(&str, &str)] = &[
    ("enabled", "learning"),
    ("failure_notes", "learning"),
    ("weekly_eval", "learning"),
    ("self_written", "skills"),
];

/// `POST /api/learn/settings {enabled?, failure_notes?, weekly_eval?,
/// self_written?, base_hash?, confirm?}`.
fn settings(ctx: &Ctx, body: &Value) -> Answer {
    let path = need!(place(ctx));
    need!(fresh(&path, body));
    let (_, old) = need!(read_doc(&path));
    let Some(old) = old else {
        return bad(
            422,
            "the config file doesn't read as TOML; fix it on the Config page or restore the last good copy",
        );
    };
    let before = need!(loaded(ctx));
    let mut set: Vec<(&str, &str, bool)> = Vec::new();
    for (key, section) in SWITCHES {
        let Some(v) = body.get(*key) else { continue };
        match v.as_bool() {
            Some(b) => set.push((section, key, b)),
            None => return bad(400, format!("`{key}` takes true or false")),
        }
    }
    if set.is_empty() {
        return bad(
            400,
            "nothing to change: send enabled, failure_notes, weekly_eval or self_written",
        );
    }
    // Saving its own skills widens what the agent can do: asked first.
    if set.contains(&("skills", "self_written", true)) && !before.skills.self_written {
        need!(confirmed(
            body,
            "Let the agent save skills it writes itself? Each one is kept only when its check passes in the sandbox and it scans clean, and it shows on this page; nothing is installed from the internet. New sessions get it.".into()
        ));
    }
    let mut new = old.clone();
    for (section, key, value) in &set {
        let Some(table) = new
            .entry(section)
            .or_insert_with(toml_edit::table)
            .as_table_like_mut()
        else {
            return bad(422, format!("`[{section}]` isn't a table"));
        };
        table.insert(key, TomlItem::Value((*value).into()));
    }
    let mut answer = if new.to_string() != old.to_string() {
        match commit(ctx, &path, Some(&old), &new, "learning", true) {
            Some((200, v)) => v,
            other => return other,
        }
    } else {
        json!({ "ok": true, "sections": [] })
    };
    let cfg = need!(loaded(ctx));
    let l = &cfg.learning;
    let task = match crate::learn::apply(&cfg) {
        Ok(words) => words,
        Err(e) => format!("the pass's task wasn't updated: {e:#}"),
    };
    crate::self_extend::set_self_written(cfg.skills.self_written);
    let switch = |c: &Config, key: &str| match key {
        "enabled" => c.learning.enabled,
        "failure_notes" => c.learning.failure_notes,
        "weekly_eval" => c.learning.weekly_eval,
        _ => c.skills.self_written,
    };
    let changed: Vec<String> = SWITCHES
        .iter()
        .filter(|(k, _)| switch(&before, k) != switch(&cfg, k))
        .map(|(k, _)| format!("{k} {} → {}", switch(&before, k), switch(&cfg, k)))
        .collect();
    if !changed.is_empty() {
        if let Some(hub) = &ctx.hub {
            hub.audit().record(
                chrono::Utc::now(),
                "learning_settings",
                None,
                None,
                json!({ "changes": changed, "by": by() }),
            );
        }
    }
    let state = if l.enabled {
        format!(
            "Learning is on: a pass runs {}, within ${:.2} a pass and ${:.2} a day.",
            schedule_words(&l.schedule, &l.timezone),
            l.max_usd_per_pass,
            l.max_usd_per_day
        )
    } else {
        "Learning is off: no pass runs by itself; Run still starts one.".to_string()
    };
    let notes = if l.failure_notes {
        "A run that ends unfinished leaves a note for a week."
    } else {
        "Unfinished runs leave no note."
    };
    let eval = if l.weekly_eval {
        format!(
            " The playbook is measured {} (with and without it), within ${:.2}.",
            schedule_words(&l.eval_schedule, &l.timezone),
            l.eval_usd_cap
        )
    } else {
        String::new()
    };
    let skills = if cfg.skills.self_written {
        if ctx.live.is_some() {
            " New sessions can save skills they write (skill_keep)."
        } else {
            " The bot's sessions can save skills they write from its next start."
        }
    } else {
        ""
    };
    let what = if changed.is_empty() {
        "Nothing changed.".to_string()
    } else {
        format!("Changed: {}.", changed.join(", "))
    };
    let restart = needs_restart(&cfg);
    let later = if restart {
        " Telegram's 👍/👎 follow it from the bot's next restart."
    } else {
        ""
    };
    answer["ok"] = json!(true);
    answer["said"] = json!(format!(
        "{state} {notes}{eval}{skills} {what} Now {task}.{later}"
    ));
    answer["needs_restart"] = json!(restart);
    answer["restart"] = json!(restart);
    ok(answer)
}

/// `POST /api/learn/run {dry_run?}`: a pass in the background, or what
/// one would review.
async fn run(ctx: &Ctx, body: &Value) -> Answer {
    let cfg = need!(loaded(ctx));
    let dry = body
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let fallback = ctx.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
    match crate::learn::start(&cfg, dry, &fallback).await {
        Ok(s) => ok(json!({ "ok": true, "said": ctx.redactor.redact(&s.said), "id": s.id })),
        Err(e) => bad(409, ctx.redactor.redact(&format!("{e:#}"))),
    }
}

/// `POST /api/learn/eval`: the weekly playbook eval, now, in the
/// background; refused in words over a cap, with the kill switch on, with
/// one running or nothing to measure.
async fn eval(ctx: &Ctx) -> Answer {
    let cfg = need!(loaded(ctx));
    match crate::learn::weekly::start(&cfg).await {
        Ok(said) => ok(json!({ "ok": true, "said": ctx.redactor.redact(&said) })),
        Err(e) => bad(409, ctx.redactor.redact(&format!("{e:#}"))),
    }
}

/// A change id: `<pass>:<n>` (kept), `<pass>:p<k>` (proposed),
/// `<pass>:r<k>` (rejected), or a whole pass.
enum Id<'a> {
    Pass(&'a str),
    Kept(&'a str, usize),
    Proposed(&'a str, &'a str),
    Rejected,
}

fn parse_id(id: &str) -> Option<Id<'_>> {
    let Some((pass, rest)) = id.split_once(':') else {
        return Some(Id::Pass(id));
    };
    if let Ok(n) = rest.parse::<usize>() {
        return Some(Id::Kept(pass, n));
    }
    if rest.starts_with('p') && rest[1..].parse::<usize>().is_ok() {
        return Some(Id::Proposed(pass, rest));
    }
    if rest.starts_with('r') && rest[1..].parse::<usize>().is_ok() {
        return Some(Id::Rejected);
    }
    None
}

fn refused(ctx: &Ctx, said: &str) -> Answer {
    let said = ctx.redactor.redact(said);
    Some((409, json!({ "ok": false, "error": said, "said": said })))
}

/// `POST /api/learn/revert {id}`: a kept change's Undo, as `ferrule learn
/// revert <pass> --change N`; a pass id undoes the whole pass.
fn revert(ctx: &Ctx, body: &Value) -> Answer {
    let id = need!(arg(body, "id"));
    let data = need!(data(ctx));
    match parse_id(id) {
        Some(Id::Kept(pass, n)) => {
            let said = crate::learn::unlearn(pass, n);
            if said.starts_with("Done") {
                ok(json!({ "ok": true, "said": ctx.redactor.redact(&said) }))
            } else {
                refused(ctx, &said)
            }
        }
        Some(Id::Pass(pass)) => {
            let dir = LearnDir::new(&data);
            match ferrule_learn::revert(&dir, pass, Some(&data.join("memory.db"))) {
                Ok(p) => {
                    crate::learn::signal(signals::UNLEARN, "dashboard", Some(p.id.clone()));
                    let r = p
                        .reverted
                        .unwrap_or_else(|| ferrule_learn::files::RevertNote {
                            at: String::new(),
                            undone: vec![],
                            not_undone: vec![],
                        });
                    let mut said = format!("Reverted pass {}: {}.", p.id, r.undone.join("; "));
                    if !r.not_undone.is_empty() {
                        said.push_str(&format!(" Not undone: {}.", r.not_undone.join("; ")));
                    }
                    ok(json!({ "ok": true, "said": ctx.redactor.redact(&said) }))
                }
                Err(e) => refused(ctx, &format!("Nothing was undone: {e:#}")),
            }
        }
        Some(Id::Proposed(..)) => bad(
            400,
            "That change is only proposed: it never landed. Keep or Drop it instead.",
        ),
        Some(Id::Rejected) => bad(400, "That change was rejected: it never landed."),
        None => bad(400, format!("`{id}` isn't a change id")),
    }
}

/// `<pass>:p<k>`, or the answer that says why not.
fn proposal(body: &Value) -> Result<(String, String), Answer> {
    let id = arg(body, "id")?;
    match parse_id(id) {
        Some(Id::Proposed(pass, k)) => Ok((pass.to_string(), k.to_string())),
        _ => Err(bad(
            400,
            format!("`{id}` isn't a proposed change (they look like `<pass>:p1`)"),
        )),
    }
}

/// `POST /api/learn/keep {id}`: a proposed change lands, as a kept one
/// does, with its "Learned" line and Undo.
fn keep(ctx: &Ctx, body: &Value) -> Answer {
    let (pass, k) = need!(proposal(body));
    let cfg = need!(loaded(ctx));
    let dir = LearnDir::new(&need!(data(ctx)));
    match ferrule_learn::proposed::keep(&dir, &pass, &k, cfg.learning.max_bullets) {
        Ok(l) => {
            crate::learn::signal(signals::KEEP, "dashboard", Some(format!("{pass}:{k}")));
            if let Some(hub) = &ctx.hub {
                hub.tell_owner_choices(
                    l.line.clone(),
                    vec![("Undo".into(), crate::learn::undo_command(&l))],
                );
            }
            ok(json!({
                "ok": true,
                "said": ctx.redactor.redact(&l.line),
                "change": format!("{}:{}", l.pass, l.change),
            }))
        }
        Err(e) => refused(ctx, &format!("Not kept: {e:#}")),
    }
}

/// `POST /api/learn/drop {id}`: a proposed change never lands.
fn drop(ctx: &Ctx, body: &Value) -> Answer {
    let (pass, k) = need!(proposal(body));
    let dir = LearnDir::new(&need!(data(ctx)));
    match ferrule_learn::proposed::drop(&dir, &pass, &k) {
        Ok(said) => {
            crate::learn::signal(signals::DROP, "dashboard", Some(format!("{pass}:{k}")));
            ok(json!({ "ok": true, "said": ctx.redactor.redact(&said) }))
        }
        Err(e) => refused(ctx, &format!("Not dropped: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_read_as_words() {
        assert_eq!(
            schedule_words("0 3 * * *", "UTC"),
            "every night at 03:00 (UTC)"
        );
        assert_eq!(
            schedule_words("30 9 * * *", "Asia/Jerusalem"),
            "every day at 09:30 (Asia/Jerusalem)"
        );
        assert_eq!(
            schedule_words("0 9 * * 1-5", "UTC"),
            "on weekdays at 09:00 (UTC)"
        );
        assert_eq!(
            schedule_words("43 4 * * 0", "UTC"),
            "every Sunday at 04:43 (UTC)"
        );
        assert_eq!(
            schedule_words("*/5 * * * *", "UTC"),
            "on the schedule `*/5 * * * *` (UTC)"
        );
    }

    #[test]
    fn change_ids_parse() {
        assert!(matches!(
            parse_id("20261103-030000-ab12:2"),
            Some(Id::Kept(_, 2))
        ));
        assert!(matches!(
            parse_id("20261103-030000-ab12:p1"),
            Some(Id::Proposed(_, "p1"))
        ));
        assert!(matches!(parse_id("x:r3"), Some(Id::Rejected)));
        assert!(matches!(
            parse_id("20261103-030000-ab12"),
            Some(Id::Pass(_))
        ));
        assert!(parse_id("x:zz").is_none());
    }

    #[test]
    fn the_audits_signals_are_refusals_and_proactive_answers() {
        assert!(audit_signal("approval_answered", &json!({"answer": "no"})));
        assert!(!audit_signal(
            "approval_answered",
            &json!({"answer": "timeout"})
        ));
        assert!(audit_signal(
            "proactive_answered",
            &json!({"answer": "never"})
        ));
        assert!(!audit_signal(
            "proactive_answered",
            &json!({"answer": "do"})
        ));
        assert!(audit_signal("proactive_undone", &json!({})));
    }
}
