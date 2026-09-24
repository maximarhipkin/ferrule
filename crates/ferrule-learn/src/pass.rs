//! One learning pass, end to end (`docs/m16-learning-loop.md`): review the
//! episodes after the cursor, gate and apply playbook deltas, merge memory
//! clusters, and journal every step; plus the dry-run plan and revert.

use crate::budget::{row, Caps, LearnSink, Meter, PriceFn, Spent};
use crate::consolidate::{self, CLUSTER_JACCARD, CLUSTER_ROWS};
use crate::diff::unified;
use crate::episode::{render_tail, scan_sessions, Episode};
use crate::files::{
    Change, EpisodeNote, LearnDir, PassRecord, Rejected, RevertNote, State, DONE, REVERTED,
    RUNNING, STOPPED_BUDGET, STOPPED_ERRORS,
};
use crate::gate::{Gate, GateRun};
use crate::playbook::{prompt_block, Delta, Playbook, TEMPLATE};
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
    pub task_episodes: Vec<Episode>,
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
        max_output_tokens: Some(1024),
        temperature: Some(0.0),
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

/// Runs one pass. Takes the lock, so a second concurrent pass is refused.
pub async fn run_pass(opts: &Options, env: &Env) -> Result<PassRecord> {
    let dir = &env.dir;
    let id = dir.new_pass_id();
    let _lock = dir.lock(&format!("pass {id}"))?;
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
            "no check to gate on ([learning] check or [agent] verify_command): adds and edits are rejected"
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
    result?;
    Ok(rec)
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
        let lessons = pb.lessons();
        let full = lessons.len() >= opts.max_bullets;
        let answer = ask(
            env,
            meter,
            &id,
            "reflect",
            reflect::SYSTEM,
            reflect::user_message(&lessons, full, ep, &tail),
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
        let Proposal { delta, reason } = proposal;
        let Some(delta) = delta else {
            note(rec, ep, format!("no change: {reason}"));
            cursor = cursor.max(ep.at);
            dir.save_pass(rec)?;
            continue;
        };
        let (op, did, dtext) = (
            delta.op(),
            match &delta {
                Delta::Add { .. } => None,
                Delta::Edit { id, .. } | Delta::Retire { id } => Some(*id),
            },
            delta.text().map(str::to_string),
        );
        // Guards on the proposal itself.
        let guard = match &delta {
            Delta::Add { text } if full => Err(format!(
                "the playbook is full ({} lessons, max_bullets {}); {text:?} not added",
                lessons.len(),
                opts.max_bullets
            )),
            Delta::Add { text } => screen::check(text, &lessons, None),
            Delta::Edit { id: n, text } => match pb.get(*n) {
                None => Err(format!("there is no lesson pb-{n} to edit")),
                Some(_) => screen::check(text, &lessons, Some(*n)),
            },
            Delta::Retire { id: n } => match pb.get(*n) {
                None => Err(format!("there is no lesson pb-{n} to retire")),
                Some(_) => Ok(()),
            },
        };
        if let Err(why) = guard {
            reject(rec, dir, &ep.key, op, did, dtext, why);
            note(rec, ep, format!("rejected {op}"));
            cursor = cursor.max(ep.at);
            dir.save_pass(rec)?;
            continue;
        }
        let new_id = last_id + 1;
        let mut gate_reason = String::from("retire is not gated");
        if !matches!(delta, Delta::Retire { .. }) {
            if env.gate.check().is_none() {
                reject(
                    rec,
                    dir,
                    &ep.key,
                    op,
                    did,
                    dtext,
                    "no check to gate on".into(),
                );
                note(rec, ep, format!("rejected {op}"));
                cursor = cursor.max(ep.at);
                dir.save_pass(rec)?;
                continue;
            }
            if let Some(why) = meter.exceeded() {
                stop = Some(Stop::Budget(why));
                skip_episodes(rec, &all[i..]);
                break;
            }
            let mut candidate = pb.clone();
            if let Err(e) = candidate.apply(&delta, new_id) {
                reject(rec, dir, &ep.key, op, did, dtext, e);
                cursor = cursor.max(ep.at);
                continue;
            }
            let block = prompt_block(&candidate.render(), opts.max_bullets, opts.max_prompt_chars);
            gate_n += 1;
            let sink = LearnSink::new(
                env.ledger.clone(),
                meter.clone(),
                env.price.clone(),
                format!("gate:{gate_n}"),
            );
            dir.log(
                &id,
                "gate",
                json!({ "episode": ep.key, "n": gate_n, "op": op }),
            );
            let verdict = env
                .gate
                .run(GateRun {
                    n: gate_n,
                    episode: ep,
                    playbook_block: block.text.as_deref(),
                    sink,
                    meter: meter.clone(),
                    pass_dir: &pass_dir,
                })
                .await;
            if !verdict.passed {
                let budget = meter.exceeded();
                reject(rec, dir, &ep.key, op, did, dtext, verdict.reason);
                note(rec, ep, format!("rejected {op} at the gate"));
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
        let applied = match pb.apply(&delta, new_id) {
            Ok(a) => a,
            Err(e) => {
                reject(rec, dir, &ep.key, op, did, dtext, e);
                cursor = cursor.max(ep.at);
                continue;
            }
        };
        if matches!(delta, Delta::Add { .. }) {
            last_id = new_id;
        }
        dir.write_playbook(&pb.render())?;
        // Ids are never reused, even if the pass dies right here.
        state.last_id = last_id;
        dir.save_state(&state)?;
        dir.log(
            &id,
            "change",
            json!({ "episode": ep.key, "applied": applied, "reason": reason }),
        );
        rec.changes.push(Change::Playbook {
            applied,
            reason,
            episode: ep.key.clone(),
            gate: gate_reason,
        });
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
                } => {
                    let text = match (&applied.old, &applied.new) {
                        (Some(o), Some(n)) => format!("{o:?} → {n:?}"),
                        (None, Some(n)) => format!("{n:?}"),
                        (Some(o), None) => format!("{o:?}"),
                        (None, None) => String::new(),
                    };
                    format!(
                        "playbook {} pb-{}: {text} — {reason} (from {episode}; gate: {gate})",
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
        .find(|p| !p.changes.is_empty() && p.status != REVERTED)
        .map(|p| p.id)
        .ok_or_else(|| anyhow::anyhow!("no learning pass with changes to revert"))
}

/// Undoes a pass: its playbook changes (the whole file when nothing touched
/// it since, else line by line) and its memory merges, newest first.
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
    let applied: Vec<_> = p
        .changes
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
            (Some(after), Some(before)) if after == current => {
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
    let merges: Vec<_> = p
        .changes
        .iter()
        .filter_map(|c| match c {
            Change::Memory {
                new_id,
                created,
                replaced,
                ..
            } => Some((*new_id, *created, replaced.clone())),
            _ => None,
        })
        .collect();
    if !merges.is_empty() {
        match memory_db.map(MemoryStore::open) {
            None => not_undone.push("memory merges: no memory database".into()),
            Some(Err(e)) => not_undone.push(format!("memory merges: {e}")),
            Some(Ok(store)) => {
                for (new_id, created, replaced) in merges.iter().rev() {
                    match store.undo_update(*new_id, replaced, *created) {
                        Ok(_) => undone.push(format!("memory merge into #{new_id} undone")),
                        Err(e) => not_undone.push(format!("memory merge into #{new_id}: {e}")),
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

#[cfg(test)]
mod tests;
