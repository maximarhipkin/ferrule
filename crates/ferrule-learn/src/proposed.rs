//! "Proposed, not proven" (`docs/m16-learning-loop.md`, "Proposed
//! changes"): the owner's Keep lands a proposal exactly as the pass lands
//! a kept change (the same guards, add one cut one, a numbered change of
//! its pass that `learn revert --change N` and Undo take back); Drop
//! marks it rejected. Proven-bad changes are never proposed.

use crate::files::{
    Change, LearnDir, PassRecord, Proposal, Proposed, DROPPED, KEPT, PROPOSED, REVERTED, RUNNING,
};
use crate::pass::{changelog, choose_cut, learned, near_cap, Learned, MADE_ROOM};
use crate::playbook::{Delta, Playbook, TEMPLATE};
use crate::screen;
use anyhow::{anyhow, bail, Result};
use serde_json::json;

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// The pass and the index of its proposal `pid`, still waiting.
fn open(dir: &LearnDir, pass: &str, pid: &str) -> Result<(PassRecord, usize)> {
    let p = dir.load_pass(pass)?;
    if p.status == RUNNING {
        bail!("pass {pass} is still running");
    }
    if p.status == REVERTED {
        bail!("pass {pass} was reverted; its proposals went with it");
    }
    let k = p
        .proposed
        .iter()
        .position(|x| x.id == pid)
        .ok_or_else(|| anyhow!("pass {pass} has no proposal {pid}"))?;
    let status = &p.proposed[k].status;
    if status != PROPOSED {
        bail!("proposal {pid} of pass {pass} was {status} already");
    }
    Ok((p, k))
}

fn save(dir: &LearnDir, p: &PassRecord, event: &str, detail: serde_json::Value) -> Result<()> {
    dir.save_pass(p)?;
    let diff = dir
        .read_pass_file(&p.id, "playbook.diff")
        .unwrap_or_default();
    dir.write_pass_file(&p.id, "changelog.md", &changelog(p, &diff))?;
    dir.log(&p.id, event, detail);
    Ok(())
}

/// Keeps proposal `pid` of pass `pass`: it lands now. `max_bullets` is
/// `[learning] max_bullets`. Returns the "Learned" line, with the change
/// number its Undo names.
pub fn keep(dir: &LearnDir, pass: &str, pid: &str, max_bullets: usize) -> Result<Learned> {
    let _lock = dir.lock(&format!("keep {pass} {pid}"))?;
    let (mut p, k) = open(dir, pass, pid)?;
    let x = p.proposed[k].clone();
    let gate = format!("kept by the owner, not proven by the pass: {}", x.gate);
    let first = p.changes.len() + 1;
    match &x.change {
        Proposal::Lesson {
            op,
            target,
            text,
            retire,
        } => keep_lesson(
            dir,
            &mut p,
            &x,
            op,
            *target,
            text.as_deref(),
            *retire,
            max_bullets,
            &gate,
        )?,
        Proposal::Skill {
            name,
            description,
            dir: rel,
            scripts,
            notes,
        } => {
            let draft = dir.pass_dir(pass).join(rel);
            if !draft.join("SKILL.md").is_file() {
                bail!("the proposed draft of `{name}` is gone from the pass dir");
            }
            let kept =
                crate::skills::keep(dir, pass, p.changes.len(), &draft, name, x.scope.as_deref())
                    .map_err(|e| anyhow!(e))?;
            let op = if kept.promoted {
                "promote"
            } else if kept.backup.is_some() || kept.previous.is_some() {
                "replace"
            } else {
                "add"
            };
            p.changes.push(Change::Skill {
                name: name.clone(),
                description: description.clone(),
                op: op.into(),
                digest: kept.digest,
                scope: kept.scope,
                backup: kept.backup,
                previous: kept.previous,
                scripts: scripts.clone(),
                reason: x.reason.clone(),
                episode: x.episode.clone(),
                gate,
                evidence: x.evidence.clone(),
                notes: notes.clone(),
            });
            let _ = std::fs::remove_dir_all(&draft);
        }
    }
    let px = &mut p.proposed[k];
    px.status = KEPT.into();
    px.decided_at = Some(now());
    px.kept_as = Some(first);
    save(dir, &p, "keep", json!({ "proposal": pid, "change": first }))?;
    learned(&p)
        .into_iter()
        .find(|l| l.change == first)
        .ok_or_else(|| anyhow!("proposal {pid} was kept as change {first}"))
}

