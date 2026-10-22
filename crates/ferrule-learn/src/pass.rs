//! One learning pass, end to end (`docs/m16-learning-loop.md`): review the
//! episodes after the cursor, gate and apply playbook deltas, merge memory
//! clusters, and journal every step; plus the dry-run plan and revert.

use crate::budget::{row, Caps, LearnSink, Meter, PriceFn, Spent};
use crate::consolidate::{self, CLUSTER_JACCARD, CLUSTER_ROWS};
use crate::diff::unified;
use crate::episode::{render_run, render_tail, scan_sessions, Episode};
use crate::files::{
    Change, ChangeRevert, EpisodeNote, LearnDir, PassRecord, Rejected, RevertNote, State, DONE,
    REVERTED, RUNNING, STOPPED_BUDGET, STOPPED_ERRORS,
};
use crate::gate::{Gate, GateRun, GateVerdict, ProbeOut};
use crate::judge::{Contrast, Side, Slate};
use crate::playbook::{prompt_block_for, Delta, Playbook, TEMPLATE};
use crate::reflect::{self, Proposal};
use crate::screen;
use anyhow::{bail, Result};
use ferrule_core::{CompletionRequest, LedgerSink, Message, Provider, Usage};
use ferrule_memory::{Memory, MemoryStore};
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// Model calls that fail in a row before the pass gives up.
pub const MAX_ERRORS_IN_ROW: usize = 3;

/// M53.4: from this many lessons in a prompt's view (90% of
/// `max_bullets`), an add must retire one: add one, cut one.
pub fn near_cap(max_bullets: usize) -> usize {
    (max_bullets * 9 / 10).max(1)
}

/// How the retire that made room for an add begins its reason.
const MADE_ROOM: &str = "made room for ";

/// The start of the reason of the retire that made room for `a`.
fn made_room(a: &crate::playbook::Applied) -> String {
    format!("{MADE_ROOM}{} pb-{} (", a.op, a.id)
}

/// M53.5: one change a pass kept, for the owner's "Learned" line.
#[derive(Debug, Clone, PartialEq)]
pub struct Learned {
    pub pass: String,
    /// The change's number in the pass, from 1 (`learn revert --change`).
    pub change: usize,
    /// "Learned: …", one line.
    pub line: String,
}

/// Told each lesson or skill a pass kept, once the pass is over and its
/// lock released (so an Undo can run at once). Memory merges and the
/// retires that made room for an add aren't told: the add's Undo takes its
/// retire back too.
pub type OnKept = Arc<dyn Fn(&Learned) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct Options {
    pub caps: Caps,
    pub max_episodes: usize,
    /// Lessons the playbook may hold; also the prompt cap.
    pub max_bullets: usize,
    pub max_clusters: usize,
    /// Characters of lessons a prompt gets.
    pub max_prompt_chars: usize,
    /// `manual` or `scheduled`.
    pub trigger: String,
    /// For the journal: the workspace the gate copies.
    pub workspace: Option<PathBuf>,
}

/// What a pass works with; the CLI builds it from the config.
pub struct Env {
    pub dir: LearnDir,
    pub provider: Arc<dyn Provider>,
    /// As the ledger records it.
    pub provider_name: String,
    pub model: String,
    pub ledger: Option<Arc<dyn LedgerSink>>,
    pub price: PriceFn,
    /// Whether the provider has prices; without them only token caps hold.
    pub priced: bool,
    /// What earlier passes spent in the last 24 hours (from the ledger).
    pub day_before: Spent,
    pub memory_db: Option<PathBuf>,
    pub gate: Arc<dyn Gate>,
    /// Scanned for retried sessions.
    pub sessions_dir: Option<PathBuf>,
    /// Scheduled-task runs, collected by the caller from the task store.
    /// The owner's 👎s (M53.5) the pass reads itself.
    pub task_episodes: Vec<Episode>,
    /// M53.5: told what the pass kept.
    pub on_kept: Option<OnKept>,
}

