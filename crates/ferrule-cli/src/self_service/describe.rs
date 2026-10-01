//! The words on a card: what exactly will change, from what, in plain
//! sentences. The owner approves these words, and the approval is bound to
//! the op that produced them.

use super::Op;
use crate::dashboard::{api, Ctx};
use ferrule_trust::ChatRef;
use serde_json::{json, Value};

const HOOKS_SHOWN: usize = 1500;
const PROMPT_SHOWN: usize = 500;

/// A page's GET as JSON, or why not.
pub(super) async fn get(ctx: &Ctx, path: &str) -> Result<Value, String> {
    api::read(ctx, path).await
}

fn list(names: impl Iterator<Item = String>) -> String {
    let all: Vec<String> = names.collect();
    if all.is_empty() {
        "there are none".into()
    } else {
        format!("they are: {}", all.join(", "))
    }
}

fn unknown(kind: &str, x: &str, names: impl Iterator<Item = String>) -> String {
    format!("there's no {kind} `{x}`; {}", list(names))
}

fn now_or(v: &Value) -> String {
    v.as_str().unwrap_or("none").to_string()
}

/// A task named by its name becomes its id, so the card, the digest and
/// the run all say the same one.
pub(super) async fn resolve(op: Op, ctx: &Ctx) -> Result<Op, String> {
    let id = match &op {
        Op::TaskSchedule { id, .. }
        | Op::TaskModel { id, .. }
        | Op::TaskPause { id }
        | Op::TaskResume { id }
        | Op::TaskDelete { id }
        | Op::TaskRunNow { id } => id.clone(),
        _ => return Ok(op),
    };
    let v = get(ctx, "tasks").await?;
    let rows = v["tasks"].as_array().cloned().unwrap_or_default();
    let found = rows
        .iter()
        .find(|t| t["id"] == id.as_str())
        .or_else(|| rows.iter().find(|t| t["name"] == id.as_str()));
    let Some(t) = found else {
        return Err(unknown(
            "task",
            &id,
            rows.iter().map(|t| {
                format!(
                    "{} ({})",
                    t["name"].as_str().unwrap_or(""),
                    t["id"].as_str().unwrap_or("")
                )
            }),
        ));
    };
    let real = t["id"].as_str().unwrap_or(&id).to_string();
    Ok(match op {
        Op::TaskSchedule {
            schedule, timezone, ..
        } => Op::TaskSchedule {
            id: real,
            schedule,
            timezone,
        },
        Op::TaskModel { model, .. } => Op::TaskModel { id: real, model },
        Op::TaskPause { .. } => Op::TaskPause { id: real },
        Op::TaskResume { .. } => Op::TaskResume { id: real },
        Op::TaskDelete { .. } => Op::TaskDelete { id: real },
        Op::TaskRunNow { .. } => Op::TaskRunNow { id: real },
        other => other,
    })
}

async fn task_name(ctx: &Ctx, id: &str) -> String {
    match get(ctx, "tasks").await {
        Ok(v) => v["tasks"]
            .as_array()
            .and_then(|r| r.iter().find(|t| t["id"] == id))
            .and_then(|t| t["name"].as_str().map(str::to_string))
            .unwrap_or_else(|| id.to_string()),
        Err(_) => id.to_string(),
    }
}

fn check_model(ctx: &Ctx, model: &str) -> Result<(), String> {
    let Some(m) = &ctx.models else {
        return Err("the models aren't available in this process".into());
    };
    m.resolve(model).map(|_| ()).map_err(|why| {
        let names: Vec<String> = m
            .view()
            .models
            .iter()
            .map(|r| r.reference.clone())
            .collect();
        format!(
            "`{model}` isn't a connected model ({why}); {}",
            list(names.into_iter())
        )
    })
}