#[allow(clippy::too_many_arguments)]
fn keep_lesson(
    dir: &LearnDir,
    p: &mut PassRecord,
    x: &Proposed,
    op: &str,
    target: Option<u32>,
    text: Option<&str>,
    retire: Option<u32>,
    max_bullets: usize,
    gate: &str,
) -> Result<()> {
    let scope = x.scope.as_deref();
    let current = dir.read_playbook()?;
    let mut pb = Playbook::parse(if current.trim().is_empty() {
        TEMPLATE
    } else {
        &current
    });
    let lessons = pb.lessons_for(scope);
    let visible = |n: u32| lessons.iter().any(|l| l.id == Some(n));
    let delta = match (op, target, text) {
        ("add", _, Some(t)) => {
            screen::check(t, &lessons, None).map_err(|e| anyhow!(e))?;
            Delta::Add { text: t.into() }
        }
        ("edit", Some(n), Some(t)) if visible(n) => {
            screen::check(t, &lessons, Some(n)).map_err(|e| anyhow!(e))?;
            Delta::Edit {
                id: n,
                text: t.into(),
            }
        }
        ("promote", Some(n), _) if pb.lessons().iter().any(|l| l.id == Some(n)) => {
            Delta::Promote { id: n }
        }
        (_, Some(n), _) => bail!("the playbook has no lesson pb-{n} for it any more"),
        _ => bail!("the proposal is not a change this version can make ({op})"),
    };
    let mut state = dir.state()?;
    let cut = if lessons.len() >= near_cap(max_bullets)
        && matches!(delta, Delta::Add { .. } | Delta::Promote { .. })
    {
        let cut = choose_cut(&lessons, target, retire, &state, &dir.usage_stats());
        if cut.is_none() && lessons.len() >= max_bullets {
            bail!(
                "the playbook is full ({} lessons here, max_bullets {max_bullets}) and holds no lesson to retire for it",
                lessons.len()
            );
        }
        cut
    } else {
        None
    };
    let new_id = state.last_id.max(pb.max_id()) + 1;
    let applied = pb.apply_in(&delta, new_id, scope).map_err(|e| anyhow!(e))?;
    let cut_applied = match cut {
        Some((n, why)) => Some((
            pb.apply(&Delta::Retire { id: n }, 0)
                .map_err(|e| anyhow!(e))?,
            why,
        )),
        None => None,
    };
    if matches!(delta, Delta::Add { .. }) {
        state.last_id = new_id;
    }
    state
        .touched
        .insert(applied.id, chrono::Utc::now().timestamp());
    if let Some((a, _)) = &cut_applied {
        state.touched.remove(&a.id);
    }
    dir.write_playbook(&pb.render())?;
    dir.save_state(&state)?;
    let made_room_for = format!("{} pb-{}", applied.op, applied.id);
    p.changes.push(Change::Playbook {
        applied,
        reason: x.reason.clone(),
        episode: x.episode.clone(),
        gate: gate.into(),
        evidence: x.evidence.clone(),
    });
    if let Some((a, why)) = cut_applied {
        p.changes.push(Change::Playbook {
            applied: a,
            reason: format!("{MADE_ROOM}{made_room_for} (add one, cut one: {why})"),
            episode: x.episode.clone(),
            gate: gate.into(),
            evidence: x.evidence.clone(),
        });
    }
    Ok(())
}

/// Drops proposal `pid` of pass `pass`: it never lands. In words.
pub fn drop(dir: &LearnDir, pass: &str, pid: &str) -> Result<String> {
    let _lock = dir.lock(&format!("drop {pass} {pid}"))?;
    let (mut p, k) = open(dir, pass, pid)?;
    let x = &mut p.proposed[k];
    x.status = DROPPED.into();
    x.decided_at = Some(now());
    let what = match &x.change {
        Proposal::Lesson { op, text, .. } => {
            format!("the proposed {op} \"{}\"", text.as_deref().unwrap_or(""))
        }
        Proposal::Skill { name, dir: rel, .. } => {
            let _ = std::fs::remove_dir_all(dir.pass_dir(pass).join(rel));
            format!("the proposed skill `{name}`")
        }
    };
    save(dir, &p, "drop", json!({ "proposal": pid }))?;
    Ok(format!("Dropped {what}; it won't be used."))
}