/// What `--dry-run` shows: the pass's input, nothing spent or written.
#[derive(Debug)]
pub struct Plan {
    pub cursor: i64,
    pub episodes: Vec<Episode>,
    /// Episodes past `max_episodes`, left for a later pass.
    pub later: usize,
    pub clusters: Vec<Vec<Memory>>,
    pub later_clusters: usize,
    pub caps: Caps,
    pub day_before: Spent,
    pub check: Option<String>,
    pub priced: bool,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Every episode after `cursor`, oldest first, each once.
fn episodes(env: &Env, cursor: i64) -> Vec<Episode> {
    let mut all: Vec<Episode> = env
        .task_episodes
        .iter()
        .filter(|e| e.at > cursor)
        .cloned()
        .collect();
    if let Some(dir) = &env.sessions_dir {
        all.extend(scan_sessions(dir, cursor));
    }
    all.extend(crate::thumbs::episodes(&env.dir, cursor));
    all.sort_by(|a, b| a.at.cmp(&b.at).then(a.key.cmp(&b.key)));
    let mut seen = HashSet::new();
    all.retain(|e| seen.insert(e.key.clone()));
    all
}

fn clusters(env: &Env) -> Result<Vec<Vec<Memory>>> {
    let Some(db) = &env.memory_db else {
        return Ok(Vec::new());
    };
    if !db.exists() {
        return Ok(Vec::new());
    }
    let store = MemoryStore::open(db)?;
    Ok(store.similar_clusters(CLUSTER_JACCARD, CLUSTER_ROWS)?)
}

pub fn plan(opts: &Options, env: &Env) -> Result<Plan> {
    let cursor = env.dir.state()?.cursor;
    let mut eps = episodes(env, cursor);
    let later = eps.len().saturating_sub(opts.max_episodes);
    eps.truncate(opts.max_episodes);
    let mut cl = clusters(env)?;
    let later_clusters = cl.len().saturating_sub(opts.max_clusters);
    cl.truncate(opts.max_clusters);
    Ok(Plan {
        cursor,
        episodes: eps,
        later,
        clusters: cl,
        later_clusters,
        caps: opts.caps,
        day_before: env.day_before,
        check: env.gate.check(),
        priced: env.priced,
    })
}

enum Stop {
    Budget(String),
    Errors(String),
}

/// One model call of the pass itself, recorded in the ledger through a
/// [`LearnSink`].
async fn ask(
    env: &Env,
    meter: &Arc<Meter>,
    pass: &str,
    origin: &str,
    system: &str,
    user: String,
) -> Result<String, String> {
    let sink = LearnSink::new(env.ledger.clone(), meter.clone(), env.price.clone(), origin);
    let req = CompletionRequest {
        messages: vec![Message::system(system), Message::user(user)],
        tools: vec![],
        // M53.3: a skill's body and scripts need room; answers are capped
        // by the pass's token budget either way.
        max_output_tokens: Some(4096),
        temperature: Some(0.0),
        stream: None,
    };
    let started = Instant::now();
    let res = env.provider.complete(req).await;
    let ms = started.elapsed().as_millis() as u64;
    match res {
        Ok(resp) => {
            sink.record(row(
                pass,
                &env.provider_name,
                &env.model,
                0,
                &resp.usage,
                ms,
                None,
            ));
            Ok(resp.message.content.unwrap_or_default())
        }
        Err(e) => {
            let msg = e.to_string();
            sink.record(row(
                pass,
                &env.provider_name,
                &env.model,
                0,
                &Usage::default(),
                ms,
                Some(&msg),
            ));
            Err(msg)
        }
    }
}

/// Why weak evidence is weak: no control run fitted.
const NO_CONTROL: &str =
    "the caps left room for one re-run only, so there was no control run without the change";

/// M53.1 (wave C): the evidence a change needs. The episode re-runs with
/// the change (`with`): through the check when there is one, else as a
/// probe. When the caps leave room for another run of what that one cost,
/// it re-runs without the change (`control`), and:
/// - with a check, the change is kept only when the control fails it;
/// - without one, the judge compares the two runs blind (seeded A/B) and
///   the change is kept only when it picks the run with the change.
///
/// Without a control the evidence is weak (the check alone, or the judge
/// against the episode's own run, as before M53.1's fix): the change is
/// then kept only when the owner's 👍 corroborates it ([`weak`]).
async fn contrast_gate(
    env: &Env,
    meter: &Arc<Meter>,
    pass: &str,
    ep: &Episode,
    with: GateRun<'_>,
    control: GateRun<'_>,
) -> GateVerdict {
    let slate = Slate::new(pass, with.n);
    let before = meter.spent();
    if env.gate.check().is_some() {
        let v = env.gate.run(with).await;
        if !v.passed {
            return v;
        }
        if !meter.room_for(meter.spent().since(before)) {
            return weak(ep, &v.reason, NO_CONTROL);
        }
        let c = env.gate.run(control).await;
        if meter.exceeded().is_some() {
            return GateVerdict::fail(format!(
                "{}; the control run without it was cut by the budget",
                v.reason
            ));
        }
        if c.passed {
            return GateVerdict::fail(format!(
                "{}; but {}: nothing shows the lesson helped",
                v.reason, c.reason
            ));
        }
        return GateVerdict {
            passed: true,
            reason: format!("{}; without it, {}", v.reason, c.reason),
        };
    }
    let Some(w) = env.gate.probe(with).await else {
        return GateVerdict::fail("no check to gate on");
    };
    if !w.finished {
        return GateVerdict::fail(format!(
            "the probe run with the change stopped before finishing: {}",
            w.incomplete.as_deref().unwrap_or("unknown")
        ));
    }
    if meter.room_for(meter.spent().since(before)) {
        let Some(c) = env.gate.probe(control).await else {
            return GateVerdict::fail("the gate could not run the control");
        };
        if meter.exceeded().is_some() {
            return GateVerdict::fail("budget reached during the control run");
        }
        if !c.finished && c.tail.trim().is_empty() {
            return GateVerdict::fail(format!(
                "the control run without the change could not run: {}",
                c.incomplete.as_deref().unwrap_or("unknown")
            ));
        }
        let other = Side {
            tail: &c.tail,
            stopped: c.incomplete.as_deref(),
        };
        let blind = slate.describe();
        return match judge(env, meter, pass, ep, &slate, &w, other, "control").await {
            Err(why) => GateVerdict::fail(why),
            Ok((Contrast::Better, why)) => GateVerdict {
                passed: true,
                reason: format!(
                    "kept by the judge: the run with the change beat a control run without it ({blind}): {why}"
                ),
            },
            Ok((Contrast::Worse, why)) => GateVerdict::fail(format!(
                "the judge found the control run without the change better ({blind}): {why}"
            )),
            Ok((Contrast::Confounded, why)) => GateVerdict::fail(format!(
                "the judge saw no clear difference with and without the change ({blind}): {why}"
            )),
        };
    }
    let original = ep.transcript.as_deref().map(render_run).unwrap_or_default();
    let other = Side {
        tail: &original,
        stopped: None,
    };
    let blind = slate.describe();
    match judge(env, meter, pass, ep, &slate, &w, other, "original").await {
        Err(why) => GateVerdict::fail(format!("weak evidence ({NO_CONTROL}): {why}")),
        Ok((Contrast::Better, why)) => weak(
            ep,
            &format!(
                "the judge preferred the run with the change to the episode's own run ({blind}): {why}"
            ),
            NO_CONTROL,
        ),
        Ok((_, why)) => GateVerdict::fail(format!(
            "weak evidence ({NO_CONTROL}), and the judge did not prefer the run with the change to the episode's own run ({blind}): {why}"
        )),
    }
}

/// Weak evidence is kept only when the owner's 👍 corroborates it: the
/// episode is a 👎 on a chat reply (M53.5) and the owner reacted 👍 to a
/// later reply in the same chat. That says the chat ended well, not that
/// the change did; it is the only owner's word the pass has.
fn weak(ep: &Episode, found: &str, why: &str) -> GateVerdict {
    if ep.outcome == crate::thumbs::OUTCOME && ep.fixed {
        GateVerdict {
            passed: true,
            reason: format!(
                "kept on weak evidence ({why}), corroborated by the owner's 👍 on a later reply in this chat: {found}"
            ),
        }
    } else {
        GateVerdict::fail(format!(
            "weak evidence, not kept without the owner's 👍 ({why}): {found}"
        ))
    }
}

/// One blind judge call: the run with the change against `other`, in the
/// slate's order. An unreadable answer is confounded; a failed call is an
/// error.
#[allow(clippy::too_many_arguments)]
async fn judge(
    env: &Env,
    meter: &Arc<Meter>,
    pass: &str,
    ep: &Episode,
    slate: &Slate,
    with: &ProbeOut,
    other: Side<'_>,
    against: &str,
) -> Result<(Contrast, String), String> {
    let w = Side {
        tail: &with.tail,
        stopped: None,
    };
    let (a, b) = slate.order(w, other);
    env.dir.log(
        pass,
        "judge",
        json!({ "episode": ep.key, "seed": format!("{:016x}", slate.seed),
                "change_as": slate.with_label().to_string(), "against": against }),
    );
    let answer = ask(
        env,
        meter,
        pass,
        "judge",
        crate::judge::SYSTEM,
        crate::judge::user_message(&ep.goal, a, b),
    )
    .await
    .map_err(|e| format!("the judge call failed: {e}"))?;
    Ok(match crate::judge::parse(&answer) {
        Err(e) => (
            Contrast::Confounded,
            format!("its answer was unusable, so it counts as confounded: {e}"),
        ),
        Ok(v) => (slate.contrast(&v), v.why),
    })
}

/// Runs one pass. Takes the lock, so a second concurrent pass is refused.
pub async fn run_pass(opts: &Options, env: &Env) -> Result<PassRecord> {
    let dir = &env.dir;
    let id = dir.new_pass_id();
    let lock = dir.lock(&format!("pass {id}"))?;
    let recovered = dir.recover(&id);
    let state = dir.state()?;
    let before = dir.read_playbook()?;
    let meter = Meter::new(opts.caps, env.day_before);
    let mut rec = PassRecord {
        id: id.clone(),
        status: RUNNING.into(),
        trigger: opts.trigger.clone(),
        started_at: now(),
        finished_at: None,
        caps: opts.caps,
        spent: Spent::default(),
        day_spent_before: env.day_before,
        check: env.gate.check(),
        workspace: opts.workspace.as_ref().map(|p| p.display().to_string()),
        cursor_before: state.cursor,
        cursor_after: state.cursor,
        episodes: vec![],
        changes: vec![],
        rejected: vec![],
        skipped: vec![],
        notes: vec![],
        reverted: None,
        undone: vec![],
    };
    for r in &recovered {
        rec.notes
            .push(format!("pass {r} had stopped midway; marked interrupted"));
    }
    if !env.priced {
        rec.notes.push(
            "the provider has no prices in config: calls carry no cost, only the token caps apply"
                .into(),
        );
    }
    if rec.check.is_none() {
        rec.notes.push(
            "no check to gate on ([learning] check or [agent] verify_command): adds and edits are probed with and without the change and judged blind (M53, M53.1); retires are not gated"
                .into(),
        );
    }
    dir.write_pass_file(&id, "playbook.before.md", &before)?;
    dir.save_pass(&rec)?;
    dir.log(
        &id,
        "start",
        json!({ "trigger": opts.trigger, "caps": opts.caps }),
    );

    let result = body(opts, env, &meter, &mut rec, state, &before).await;
    rec.spent = meter.spent();
    rec.finished_at = Some(now());
    if let Err(e) = &result {
        rec.status = crate::files::INTERRUPTED.into();
        rec.notes.push(format!("the pass failed: {e:#}"));
    }
    let after = dir.read_playbook().unwrap_or_else(|_| before.clone());
    dir.write_pass_file(&id, "playbook.after.md", &after)?;
    let diff = unified(&before, &after, "playbook.before.md", "playbook.after.md");
    dir.write_pass_file(&id, "playbook.diff", &diff)?;
    dir.save_pass(&rec)?;
    dir.write_pass_file(&id, "changelog.md", &changelog(&rec, &diff))?;
    dir.log(
        &id,
        "finish",
        json!({ "status": rec.status, "changes": rec.changes.len(),
                "rejected": rec.rejected.len(), "spent": rec.spent }),
    );
    drop(lock);
    if let Some(told) = &env.on_kept {
        for l in learned(&rec) {
            told(&l);
        }
    }
    result?;
    Ok(rec)
}

/// The "Learned" lines for what `rec` kept: lessons and skills, each with
/// the number its Undo names.
pub fn learned(rec: &PassRecord) -> Vec<Learned> {
    let clip = |s: &str| {
        let s = s.trim();
        if s.chars().count() <= 200 {
            s.to_string()
        } else {
            format!("{}…", s.chars().take(200).collect::<String>())
        }
    };
    let mut out = Vec::new();
    for (i, c) in rec.changes.iter().enumerate() {
        let line = match c {
            Change::Playbook {
                applied, reason, ..
            } if applied.op == "retire" && reason.starts_with(MADE_ROOM) => continue,
            Change::Playbook { applied, .. } => {
                let text = clip(
                    applied
                        .new
                        .as_deref()
                        .or(applied.old.as_deref())
                        .unwrap_or(""),
                );
                let mut line = match applied.op.as_str() {
                    "add" => format!("Learned: {text}"),
                    "edit" => format!("Learned: pb-{} now reads \"{text}\"", applied.id),
                    "promote" => format!("Learned: \"{text}\" now applies everywhere"),
                    "retire" => format!("Learned to drop: {text}"),
                    other => format!("Learned ({other} pb-{}): {text}", applied.id),
                };
                if let (Some(scope), "add") = (&applied.scope, applied.op.as_str()) {
                    line.push_str(&format!(" (only in {scope})"));
                }
                if let Some(Change::Playbook {
                    applied: cut,
                    reason,
                    ..
                }) = rec.changes.get(i + 1)
                {
                    if reason.starts_with(&made_room(applied)) {
                        line.push_str(&format!(" (pb-{} retired to make room)", cut.id));
                    }
                }
                line
            }
            Change::Skill {
                name,
                description,
                op,
                ..
            } => match op.as_str() {
                "promote" => format!(
                    "Learned: the skill `{name}` now applies everywhere: {}",
                    clip(description)
                ),
                "replace" => format!(
                    "Learned a new version of the skill `{name}`: {}",
                    clip(description)
                ),
                _ => format!("Learned a skill `{name}`: {}", clip(description)),
            },
            Change::Memory { .. } => continue,
        };
        out.push(Learned {
            pass: rec.id.clone(),
            change: i + 1,
            line,
        });
    }
    out
}

async fn body(
    opts: &Options,
    env: &Env,
    meter: &Arc<Meter>,
    rec: &mut PassRecord,
    mut state: State,
    before: &str,
) -> Result<()> {
    let dir = &env.dir;
    let id = rec.id.clone();
    let pass_dir = dir.pass_dir(&id);
    let mut all = episodes(env, state.cursor);
    let later = all.len().saturating_sub(opts.max_episodes);
    all.truncate(opts.max_episodes);
    if later > 0 {
        rec.notes.push(format!(
            "{later} more episode(s) past max_episodes, left for the next pass"
        ));
    }

    let mut pb = Playbook::parse(if before.trim().is_empty() {
        TEMPLATE
    } else {
        before
    });
    let mut last_id = state.last_id.max(pb.max_id());
    let mut cursor = state.cursor;
    let mut errors = 0usize;
    let mut stop: Option<Stop> = None;
    let mut gate_n = 0usize;
    // A failed call leaves its episode for the next pass.
    let mut first_failed: Option<i64> = None;

    for (i, ep) in all.iter().enumerate() {
        if let Some(why) = meter.exceeded() {
            stop = Some(Stop::Budget(why));
            skip_episodes(rec, &all[i..]);
            break;
        }
        let tail = ep
            .transcript
            .as_deref()
            .map(render_tail)
            .unwrap_or_default();
        // M53.4: the episode's scope, and what a prompt there sees.
        let scope = ep
            .scope
            .clone()
            .or_else(|| opts.workspace.as_deref().map(crate::scope::workspace));
        let scope = scope.as_deref();
        let lessons = pb.lessons_for(scope);
        let full = lessons.len() >= near_cap(opts.max_bullets);
        let answer = ask(
            env,
            meter,
            &id,
            "reflect",
            reflect::SYSTEM,
            reflect::user_message(&lessons, full, &dir.learned_skills(scope), ep, &tail),
        )
        .await;
        let text = match answer {
            Ok(t) => {
                errors = 0;
                t
            }
            Err(e) => {
                errors += 1;
                note(rec, ep, format!("skipped: the reflector call failed: {e}"));
                dir.log(&id, "skip", json!({ "episode": ep.key, "reason": e }));
                first_failed.get_or_insert(ep.at);
                if errors >= MAX_ERRORS_IN_ROW {
                    stop = Some(Stop::Errors(format!(
                        "{MAX_ERRORS_IN_ROW} model calls failed in a row; last: {e}"
                    )));
                    skip_episodes(rec, &all[i + 1..]);
                    break;
                }
                continue;
            }
        };
        let proposal = match reflect::parse(&text) {
            Ok(p) => p,
            Err(reason) => {
                reject(rec, dir, &ep.key, "answer", None, None, reason);
                note(rec, ep, "rejected: unusable answer".into());
                cursor = cursor.max(ep.at);
                dir.save_pass(rec)?;
                continue;
            }
        };
        let Proposal {
            delta,
            skill: proposed_skill,
            reason,
            evidence,
            retire: pick,
        } = proposal;
        // M53.3: a skill goes through its own validators, then the same gate.
        if let Some(draft) = proposed_skill {
            let block =
                prompt_block_for(&pb.render(), scope, opts.max_bullets, opts.max_prompt_chars);
            let flow = skill::delta(
                skill::Ctx {
                    env,
                    meter,
                    pass_dir: &pass_dir,
                    gate_n: &mut gate_n,
                    episode: ep,
                    playbook_block: block.text.as_deref(),
                    scope,
                },
                rec,
                draft,
                reason,
                evidence,
            )
            .await?;
            match flow {
                skill::Flow::Next => {
                    cursor = cursor.max(ep.at);
                    rec.spent = meter.spent();
                    dir.save_pass(rec)?;
                    continue;
                }
                skill::Flow::BudgetBefore(why) => {
                    stop = Some(Stop::Budget(why));
                    skip_episodes(rec, &all[i..]);
                    break;
                }
                skill::Flow::BudgetAfter(why) => {
                    stop = Some(Stop::Budget(why));
                    skip_episodes(rec, &all[i + 1..]);
                    break;
                }
            }
        }
        let Some(mut delta) = delta else {
            note(rec, ep, format!("no change: {reason}"));
            cursor = cursor.max(ep.at);
            dir.save_pass(rec)?;
            continue;
        };
        let (op, did, dtext) = (
            delta.op(),
            match &delta {
                Delta::Add { .. } => None,
                Delta::Edit { id, .. } | Delta::Retire { id } | Delta::Promote { id } => Some(*id),
            },
            delta.text().map(str::to_string),
        );
        let visible = |n: u32| lessons.iter().any(|l| l.id == Some(n));
        // Guards on the proposal itself.
        let guard = match &delta {
            Delta::Add { text } => screen::check(text, &lessons, None),
            Delta::Edit { id: n, text } if visible(*n) => screen::check(text, &lessons, Some(*n)),
            Delta::Edit { id: n, .. } => Err(format!("there is no lesson pb-{n} to edit here")),
            Delta::Retire { id: n } if visible(*n) => Ok(()),
            Delta::Retire { id: n } => Err(format!("there is no lesson pb-{n} to retire here")),
            Delta::Promote { .. } => Err("the reflector can't promote".into()),
        };
        if let Err(why) = guard {
            reject(rec, dir, &ep.key, op, did, dtext, why);
            note(rec, ep, format!("rejected {op}"));
            cursor = cursor.max(ep.at);
            dir.save_pass(rec)?;
            continue;
        }
        // M53.4: the same lesson kept in another scope before: this second
        // scope promotes it to global instead of adding a copy.
        if let Delta::Add { text } = &delta {
            let elsewhere = pb.lessons().into_iter().find(|l| {
                l.id.is_some()
                    && l.scope.is_some()
                    && l.scope.as_deref() != scope
                    && screen::same(text, &l.text)
            });
            if let Some(l) = elsewhere.and_then(|l| l.id) {
                delta = Delta::Promote { id: l };
            }
        }
        let (op, did) = (
            delta.op(),
            match &delta {
                Delta::Promote { id } => Some(*id),
                _ => did,
            },
        );
        // M53.4: add one, cut one. Near the cap, an add (or a promotion,
        // which shows here too) retires a lesson this scope sees: the
        // reflector's pick, else the oldest no pass has kept since.
        let mut cut = None;
        if full && matches!(delta, Delta::Add { .. } | Delta::Promote { .. }) {
            let mine = |n: u32| lessons.iter().any(|l| l.id == Some(n)) && Some(n) != did;
            cut = match pick.filter(|n| mine(*n)) {
                Some(n) => Some((n, "the reflector's pick")),
                None => lessons
                    .iter()
                    .filter_map(|l| l.id)
                    .filter(|n| mine(*n))
                    .min_by_key(|n| (state.touched.get(n).copied().unwrap_or(0), *n))
                    .map(|n| (n, "the oldest lesson no pass kept since")),
            };
            if cut.is_none() && lessons.len() >= opts.max_bullets {
                let why = format!(
                    "the playbook is full ({} lessons here, max_bullets {}) and holds no lesson the pass may retire",
                    lessons.len(),
                    opts.max_bullets
                );
                reject(rec, dir, &ep.key, op, did, dtext, why);
                note(rec, ep, format!("rejected {op}"));
                cursor = cursor.max(ep.at);
                dir.save_pass(rec)?;
                continue;
            }
        }
        let new_id = last_id + 1;
        let mut gate_reason = String::from("retire is not gated");
        if !matches!(delta, Delta::Retire { .. }) {
            let probed = env.gate.check().is_none();
            if let Some(why) = meter.exceeded() {
                stop = Some(Stop::Budget(why));
                skip_episodes(rec, &all[i..]);
                break;
            }
            let mut candidate = pb.clone();
            let tried = candidate
                .apply_in(&delta, new_id, scope)
                .and_then(|_| match cut {
                    Some((n, _)) => candidate.apply(&Delta::Retire { id: n }, 0).map(|_| ()),
                    None => Ok(()),
                });
            if let Err(e) = tried {
                reject(rec, dir, &ep.key, op, did, dtext, e);
                cursor = cursor.max(ep.at);
                continue;
            }
            let block = prompt_block_for(
                &candidate.render(),
                scope,
                opts.max_bullets,
                opts.max_prompt_chars,
            );
            // M53.1: the control run sees the playbook as it stands.
            let current =
                prompt_block_for(&pb.render(), scope, opts.max_bullets, opts.max_prompt_chars);
            gate_n += 1;
            let sink = |origin: &str| {
                LearnSink::new(
                    env.ledger.clone(),
                    meter.clone(),
                    env.price.clone(),
                    format!("{origin}:{gate_n}"),
                )
            };
            dir.log(
                &id,
                if probed { "probe" } else { "gate" },
                json!({ "episode": ep.key, "n": gate_n, "op": op }),
            );
            let run = GateRun {
                n: gate_n,
                episode: ep,
                playbook_block: block.text.as_deref(),
                skill: None,
                sink: sink("gate"),
                meter: meter.clone(),
                pass_dir: &pass_dir,
                control: false,
            };
            let control = GateRun {
                playbook_block: current.text.as_deref(),
                sink: sink("control"),
                control: true,
                ..run.clone()
            };
            let verdict = contrast_gate(env, meter, &id, ep, run, control).await;
            if !verdict.passed {
                let budget = meter.exceeded();
                reject(rec, dir, &ep.key, op, did, dtext, verdict.reason);
                note(
                    rec,
                    ep,
                    format!(
                        "rejected {op} at the {}",
                        if probed { "probe" } else { "gate" }
                    ),
                );
                if let Some(why) = budget {
                    // Not finished: the cursor stays before it.
                    stop = Some(Stop::Budget(why));
                    skip_episodes(rec, &all[i + 1..]);
                    break;
                }
                cursor = cursor.max(ep.at);
                dir.save_pass(rec)?;
                continue;
            }
            gate_reason = verdict.reason;
        }
        let applied = match pb.apply_in(&delta, new_id, scope) {
            Ok(a) => a,
            Err(e) => {
                reject(rec, dir, &ep.key, op, did, dtext, e);
                cursor = cursor.max(ep.at);
                continue;
            }
        };
        let cut_applied = match cut {
            Some((n, why)) => pb.apply(&Delta::Retire { id: n }, 0).ok().map(|a| (a, why)),
            None => None,
        };
        if matches!(delta, Delta::Add { .. }) {
            last_id = new_id;
        }
        if matches!(delta, Delta::Retire { .. }) {
            state.touched.remove(&applied.id);
        } else {
            state
                .touched
                .insert(applied.id, chrono::Utc::now().timestamp());
        }
        if let Some((a, _)) = &cut_applied {
            state.touched.remove(&a.id);
        }
        dir.write_playbook(&pb.render())?;
        // Ids are never reused, even if the pass dies right here.
        state.last_id = last_id;
        dir.save_state(&state)?;
        dir.log(
            &id,
            "change",
            json!({ "episode": ep.key, "applied": applied, "reason": reason,
                    "evidence": evidence }),
        );
        let made_room_for = format!("{} pb-{}", applied.op, applied.id);
        rec.changes.push(Change::Playbook {
            applied,
            reason,
            episode: ep.key.clone(),
            gate: gate_reason.clone(),
            evidence: evidence.clone(),
        });
        if let Some((a, why)) = cut_applied {
            dir.log(
                &id,
                "change",
                json!({ "episode": ep.key, "applied": a, "reason": "add one, cut one" }),
            );
            rec.changes.push(Change::Playbook {
                applied: a,
                reason: format!("{MADE_ROOM}{made_room_for} (add one, cut one: {why})"),
                episode: ep.key.clone(),
                gate: gate_reason,
                evidence,
            });
        }
        note(rec, ep, format!("{op} kept"));
        cursor = cursor.max(ep.at);
        rec.spent = meter.spent();
        dir.save_pass(rec)?;
    }

    // Memory consolidation.
    if stop.is_none() {
        match clusters(env) {
            Err(e) => rec
                .notes
                .push(format!("memory consolidation skipped: {e:#}")),
            Ok(cl) => {
                if cl.len() > opts.max_clusters {
                    rec.notes.push(format!(
                        "{} more memory cluster(s) past max_clusters, left for the next pass",
                        cl.len() - opts.max_clusters
                    ));
                }
                let cl: Vec<_> = cl.into_iter().take(opts.max_clusters).collect();
                for (k, facts) in cl.iter().enumerate() {
                    if let Some(why) = meter.exceeded() {
                        stop = Some(Stop::Budget(why));
                        for f in &cl[k..] {
                            rec.skipped
                                .push(format!("{}: budget reached", cluster_name(f)));
                        }
                        break;
                    }
                    let source = cluster_name(facts);
                    let text = match ask(
                        env,
                        meter,
                        &id,
                        "consolidate",
                        consolidate::SYSTEM,
                        consolidate::user_message(facts),
                    )
                    .await
                    {
                        Ok(t) => {
                            errors = 0;
                            t
                        }
                        Err(e) => {
                            errors += 1;
                            rec.skipped
                                .push(format!("{source}: the consolidation call failed: {e}"));
                            if errors >= MAX_ERRORS_IN_ROW {
                                stop = Some(Stop::Errors(format!(
                                    "{MAX_ERRORS_IN_ROW} model calls failed in a row; last: {e}"
                                )));
                                break;
                            }
                            continue;
                        }
                    };
                    match consolidate::parse(&text) {
                        Err(why) => reject(rec, dir, &source, "merge", None, None, why),
                        Ok(consolidate::Decision::Keep { reason }) => {
                            rec.notes.push(format!("{source} kept apart: {reason}"))
                        }
                        Ok(consolidate::Decision::Merge { content, reason }) => {
                            match merge(env, facts, &content) {
                                Err(why) => {
                                    reject(rec, dir, &source, "merge", None, Some(content), why)
                                }
                                Ok((new_id, created, replaced)) => {
                                    dir.log(
                                        &id,
                                        "change",
                                        json!({ "memory": new_id, "replaced": replaced,
                                                "created": created }),
                                    );
                                    rec.changes.push(Change::Memory {
                                        new_id,
                                        created,
                                        replaced,
                                        content,
                                        reason,
                                    });
                                }
                            }
                        }
                    }
                    rec.spent = meter.spent();
                    dir.save_pass(rec)?;
                }
            }
        }
    }

    rec.status = match &stop {
        None => DONE.into(),
        Some(Stop::Budget(why)) => {
            rec.notes.push(format!("stopped: {why}"));
            STOPPED_BUDGET.into()
        }
        Some(Stop::Errors(why)) => {
            rec.notes.push(format!("stopped: {why}"));
            STOPPED_ERRORS.into()
        }
    };
    if let Some(at) = first_failed {
        cursor = cursor.min(at - 1).max(rec.cursor_before);
    }
    rec.cursor_after = cursor;
    state.cursor = cursor;
    state.last_id = last_id;
    dir.save_state(&state)?;
    Ok(())
}

fn cluster_name(facts: &[Memory]) -> String {
    let ids: Vec<String> = facts.iter().map(|f| format!("#{}", f.id)).collect();
    format!("memory {}", ids.join(", "))
}

/// Applies one merge through M15's UPDATE. Returns the live fact's id,
/// whether the pass created it, and the ids it superseded.
fn merge(env: &Env, facts: &[Memory], content: &str) -> Result<(i64, bool, Vec<i64>), String> {
    let db = env
        .memory_db
        .as_ref()
        .ok_or_else(|| "no memory database".to_string())?;
    let store = MemoryStore::open(db).map_err(|e| e.to_string())?;
    for f in facts {
        match store.get(f.id).map_err(|e| e.to_string())? {
            Some(m) if m.superseded_by.is_none() => {}
            _ => return Err(format!("#{} changed since the cluster was read", f.id)),
        }
    }
    let ids: Vec<i64> = facts.iter().map(|f| f.id).collect();
    let before = store.max_id().map_err(|e| e.to_string())?;
    let ins = store
        .insert(content, &[], &ids)
        .map_err(|e| e.to_string())?;
    if ins.replaced.is_empty() {
        return Err(format!(
            "the merged fact matches #{}, outside the cluster; nothing changed",
            ins.id
        ));
    }
    Ok((ins.id, ins.id > before, ins.replaced))
}

fn note(rec: &mut PassRecord, ep: &Episode, outcome: String) {
    rec.episodes.push(EpisodeNote {
        key: ep.key.clone(),
        label: ep.label.clone(),
        outcome,
    });
}

fn skip_episodes(rec: &mut PassRecord, eps: &[Episode]) {
    for e in eps {
        rec.skipped.push(format!(
            "{} ({}): budget or errors stopped the pass",
            e.label, e.key
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn reject(
    rec: &mut PassRecord,
    dir: &LearnDir,
    source: &str,
    op: &str,
    id: Option<u32>,
    text: Option<String>,
    reason: String,
) {
    dir.log(
        &rec.id,
        "reject",
        json!({ "source": source, "op": op, "id": id, "text": text, "reason": reason }),
    );
    rec.rejected.push(Rejected {
        source: source.into(),
        op: op.into(),
        id,
        text,
        reason,
    });
}

/// `changelog.md`: the journal, for people.
pub fn changelog(p: &PassRecord, diff: &str) -> String {
    let mut s = format!("# Learning pass {}\n\n", p.id);
    s.push_str(&format!(
        "- status: {}\n- trigger: {}\n- started: {}\n",
        p.status, p.trigger, p.started_at
    ));
    if let Some(f) = &p.finished_at {
        s.push_str(&format!("- finished: {f}\n"));
    }
    s.push_str(&format!(
        "- spent: ${:.4}, {} tokens, {} calls (caps: ${:.2}/pass, ${:.2}/day, {} tokens/pass, {} tokens/day)\n",
        p.spent.usd,
        p.spent.tokens,
        p.spent.calls,
        p.caps.usd_per_pass,
        p.caps.usd_per_day,
        p.caps.tokens_per_pass,
        p.caps.tokens_per_day
    ));
    s.push_str(&format!(
        "- check: {}\n",
        p.check
            .as_deref()
            .map_or("none".into(), |c| format!("`{c}`"))
    ));
    let section = |s: &mut String, title: &str, lines: Vec<String>| {
        if !lines.is_empty() {
            s.push_str(&format!("\n## {title}\n\n"));
            for l in lines {
                s.push_str(&format!("- {l}\n"));
            }
        }
    };
    section(
        &mut s,
        "Episodes",
        p.episodes
            .iter()
            .map(|e| format!("{} ({}): {}", e.label, e.key, e.outcome))
            .collect(),
    );
    section(
        &mut s,
        "Changes",
        p.changes
            .iter()
            .map(|c| match c {
                Change::Playbook {
                    applied,
                    reason,
                    episode,
                    gate,
                    evidence,
                } => {
                    let text = match (&applied.old, &applied.new) {
                        (Some(o), Some(n)) => format!("{o:?} → {n:?}"),
                        (None, Some(n)) => format!("{n:?}"),
                        (Some(o), None) => format!("{o:?}"),
                        (None, None) => String::new(),
                    };
                    let cited = if evidence.is_empty() {
                        String::new()
                    } else {
                        format!("; evidence: {evidence:?}")
                    };
                    format!(
                        "playbook {} pb-{}: {text} — {reason} (from {episode}; gate: {gate}{cited})",
                        applied.op, applied.id
                    )
                }
                Change::Memory {
                    new_id,
                    created,
                    replaced,
                    content,
                    reason,
                } => format!(
                    "memory: {} merged into #{new_id}{}: {content:?} — {reason}",
                    replaced
                        .iter()
                        .map(|i| format!("#{i}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    if *created { " (new)" } else { "" }
                ),
                Change::Skill {
                    name,
                    description,
                    op,
                    scripts,
                    reason,
                    episode,
                    gate,
                    evidence,
                    notes,
                    ..
                } => {
                    let with = if scripts.is_empty() {
                        String::new()
                    } else {
                        format!(" with {}", scripts.join(", "))
                    };
                    let cited = if evidence.is_empty() {
                        String::new()
                    } else {
                        format!("; evidence: {evidence:?}")
                    };
                    let noted = if notes.is_empty() {
                        String::new()
                    } else {
                        format!("; {}", notes.join("; "))
                    };
                    format!(
                        "skill {op} `{name}`{with}: {description:?} — {reason} (from {episode}; gate: {gate}{cited}{noted})"
                    )
                }
            })
            .collect(),
    );
    section(
        &mut s,
        "Rejected",
        p.rejected
            .iter()
            .map(|r| {
                let what = match (&r.id, &r.text) {
                    (Some(i), Some(t)) => format!(" pb-{i} {t:?}"),
                    (Some(i), None) => format!(" pb-{i}"),
                    (None, Some(t)) => format!(" {t:?}"),
                    (None, None) => String::new(),
                };
                format!("{}{what} (from {}): {}", r.op, r.source, r.reason)
            })
            .collect(),
    );
    section(&mut s, "Skipped", p.skipped.clone());
    section(&mut s, "Notes", p.notes.clone());
    if !diff.is_empty() {
        s.push_str(&format!("\n## Playbook diff\n\n```diff\n{diff}```\n"));
    }
    if !p.undone.is_empty() {
        s.push_str("\n## Undone one by one\n\n");
        for u in &p.undone {
            s.push_str(&format!(
                "- change {} at {}: {}\n",
                u.change,
                u.at,
                u.undone.join("; ")
            ));
        }
    }
    if let Some(r) = &p.reverted {
        s.push_str(&format!("\n## Reverted {}\n\n", r.at));
        for u in &r.undone {
            s.push_str(&format!("- undone: {u}\n"));
        }
        for u in &r.not_undone {
            s.push_str(&format!("- not undone: {u}\n"));
        }
    }
    s
}

/// The id `last` stands for: the newest pass with changes not yet reverted.
pub fn resolve(dir: &LearnDir, id: &str) -> Result<String> {
    if id != "last" {
        return Ok(id.to_string());
    }
    dir.passes()
        .into_iter()
        .rev()
        .find(|p| p.changes.len() > p.undone.len() && p.status != REVERTED)
        .map(|p| p.id)
        .ok_or_else(|| anyhow::anyhow!("no learning pass with changes to revert"))
}

/// Undoes a pass: its playbook changes (the whole file when nothing touched
/// it since, else line by line) and its memory merges, newest first.
/// M53.5: changes already undone on their own are left out.
pub fn revert(dir: &LearnDir, id: &str, memory_db: Option<&std::path::Path>) -> Result<PassRecord> {
    let _lock = dir.lock("revert")?;
    let id = resolve(dir, id)?;
    let mut p = dir.load_pass(&id)?;
    if p.status == REVERTED {
        bail!("pass {id} is already reverted");
    }
    if p.status == RUNNING {
        bail!("pass {id} is still running");
    }
    let (mut undone, mut not_undone) = (Vec::new(), Vec::new());
    let open: Vec<&Change> = p
        .changes
        .iter()
        .enumerate()
        .filter(|(i, _)| !p.is_undone(i + 1))
        .map(|(_, c)| c)
        .collect();
    let applied: Vec<_> = open
        .iter()
        .filter_map(|c| match c {
            Change::Playbook { applied, .. } => Some(applied.clone()),
            _ => None,
        })
        .collect();
    if !applied.is_empty() {
        let current = dir.read_playbook()?;
        let after = dir.read_pass_file(&id, "playbook.after.md");
        let before = dir.read_pass_file(&id, "playbook.before.md");
        match (after, before) {
            (Some(after), Some(before)) if after == current && p.undone.is_empty() => {
                dir.write_playbook(&before)?;
                undone.push("playbook restored to playbook.before.md".into());
            }
            _ => {
                let mut pb = Playbook::parse(&current);
                let mut any = false;
                for a in applied.iter().rev() {
                    match pb.invert(a) {
                        Ok(()) => {
                            any = true;
                            undone.push(format!("playbook {} pb-{} undone", a.op, a.id));
                        }
                        Err(e) => not_undone.push(format!("playbook {} pb-{}: {e}", a.op, a.id)),
                    }
                }
                if any {
                    dir.write_playbook(&pb.render())?;
                }
            }
        }
    }
    // M53.3: learned skills, newest first.
    for c in open.iter().rev() {
        if matches!(c, Change::Skill { .. }) {
            match undo_one(dir, &id, c, None) {
                Ok(done) => undone.push(done),
                Err(e) => not_undone.push(e),
            }
        }
    }
    let merges: Vec<_> = open
        .iter()
        .filter(|c| matches!(c, Change::Memory { .. }))
        .collect();
    if !merges.is_empty() {
        match memory_db.map(MemoryStore::open) {
            None => not_undone.push("memory merges: no memory database".into()),
            Some(Err(e)) => not_undone.push(format!("memory merges: {e}")),
            Some(Ok(store)) => {
                for c in merges.iter().rev() {
                    match undo_one(dir, &id, c, Some(&store)) {
                        Ok(done) => undone.push(done),
                        Err(e) => not_undone.push(e),
                    }
                }
            }
        }
    }
    p.status = REVERTED.into();
    p.reverted = Some(RevertNote {
        at: now(),
        undone,
        not_undone,
    });
    dir.save_pass(&p)?;
    let diff = dir.read_pass_file(&id, "playbook.diff").unwrap_or_default();
    dir.write_pass_file(&id, "changelog.md", &changelog(&p, &diff))?;
    dir.log(&id, "revert", json!(p.reverted));
    Ok(p)
}

/// Undoes one change of pass `id`, if it still stands as the pass left it.
/// A memory merge needs `store`.
fn undo_one(
    dir: &LearnDir,
    id: &str,
    c: &Change,
    store: Option<&MemoryStore>,
) -> Result<String, String> {
    match c {
        Change::Playbook { applied, .. } => {
            undo_playbook(dir, &[applied]).map(|done| done.join("; "))
        }
        Change::Skill {
            name,
            digest,
            backup,
            previous,
            ..
        } => crate::skills::undo(dir, id, name, digest, backup.as_deref(), previous.as_ref()),
        Change::Memory {
            new_id,
            created,
            replaced,
            ..
        } => {
            let store = store.ok_or("memory merges: no memory database")?;
            store
                .undo_update(*new_id, replaced, *created)
                .map(|_| format!("memory merge into #{new_id} undone"))
                .map_err(|e| format!("memory merge into #{new_id}: {e}"))
        }
    }
}

/// Inverts `applied`, in that order, and writes the playbook only when
/// every one still applies.
fn undo_playbook(
    dir: &LearnDir,
    applied: &[&crate::playbook::Applied],
) -> Result<Vec<String>, String> {
    let current = dir.read_playbook().map_err(|e| format!("{e:#}"))?;
    let mut pb = Playbook::parse(&current);
    let mut done = Vec::new();
    for a in applied {
        pb.invert(a)
            .map_err(|e| format!("playbook {} pb-{}: {e}", a.op, a.id))?;
        done.push(format!("playbook {} pb-{} undone", a.op, a.id));
    }
    dir.write_playbook(&pb.render())
        .map_err(|e| format!("the playbook: {e:#}"))?;
    Ok(done)
}

/// M53.5: undoes change `n` (from 1) of pass `id` on its own (`learn
/// revert --change N`, the owner's Undo button): an add that retired a
/// lesson to make room takes that retire back too, or neither is undone.
/// The pass stays; a later pass revert skips what was undone here. What
/// couldn't be undone is said in `not_undone` and left for another try.
pub fn revert_change(
    dir: &LearnDir,
    id: &str,
    n: usize,
    memory_db: Option<&std::path::Path>,
) -> Result<ChangeRevert> {
    let _lock = dir.lock("revert")?;
    let id = resolve(dir, id)?;
    let mut p = dir.load_pass(&id)?;
    if p.status == REVERTED {
        bail!("pass {id} is already reverted");
    }
    if p.status == RUNNING {
        bail!("pass {id} is still running");
    }
    let Some(c) = n.checked_sub(1).and_then(|i| p.changes.get(i)) else {
        bail!("pass {id} has no change {n} (it has {})", p.changes.len());
    };
    if p.is_undone(n) {
        bail!("change {n} of pass {id} is already undone");
    }
    let at = now();
    let mut out = ChangeRevert {
        change: n,
        at: at.clone(),
        undone: vec![],
        not_undone: vec![],
    };
    let mut kept: Vec<usize> = Vec::new();
    match c {
        Change::Playbook { applied, .. } => {
            // Newest first, as a pass revert goes: the retire, then the add.
            let mut todo = vec![(n, applied)];
            if let Some(Change::Playbook {
                applied: cut,
                reason,
                ..
            }) = p.changes.get(n)
            {
                if reason.starts_with(&made_room(applied)) && !p.is_undone(n + 1) {
                    todo.insert(0, (n + 1, cut));
                }
            }
            let list: Vec<_> = todo.iter().map(|(_, a)| *a).collect();
            match undo_playbook(dir, &list) {
                Ok(done) => {
                    out.undone = done;
                    kept = todo.iter().map(|(k, _)| *k).collect();
                }
                Err(e) => out.not_undone.push(e),
            }
        }
        _ => {
            let store = match (c, memory_db) {
                (Change::Memory { .. }, Some(db)) => Some(MemoryStore::open(db)?),
                _ => None,
            };
            match undo_one(dir, &id, c, store.as_ref()) {
                Ok(done) => {
                    out.undone.push(done);
                    kept.push(n);
                }
                Err(e) => out.not_undone.push(e),
            }
        }
    }
    kept.sort_unstable();
    for k in kept {
        p.undone.push(ChangeRevert {
            change: k,
            at: at.clone(),
            undone: if k == n {
                out.undone.clone()
            } else {
                vec![format!("with change {n}")]
            },
            not_undone: vec![],
        });
    }
    dir.save_pass(&p)?;
    let diff = dir.read_pass_file(&id, "playbook.diff").unwrap_or_default();
    dir.write_pass_file(&id, "changelog.md", &changelog(&p, &diff))?;
    dir.log(&id, "revert_change", json!(out));
    Ok(out)
}

mod skill;
#[cfg(test)]
mod tests;
