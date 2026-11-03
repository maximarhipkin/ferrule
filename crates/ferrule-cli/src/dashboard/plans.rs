//! M66: plan mode's plans on the page (docs/m19-trust-cost.md §8): the
//! list, and Approve and Reject, each doing what its CLI command or the
//! chat's answer does. A plan a chat's `/plan` made has its question
//! waiting in the owner's chat: the page answers that question, so the
//! chat's run goes on exactly as on a `yes` there. A plan nobody is asked
//! about (`ferrule run --plan` without a terminal) runs as `ferrule plan
//! approve <id>`, the CLI itself.

use super::api::{
    arg, bad, by, child_env, child_id, confirmed, missing, need, ok, still_running, Answer,
};
use super::Ctx;
use ferrule_gateway::health::clip;
use ferrule_trust::{Plan, PlanStatus, PlanStore};
use serde_json::{json, Value};

/// The most plans the page lists, newest first.
const SHOWN: usize = 50;

/// Held from "is one running?" to "it's started", so two clicks (or two
/// tabs) never start two runs of one plan.
static STARTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn store(ctx: &Ctx) -> Option<PlanStore> {
    ctx.data.as_ref().map(|d| PlanStore::new(d.join("plans")))
}

fn status_name(s: PlanStatus) -> &'static str {
    match s {
        PlanStatus::Proposed => "proposed",
        PlanStatus::Approved => "approved",
        PlanStatus::Rejected => "rejected",
        PlanStatus::Executed => "executed",
    }
}

/// What the approvals' inbox calls a plan's question (`plan.rs`).
fn subject(id: &str) -> String {
    format!("plan {id}")
}

/// The code of the question waiting on plan `id`, if one is.
fn waiting(ctx: &Ctx, id: &str) -> Option<String> {
    let what = subject(id);
    ctx.hub
        .as_ref()?
        .approvals()
        .list()
        .into_iter()
        .find(|w| w.what == what)
        .map(|w| w.code)
}

/// Where each plan was asked for, from the audit log's `plan_proposed`.
fn asked_in(ctx: &Ctx, plans: &[Plan]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(hub) = &ctx.hub else {
        return out;
    };
    let since = plans
        .iter()
        .filter_map(|p| chrono::DateTime::parse_from_rfc3339(&p.created).ok())
        .min()
        .map(|t| t.with_timezone(&chrono::Utc) - chrono::Duration::minutes(1));
    for e in hub.audit().read(since).unwrap_or_default() {
        if e.event != "plan_proposed" {
            continue;
        }
        let Some(id) = e.detail["plan"].as_str() else {
            continue;
        };
        let route = e.detail["route"].as_str().unwrap_or("");
        let place = match (route, &e.detail["chat"]) {
            ("cli", _) => "the terminal (`ferrule run --plan`)".to_string(),
            (channel, Value::Null) => format!("{channel} chat"),
            (channel, chat) => format!(
                "{channel} chat {}",
                chat.as_str()
                    .map_or_else(|| chat.to_string(), str::to_string)
            ),
        };
        out.insert(id.to_string(), place);
    }
    out
}

/// `GET /api/plans`: every plan, newest first (at most 50).
pub fn list(ctx: &Ctx) -> Answer {
    let Some(store) = store(ctx) else {
        return missing("the data directory");
    };
    let plans: Vec<Plan> = store.list().into_iter().take(SHOWN).collect();
    let places = asked_in(ctx, &plans);
    let rows: Vec<Value> = plans
        .iter()
        .map(|p| {
            let chat = places.get(&p.id).cloned().unwrap_or_else(|| {
                if p.session.starts_with("plan__") {
                    "a chat's /plan".to_string()
                } else {
                    "the terminal (`ferrule run --plan`)".to_string()
                }
            });
            json!({
                "id": p.id,
                "chat": chat,
                "created": p.created,
                "status": status_name(p.status),
                "text": ctx.redactor.redact(&p.text),
                "task": clip(&ctx.redactor.redact(&p.task), 300),
                "workspace": p.workspace,
                // A question in the owner's chat waits for it.
                "waiting": p.status == PlanStatus::Proposed && waiting(ctx, &p.id).is_some(),
            })
        })
        .collect();
    ok(json!({ "plans": rows }))
}

/// The plan, checked: it exists, its text is as proposed, it's undecided.
fn proposed(ctx: &Ctx, id: &str) -> Result<Plan, Answer> {
    let store = store(ctx).ok_or_else(|| missing("the data directory"))?;
    let plan = store
        .get(id)
        .map_err(|e| bad(404, format!("{}.", ctx.redactor.redact(&e))))?;
    if plan.status != PlanStatus::Proposed {
        return Err(bad(
            409,
            format!(
                "Plan {id} is already {}; only a proposed plan can be approved or rejected.",
                status_name(plan.status)
            ),
        ));
    }
    Ok(plan)
}

