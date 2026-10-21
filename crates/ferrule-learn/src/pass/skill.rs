//! M53.3: one `skill` proposal through the pass: shape, a draft on disk,
//! the validators, the gate (or the probe and the judge), then kept in
//! `<data>/learned-skills/` and journaled like a lesson.

use super::{judged, note, reject, Env};
use crate::budget::{LearnSink, Meter};
use crate::episode::Episode;
use crate::files::{Change, PassRecord};
use crate::gate::{GateRun, GateVerdict};
use crate::skills::{self, SkillDraft};
use anyhow::Result;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

/// What the pass loop does next.
pub(super) enum Flow {
    /// The episode is done with (kept or rejected).
    Next,
    /// The budget ran out before the gate: this episode stays for later.
    BudgetBefore(String),
    /// The budget ran out during the gate: rejected, and the episode
    /// stays for later.
    BudgetAfter(String),
}

/// What the skill path borrows from the pass loop.
pub(super) struct Ctx<'a> {
    pub env: &'a Env,
    pub meter: &'a Arc<Meter>,
    pub pass_dir: &'a Path,
    pub gate_n: &'a mut usize,
    pub episode: &'a Episode,
    pub tail: &'a str,
    /// The playbook as it stands, for the gate run's prompt.
    pub playbook_block: Option<&'a str>,
    /// M53.4: the episode's scope, where the skill applies.
    pub scope: Option<&'a str>,
}

const OP: &str = "skill";

pub(super) async fn delta(
    cx: Ctx<'_>,
    rec: &mut PassRecord,
    draft: SkillDraft,
    reason: String,
    evidence: String,
) -> Result<Flow> {
    let env = cx.env;
    let ep = cx.episode;
    let name = draft.name.clone();
    let rejected = |rec: &mut PassRecord, why: String| {
        reject(rec, &env.dir, &ep.key, OP, None, Some(name.clone()), why);
        note(rec, ep, "rejected skill".into());
    };
    if let Err(why) = draft.shape() {
        rejected(rec, why);
        return Ok(Flow::Next);
    }
    // The draft lives outside `data/learn`, which the sandbox hides: the
    // parse checks run in it.
    let scratch =
        std::env::temp_dir().join(format!("ferrule-learn-{}-skill-{}", rec.id, *cx.gate_n + 1));
    let flow = drafted(cx, rec, &draft, &scratch, reason, evidence).await;
    let _ = std::fs::remove_dir_all(&scratch);
    match flow {
        Ok(f) => Ok(f),
        Err(why) => {
            rejected(rec, why);
            Ok(Flow::Next)
        }
    }
}

/// From a written draft to a kept skill; `Err` is a rejection's reason.
async fn drafted(
    cx: Ctx<'_>,
    rec: &mut PassRecord,
    draft: &SkillDraft,
    scratch: &Path,
    reason: String,
    evidence: String,
) -> Result<Flow, String> {
    let (env, meter, ep) = (cx.env, cx.meter, cx.episode);
    let dir = draft.write(scratch)?;
    let notes = skills::validate(&dir, draft, env.gate.sandbox().as_ref()).await?;
    if let Some(why) = meter.exceeded() {
        return Ok(Flow::BudgetBefore(why));
    }
    let probed = env.gate.check().is_none();
    *cx.gate_n += 1;
    let n = *cx.gate_n;
    let sink = LearnSink::new(
        env.ledger.clone(),
        meter.clone(),
        env.price.clone(),
        format!("gate:{n}"),
    );
    env.dir.log(
        &rec.id,
        if probed { "probe" } else { "gate" },
        json!({ "episode": ep.key, "n": n, "op": OP, "skill": draft.name }),
    );
    let run = GateRun {
        n,
        episode: ep,
        playbook_block: cx.playbook_block,
        skill: Some(&dir),
        sink,
        meter: meter.clone(),
        pass_dir: cx.pass_dir,
    };
    let verdict: GateVerdict = if probed {
        judged(env, meter, &rec.id, ep, cx.tail, &draft.summary(), run).await
    } else {
        env.gate.run(run).await
    };
    if !verdict.passed {
        let budget = meter.exceeded();
        reject(
            rec,
            &env.dir,
            &ep.key,
            OP,
            None,
            Some(draft.name.clone()),
            verdict.reason,
        );
        note(
            rec,
            ep,
            format!(
                "rejected skill at the {}",
                if probed { "probe" } else { "gate" }
            ),
        );
        return Ok(match budget {
            Some(why) => Flow::BudgetAfter(why),
            None => Flow::Next,
        });
    }
    let kept = skills::keep(
        &env.dir,
        &rec.id,
        rec.changes.len(),
        &dir,
        &draft.name,
        cx.scope,
    )?;
    let op = if kept.promoted {
        "promote"
    } else if kept.backup.is_some() || kept.previous.is_some() {
        "replace"
    } else {
        "add"
    };
    env.dir.log(
        &rec.id,
        "change",
        json!({ "episode": ep.key, "skill": draft.name, "op": op, "digest": kept.digest,
                "reason": reason, "evidence": evidence }),
    );
    rec.changes.push(Change::Skill {
        name: draft.name.clone(),
        description: draft.description.trim().to_string(),
        op: op.into(),
        digest: kept.digest,
        scope: kept.scope,
        backup: kept.backup,
        previous: kept.previous,
        scripts: draft.scripts.keys().cloned().collect(),
        reason,
        episode: ep.key.clone(),
        gate: verdict.reason,
        evidence,
        notes,
    });
    note(rec, ep, format!("skill {op} kept"));
    Ok(Flow::Next)
}
