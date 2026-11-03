//! M66: the conversations on disk (`GET /api/sessions`), as `ferrule
//! sessions` lists them: `<data>/sessions/<id>.jsonl`, newest first,
//! without the ones `/new` put aside. A goal loop the budget cut short
//! (`<id>.goal.json`, its judge not yet passed) resumes from here the way
//! the terminal resumes it: `ferrule run --resume <id>`, as a run the page
//! follows.

use super::api::{
    arg, bad, by, child_env, child_id, confirmed, missing, need, ok, still_running, Answer,
};
use super::Ctx;
use ferrule_gateway::health::clip;
use serde_json::{json, Value};
use std::io::BufRead;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// The most sessions the page lists.
const SHOWN: usize = 50;
/// The first message, as the list shows it.
const FIRST: usize = 119;
/// How long a resumed goal loop's round may run from the page.
pub const RESUME_LIMIT: Duration = Duration::from_secs(2 * 3600);

/// Held from "is it running?" to "it's started", so two clicks (or two
/// tabs) never start two rounds of one loop.
static STARTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where a session came from, from its id: `telegram__42` is that chat.
pub fn place_of(id: &str) -> String {
    match id.split_once("__") {
        Some((ferrule_gateway::SCHEDULER_PSEUDO_CHANNEL, task)) => {
            format!("scheduled task {task}")
        }
        Some((crate::goal_door::GOAL_CHANNEL, _)) => "a goal loop from a chat".into(),
        Some(("plan", _)) => "a chat's /plan".into(),
        Some((channel, chat)) => format!("{channel} chat {chat}"),
        None => "the terminal, or a sub-agent".into(),
    }
}

/// The messages a person and the agent said, and the first the person
/// said. A line is looked at as text first: inside a JSON string these
/// markers would be escaped, so only a message's own fields match.
fn read_transcript(path: &Path) -> (usize, String) {
    let Ok(file) = std::fs::File::open(path) else {
        return (0, String::new());
    };
    let (mut n, mut first) = (0, String::new());
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains(r#""type":"message""#) {
            continue;
        }
        let user = line.contains(r#""role":"user""#);
        if !(user || line.contains(r#""role":"assistant""#)) {
            continue;
        }
        n += 1;
        if user && first.is_empty() {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                let text = v["message"]["content"].as_str().unwrap_or("");
                first = text.split_whitespace().collect::<Vec<_>>().join(" ");
            }
        }
    }
    (n, first)
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The goal loop's state, when the session is one whose judge hasn't
/// passed: (goal, judge runs).
fn goal_pending(dir: &Path, id: &str) -> Option<crate::goal::LoopState> {
    crate::goal::load(dir, id)
        .ok()
        .flatten()
        .filter(|s| !s.judge_satisfied())
}

/// `GET /api/sessions`: the newest 50, archived ones left out.
pub fn list(ctx: &Ctx) -> Answer {
    let Some(data) = &ctx.data else {
        return missing("the data directory");
    };
    let dir = data.join("sessions");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ok(json!({ "sessions": [] })),
        Err(e) => return bad(500, format!("The sessions folder can't be read: {e}")),
    };
    let mut found: Vec<(SystemTime, String)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".jsonl")?.to_string();
            // `/new` put it aside as `<id>.<stamp>.jsonl`.
            if id.contains('.') {
                return None;
            }
            let mtime = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((mtime, id))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let router = ctx.live.as_ref().and_then(|l| l.router.upgrade());
    let rows: Vec<Value> = found
        .into_iter()
        .take(SHOWN)
        .map(|(mtime, id)| {
            let (messages, first) = read_transcript(&dir.join(format!("{id}.jsonl")));
            let goal = goal_pending(&dir, &id);
            json!({
                "place": place_of(&id),
                "updated": unix(mtime),
                "messages": messages,
                "first": clip(&ctx.redactor.redact(&first), FIRST),
                "goal_pending": goal.is_some(),
                "goal": goal.map(|g| clip(&ctx.redactor.redact(&g.goal), FIRST)),
                "busy": router.as_ref().is_some_and(|r| r.busy(&id)),
                "id": id,
            })
        })
        .collect();
    ok(json!({ "sessions": rows }))
}