/// The page's answer to a plan's question, audited as the inbox's is.
fn answer_question(ctx: &Ctx, code: &str, allow: bool, why: &str) -> Result<(), Answer> {
    let hub = ctx.hub.as_ref().ok_or_else(|| missing("the trust hub"))?;
    hub.approvals()
        .decide(code, allow, why)
        .ok_or_else(|| bad(409, "That plan's question was just answered or withdrawn."))?;
    hub.audit().record(
        chrono::Utc::now(),
        if allow {
            "approval_allowed"
        } else {
            "approval_refused"
        },
        None,
        None,
        json!({ "by": by(), "code": code }),
    );
    Ok(())
}

/// `POST /api/plans/approve {id}`.
pub fn approve(ctx: &Ctx, body: &Value) -> Answer {
    let id = need!(arg(body, "id"));
    if !child_id(id) {
        return bad(400, "That isn't a plan id.");
    }
    let label = format!("ferrule plan approve {id}");
    let _one = STARTING.lock().unwrap_or_else(|e| e.into_inner());
    if still_running(ctx, &label) {
        return bad(
            409,
            format!("Plan {id} is already running from this page; its output is under Runs."),
        );
    }
    let plan = need!(proposed(ctx, id));
    if let Some(code) = waiting(ctx, id) {
        need!(answer_question(ctx, &code, true, ""));
        return ok(json!({
            "ok": true,
            "said": format!("Approved: plan {id} runs now, and its answer goes to the chat that asked for it."),
        }));
    }
    let Some(hub) = ctx.hub.clone() else {
        return missing("the trust hub");
    };
    need!(confirmed(
        body,
        format!(
            "Run plan {id} now, in {}? It runs as `ferrule plan approve {id}`; a gated command in it is refused, as there's no terminal to ask.",
            plan.workspace
        )
    ));
    // `--`: whatever the id, it is the id, never a flag.
    let args = vec!["plan".into(), "approve".into(), "--".into(), id.into()];
    let (who, done_label) = (by(), label.clone());
    let started = ctx
        .runs
        .start_then(label, args, child_env(ctx), move |run| {
            hub.audit().record(
                chrono::Utc::now(),
                "dashboard.plan.ran",
                None,
                None,
                json!({ "by": who, "line": done_label, "code": run.code() }),
            );
        });
    match started {
        Ok(run) => {
            if let Some(hub) = &ctx.hub {
                hub.audit().record(
                    chrono::Utc::now(),
                    "dashboard.plan.approve",
                    Some(&plan.session),
                    None,
                    json!({ "by": by(), "plan": id, "run": run.id }),
                );
            }
            ok(json!({
                "ok": true,
                "said": format!("Approved: plan {id} runs as `ferrule plan approve {id}`; its output is under Runs."),
                "run": run.id,
            }))
        }
        Err(e) => bad(
            500,
            format!("Plan {id} wasn't started: `ferrule plan approve` couldn't run ({e})."),
        ),
    }
}

/// `POST /api/plans/reject {id, why?}`.
pub fn reject(ctx: &Ctx, body: &Value) -> Answer {
    let id = need!(arg(body, "id"));
    if !child_id(id) {
        return bad(400, "That isn't a plan id.");
    }
    if still_running(ctx, &format!("ferrule plan approve {id}")) {
        return bad(
            409,
            format!("Plan {id} is running from this page already; stop it under Runs first."),
        );
    }
    need!(proposed(ctx, id));
    let why = body
        .get("why")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(|w| clip(w, 200))
        .unwrap_or_else(|| "rejected on the dashboard".to_string());
    if let Some(code) = waiting(ctx, id) {
        // The chat's run records the rejection, with these words.
        need!(answer_question(ctx, &code, false, &why));
        return ok(json!({
            "ok": true,
            "said": format!("Rejected: plan {id} won't run, and the chat that asked is told."),
        }));
    }
    let (Some(hub), Some(store)) = (&ctx.hub, store(ctx)) else {
        return missing("the trust hub");
    };
    match crate::plan::reject(hub, &store, id, &why) {
        Ok(()) => ok(json!({ "ok": true, "said": format!("Rejected: plan {id} won't run.") })),
        Err(e) => bad(
            500,
            format!(
                "Plan {id} wasn't rejected: {}",
                ctx.redactor.redact(&format!("{e:#}"))
            ),
        ),
    }
}