/// The card's words for a change, or why there's nothing to ask about.
pub async fn describe(op: &Op, ctx: &Ctx, here: &ChatRef) -> Result<String, String> {
    Ok(match op {
        Op::ModelDefault { model } => {
            check_model(ctx, model)?;
            let old = ctx.models.as_ref().and_then(|m| m.view().default);
            format!(
                "Switch the default model to {model} (now {}). Every chat uses it from its next message.",
                old.as_deref().unwrap_or("none")
            )
        }
        Op::ModelHere { model } => {
            let m = ctx.models.as_ref().ok_or("the models aren't available in this process")?;
            let view = m.view();
            let pinned = view
                .pins
                .iter()
                .find(|p| p.channel == here.channel && p.chat == here.chat)
                .map(|p| p.reference.clone());
            let default = view.default.clone().unwrap_or_else(|| "none".into());
            match model {
                Some(model) => {
                    check_model(ctx, model)?;
                    format!(
                        "Use {model} in this chat only (now {}).",
                        pinned.unwrap_or(default)
                    )
                }
                None => format!(
                    "Stop pinning a model in this chat; it goes back to the default ({default})."
                ),
            }
        }
        Op::ModelFallback { models } => {
            for m in models {
                check_model(ctx, m)?;
            }
            let old = ctx
                .models
                .as_ref()
                .map(|m| m.view().fallback)
                .unwrap_or_default();
            let was = if old.is_empty() { "none".to_string() } else { old.join(", ") };
            if models.is_empty() {
                format!("Turn the fallback models off (now {was}).")
            } else {
                format!("Set the fallback models to {} (now {was}).", models.join(", "))
            }
        }
        Op::ConfigSet { key, value } => {
            let v = get(ctx, "config").await?;
            let fields = v["fields"].as_array().cloned().unwrap_or_default();
            let Some(f) = fields.iter().find(|f| f["key"] == key.as_str()) else {
                return Err(format!(
                    "`{key}` isn't a setting I can change here; the ones I can are: {}",
                    fields
                        .iter()
                        .filter_map(|f| f["key"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            };
            let show = |v: &Value| match v {
                Value::Null => "the default".to_string(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!(
                "Set `{key}` to `{}` in the config file (now `{}`).",
                show(value),
                show(&f["value"])
            )
        }
        Op::Caps { caps } => {
            let v = get(ctx, "settings").await?;
            let rows = v["caps"].as_array().cloned().unwrap_or_default();
            let mut lines = Vec::new();
            for (key, new) in caps {
                let Some(row) = rows.iter().find(|r| r["key"] == key.as_str()) else {
                    return Err(unknown(
                        "cap",
                        key,
                        rows.iter().filter_map(|r| r["key"].as_str().map(str::to_string)),
                    ));
                };
                let unit = row["unit"].as_str().unwrap_or("");
                let say = |n: f64| {
                    if n == 0.0 {
                        "no cap".to_string()
                    } else if unit == "usd" {
                        format!("${n:.2}")
                    } else {
                        format!("{} tokens", n as u64)
                    }
                };
                lines.push(format!(
                    "Set the cap `{key}` to {} (now {}).",
                    say(*new),
                    say(row["value"].as_f64().unwrap_or(0.0))
                ));
            }
            if lines.is_empty() {
                return Err("`caps` needs at least one cap".into());
            }
            lines.join("\n")
        }
        Op::SkillOn { name } | Op::SkillOff { name } => {
            let v = get(ctx, "settings").await?;
            let skills = v["skills"].as_array().cloned().unwrap_or_default();
            if !skills.iter().any(|s| s["name"] == name.as_str()) {
                return Err(unknown(
                    "skill",
                    name,
                    skills.iter().filter_map(|s| s["name"].as_str().map(str::to_string)),
                ));
            }
            if matches!(op, Op::SkillOn { .. }) {
                format!("Turn the skill `{name}` back on.")
            } else {
                format!("Turn the skill `{name}` off.")
            }
        }
        Op::McpOn { name } | Op::McpOff { name } | Op::McpRemove { name } => {
            let v = get(ctx, "settings").await?;
            let rows = v["mcp"].as_array().cloned().unwrap_or_default();
            if !rows.iter().any(|s| s["name"] == name.as_str()) {
                return Err(unknown(
                    "MCP server",
                    name,
                    rows.iter().filter_map(|s| s["name"].as_str().map(str::to_string)),
                ));
            }
            match op {
                Op::McpOn { .. } => format!("Turn the MCP server `{name}` back on."),
                Op::McpOff { .. } => format!(
                    "Turn the MCP server `{name}` off (its tools go away until it's on again)."
                ),
                _ => format!(
                    "Remove the MCP server `{name}` from the config. Adding it back needs its full settings again."
                ),
            }
        }
        Op::HooksTrust { sha } => {
            let v = get(ctx, "settings").await?;
            let h = &v["workspace_hooks"];
            if h.is_null() {
                return Err("this workspace has no hooks file to trust".into());
            }
            if let Some(e) = h["parse_error"].as_str() {
                return Err(format!("the hooks file doesn't read: {e}"));
            }
            if h["sha"] != sha.as_str() {
                return Err("the hooks changed since you looked; ask for them again".into());
            }
            let commands: Vec<String> = h["hooks"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|r| {
                    format!(
                        "• on {}{}: {}",
                        r["event"].as_str().unwrap_or(""),
                        r["matcher"].as_str().map(|m| format!(" ({m})")).unwrap_or_default(),
                        r["command"].as_str().unwrap_or("")
                    )
                })
                .collect();
            let text = commands.join("\n");
            if text.chars().count() > HOOKS_SHOWN {
                return Err("these hooks are too long to show on a card; the dashboard's Extensions page shows them in full and trusts them there".into());
            }
            format!(
                "Trust the hooks in {} (fingerprint {}). They run as you, outside the sandbox, whenever an agent works here:\n{text}",
                h["file"].as_str().unwrap_or("the workspace"),
                &sha[..sha.len().min(12)]
            )
        }
        Op::HooksUntrust => {
            let v = get(ctx, "settings").await?;
            let file = v["workspace_hooks"]["file"].as_str().unwrap_or("the workspace");
            format!("Stop trusting the hooks in {file}; they won't run until trusted again.")
        }
        Op::TaskAdd { name, prompt, schedule, kind, timezone, model } => {
            if prompt.chars().count() > PROMPT_SHOWN {
                return Err("the task's prompt is too long to show on a card; the dashboard's Tasks page takes it".into());
            }
            if let Some(m) = model.as_deref().filter(|m| *m != "default") {
                check_model(ctx, m)?;
            }
            let tz = timezone.clone().unwrap_or_else(|| "UTC".into());
            let body = json!({"schedule": schedule, "timezone": tz, "kind": kind.clone().unwrap_or_else(|| "cron".into())});
            let when = when(ctx, &body).await?;
            format!(
                "Add the task “{name}”: {when}, model {}. It will be told:\n“{prompt}”",
                model.as_deref().filter(|m| *m != "default").unwrap_or("the default")
            )
        }
        Op::TaskSchedule { id, schedule, timezone } => {
            let t = task_row(ctx, id).await?;
            let tz = timezone.clone().or_else(|| t["timezone"].as_str().map(str::to_string)).unwrap_or_else(|| "UTC".into());
            let new = when(ctx, &json!({"schedule": schedule, "timezone": tz})).await?;
            format!(
                "Change when the task “{}” runs: {} ({}) → {new}.",
                t["name"].as_str().unwrap_or(id),
                t["schedule"].as_str().unwrap_or(""),
                t["timezone"].as_str().unwrap_or("UTC"),
            )
        }
        Op::TaskModel { id, model } => {
            let t = task_row(ctx, id).await?;
            if let Some(m) = model.as_deref().filter(|m| *m != "default") {
                check_model(ctx, m)?;
            }
            format!(
                "Run the task “{}” on {} (now {}).",
                t["name"].as_str().unwrap_or(id),
                model.as_deref().filter(|m| *m != "default").unwrap_or("the default"),
                now_or_default(&t["model"]),
            )
        }
        Op::TaskPause { id } => format!("Pause the task “{}”.", task_name(ctx, id).await),
        Op::TaskResume { id } => format!("Resume the task “{}”.", task_name(ctx, id).await),
        Op::TaskDelete { id } => {
            format!("Delete the task “{}” and its run history.", task_name(ctx, id).await)
        }
        Op::TaskRunNow { id } => format!("Run the task “{}” now.", task_name(ctx, id).await),
        Op::ChannelRestart { name } => format!("Restart the {name} channel's connection."),
        Op::ConfigRestore => "Replace the config file with the last copy that read. The current file is kept beside it.".into(),
        Op::Backup => "Make a backup of the data dir (no secrets in it).".into(),
        Op::Disconnect { name } => {
            let c = ctx.connections.as_ref().ok_or("connections aren't available in this process")?;
            let snap = c.snapshot().map_err(|e| format!("{e:#}"))?;
            if !snap.connections.iter().any(|x| x.service == *name) {
                return Err(unknown(
                    "connection",
                    name,
                    snap.connections.iter().map(|x| x.service.clone()),
                ));
            }
            format!("Disconnect {name}: its saved sign-in is deleted and its tools go away.")
        }
        Op::Update | Op::Restart => return Err("not built yet".into()),
        _ => return Err(format!("`{}` doesn't ask", op.name())),
    })
}

fn now_or_default(v: &Value) -> String {
    match v.as_str() {
        Some(m) => m.to_string(),
        None => "the default".into(),
    }
}

async fn task_row(ctx: &Ctx, id: &str) -> Result<Value, String> {
    let v = get(ctx, "tasks").await?;
    v["tasks"]
        .as_array()
        .and_then(|r| r.iter().find(|t| t["id"] == id))
        .cloned()
        .ok_or_else(|| format!("there's no task `{id}`"))
}

/// "daily at 09:00 (next: 2026-10-02 09:00 Asia/Jerusalem)", by the same
/// parser `ferrule tasks add` uses.
async fn when(ctx: &Ctx, body: &Value) -> Result<String, String> {
    let _ = now_or;
    let answer = api::act(ctx, "tasks/preview", body, "preview").await;
    let (status, v) = answer.ok_or("the schedule can't be checked here")?;
    if status != 200 {
        return Err(v["error"]
            .as_str()
            .unwrap_or("that schedule doesn't read")
            .to_string());
    }
    let tz: chrono_tz::Tz = body["timezone"]
        .as_str()
        .and_then(|z| z.parse().ok())
        .unwrap_or(chrono_tz::UTC);
    let next = v["next"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_i64)
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| t.with_timezone(&tz).format("%Y-%m-%d %H:%M %Z").to_string());
    let sched = body["schedule"].as_str().unwrap_or("");
    Ok(match next {
        Some(n) => format!("{sched} (next {n})"),
        None => sched.to_string(),
    })
}