/// What a resume run is called, to find one that's still going.
fn label(id: &str) -> String {
    format!("ferrule run --resume {id}")
}

/// `POST /api/sessions/resume {id}`: a goal-pending loop, resumed in the
/// background as `ferrule run --resume <id>` would at the terminal.
pub fn resume(ctx: &Ctx, body: &Value) -> Answer {
    let id = need!(arg(body, "id"));
    // A session id as files are named, and never one a child reads as a flag.
    if !child_id(id) {
        return bad(400, "That isn't a session id.");
    }
    let _one = STARTING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(data) = &ctx.data else {
        return missing("the data directory");
    };
    let dir = data.join("sessions");
    if !dir.join(format!("{id}.jsonl")).exists() {
        return bad(404, format!("There's no session `{id}`."));
    }
    let Some(state) = goal_pending(&dir, id) else {
        let why = match crate::goal::load(&dir, id) {
            Ok(Some(_)) => "its goal is already met: the judge passed",
            _ => "it isn't a goal loop; send a message in its chat to go on with a conversation",
        };
        return bad(
            409,
            format!("Only a goal-pending loop resumes from here, and `{id}` can't: {why}."),
        );
    };
    let router = ctx.live.as_ref().and_then(|l| l.router.upgrade());
    if still_running(ctx, &label(id)) {
        return bad(
            409,
            format!(
                "Goal loop `{id}` is already resuming from this page; its output is under Runs."
            ),
        );
    }
    // A lane the gateway runs it on, with a turn going or messages waiting:
    // two agents would write one transcript.
    let lane = router.as_ref().and_then(|r| r.lane(id));
    if lane
        .as_ref()
        .is_some_and(|l| l.busy_for.is_some() || l.queued > 0)
    {
        return bad(
            409,
            format!("Goal loop `{id}` is running in the gateway now; wait for its round to end."),
        );
    }
    let Some(hub) = ctx.hub.clone() else {
        return missing("the trust hub");
    };
    let goal = clip(&state.goal, 120);
    need!(confirmed(
        body,
        format!(
            "Resume the goal “{goal}”? It runs here as `{}`, under your caps, for up to {} hours; a gated command in it is refused, as there's no terminal to ask.",
            label(id),
            RESUME_LIMIT.as_secs() / 3600
        )
    ));
    // The gateway's idle lane for it goes: the child is its one writer.
    if let (Some(router), Some(_)) = (&router, &lane) {
        router.retire(id);
    }
    let args = vec!["run".into(), format!("--resume={id}").into()];
    let (who, sid) = (by(), id.to_string());
    let started = ctx
        .runs
        .start_for(label(id), args, child_env(ctx), RESUME_LIMIT, move |run| {
            hub.audit().record(
                chrono::Utc::now(),
                "dashboard.session.resumed",
                Some(&sid),
                None,
                json!({ "by": who, "code": run.code(), "secs": run.started.elapsed().map_or(0, |d| d.as_secs()) }),
            );
        });
    match started {
        Ok(run) => {
            if let Some(hub) = &ctx.hub {
                hub.audit().record(
                    chrono::Utc::now(),
                    "dashboard.session.resume",
                    Some(id),
                    None,
                    json!({ "by": by(), "run": run.id, "attempts": state.attempts }),
                );
            }
            ok(json!({
                "ok": true,
                "said": format!("Resuming the goal loop in the background as `{}`; its output is under Runs.", label(id)),
                "run": run.id,
            }))
        }
        Err(e) => bad(
            500,
            format!("The goal loop wasn't resumed: `ferrule run` couldn't start ({e})."),
        ),
    }
}
