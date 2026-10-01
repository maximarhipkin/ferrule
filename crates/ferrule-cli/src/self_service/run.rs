//! Running an op, after the owner approved it (or at once, for a read).
//!
//! A change goes through `dashboard::api::act`, the code the page's buttons
//! run, so its validation, its managed locks and its audit rows apply
//! whoever asks. Nothing here edits a file by hand.

use super::describe::get;
use super::Op;
use crate::dashboard::{api, backup_page, Ctx};
use ferrule_trust::ChatRef;
use serde_json::{json, Value};
use std::time::Duration;

/// A result is never longer than this many characters.
const SHOWN: usize = 3000;

/// How long `doctor` may take before the answer says so.
const DOCTOR_WAIT: Duration = Duration::from_secs(90);

fn clip(text: &str) -> String {
    if text.chars().count() <= SHOWN {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(SHOWN).collect();
    cut.push('…');
    cut
}

/// The page's POST for a change op, and the body it takes.
fn route_of(op: &Op, here: &ChatRef) -> Option<(&'static str, Value)> {
    Some(match op {
        Op::ModelDefault { model } => ("models/default", json!({ "model": model })),
        Op::ModelHere { model: Some(model) } => (
            "models/pin",
            json!({ "channel": here.channel, "chat": here.chat, "model": model }),
        ),
        Op::ModelHere { model: None } => (
            "models/unpin",
            json!({ "channel": here.channel, "chat": here.chat }),
        ),
        Op::ModelFallback { models } => ("models/fallback", json!({ "models": models })),
        Op::ConfigSet { key, value } => ("config/set", json!({ "key": key, "value": value })),
        Op::Caps { caps } => ("settings/caps", json!({ "caps": caps })),
        Op::SkillOn { name } => ("skills/enable", json!({ "name": name })),
        Op::SkillOff { name } => ("skills/disable", json!({ "name": name })),
        Op::McpOn { name } => ("mcp/enable", json!({ "name": name })),
        Op::McpOff { name } => ("mcp/disable", json!({ "name": name })),
        Op::McpRemove { name } => ("mcp/remove", json!({ "name": name })),
        Op::HooksTrust { sha } => ("hooks/trust", json!({ "sha": sha })),
        Op::HooksUntrust => ("hooks/untrust", json!({})),
        Op::TaskAdd {
            name,
            prompt,
            schedule,
            kind,
            timezone,
            model,
        } => (
            "tasks/add",
            json!({
                "name": name, "prompt": prompt, "schedule": schedule,
                "kind": kind, "timezone": timezone, "model": model,
            }),
        ),
        Op::TaskSchedule {
            id,
            schedule,
            timezone,
        } => (
            "tasks/schedule",
            json!({ "id": id, "schedule": schedule, "timezone": timezone }),
        ),
        Op::TaskModel { id, model } => (
            "tasks/model",
            json!({ "id": id, "model": model.clone().unwrap_or_else(|| "default".into()) }),
        ),
        Op::TaskPause { id } => ("tasks/pause", json!({ "id": id })),
        Op::TaskResume { id } => ("tasks/resume", json!({ "id": id })),
        Op::TaskDelete { id } => ("tasks/delete", json!({ "id": id })),
        Op::TaskRunNow { id } => ("tasks/run", json!({ "id": id })),
        Op::ChannelRestart { name } => ("channels/restart", json!({ "name": name })),
        Op::ConfigRestore => ("config/restore", json!({})),
        Op::Disconnect { name } => ("connections/disconnect", json!({ "name": name })),
        _ => return None,
    })
}

/// What the page's handler answered, as a sentence or its error.
fn said(answer: api::Answer) -> Result<String, String> {
    match answer {
        Some((200 | 202, v)) => Ok(v["said"]
            .as_str()
            .or_else(|| v["message"].as_str())
            .unwrap_or("Done.")
            .to_string()),
        Some((_, v)) => Err(v["error"].as_str().unwrap_or("it failed").to_string()),
        None => Err("that isn't available in this process".into()),
    }
}

/// Runs `op`. A change is audited as `by`; the result is redacted and
/// clipped, so no secret and no wall of text reaches the model or the chat.
pub async fn run(op: &Op, ctx: &Ctx, by: &str, here: &ChatRef) -> Result<String, String> {
    let out = run_inner(op, ctx, by, here).await;
    out.map(|t| clip(&ctx.redactor.redact(&t)))
        .map_err(|t| clip(&ctx.redactor.redact(&t)))
}

async fn run_inner(op: &Op, ctx: &Ctx, by: &str, here: &ChatRef) -> Result<String, String> {
    if let Some((path, mut body)) = route_of(op, here) {
        // The owner's tap is the confirmation the page's sheet would ask.
        body["confirm"] = json!(true);
        return said(api::act(ctx, path, &body, by).await);
    }
    match op {
        Op::Status => status(ctx).await,
        Op::Doctor => doctor(ctx).await,
        Op::Audit { limit } => audit(ctx, *limit),
        Op::Models => models(ctx).await,
        Op::ModelTest { model } => model_test(ctx, model).await,
        Op::Settings => settings(ctx).await,
        Op::Tasks => tasks(ctx).await,
        Op::Connections => connections(ctx).await,
        Op::ConfigGet { key } => config_get(ctx, key.as_deref()).await,
        Op::Backup => backup(ctx, by).await,
        Op::UpdateCheck | Op::Update | Op::Restart => Err("not built yet".into()),
        _ => Err(format!("`{}` has no runner", op.name())),
    }
}

async fn status(ctx: &Ctx) -> Result<String, String> {
    let h = get(ctx, "health").await?;
    let mut lines = vec![format!(
        "Ferrule {}{}.",
        h["version"].as_str().unwrap_or("?"),
        match (h["gateway"] == true, h["uptime"].as_str()) {
            (true, Some(up)) => format!(", the gateway has been up {up}"),
            (true, None) => ", the gateway is running".to_string(),
            _ => String::new(),
        }
    )];
    let turns = h["turns"].as_array().map_or(0, Vec::len);
    if turns > 0 {
        lines.push(format!("{turns} turn(s) running now."));
    }
    for u in h["updates"].as_array().into_iter().flatten() {
        if let Some(u) = u.as_str() {
            lines.push(u.to_string());
        }
    }
    let problems = h["problems"].as_array().cloned().unwrap_or_default();
    if problems.is_empty() {
        lines.push("Nothing is wrong that I can see.".into());
    }
    for p in problems {
        lines.push(format!("Problem: {}", p["what"].as_str().unwrap_or("")));
    }
    Ok(lines.join("\n"))
}

async fn doctor(ctx: &Ctx) -> Result<String, String> {
    let id = match api::act(ctx, "doctor/run", &json!({}), "doctor").await {
        Some((200, v)) => v["id"].as_str().unwrap_or_default().to_string(),
        other => return said(other),
    };
    let Some(run) = ctx.runs.get(&id) else {
        return Err("doctor started but its run is gone".into());
    };
    let until = std::time::Instant::now() + DOCTOR_WAIT;
    while !run.done() {
        if std::time::Instant::now() >= until {
            return Err("Doctor didn't finish in 90 seconds; ask again in a minute.".into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let report = api::doctor_report(&run.output()).ok_or("doctor gave no report")?;
    let mut lines = Vec::new();
    for item in report["items"].as_array().into_iter().flatten() {
        let level = item["level"].as_str().unwrap_or("");
        if !matches!(level, "warn" | "fail") {
            continue;
        }
        lines.push(format!(
            "{level}: {} — {}",
            item["what"].as_str().unwrap_or(""),
            item["detail"].as_str().unwrap_or("")
        ));
    }
    if lines.is_empty() {
        return Ok("Doctor found nothing wrong.".into());
    }
    Ok(lines.join("\n"))
}

fn audit(ctx: &Ctx, limit: usize) -> Result<String, String> {
    let hub = ctx
        .hub
        .as_ref()
        .ok_or("the audit log isn't available here")?;
    let events = hub.audit().read(None).map_err(|e| e.to_string())?;
    let skip = events.len().saturating_sub(limit);
    let lines: Vec<String> = events
        .iter()
        .skip(skip)
        .map(|e| format!("{} {} {}", e.at, e.event, e.detail))
        .collect();
    if lines.is_empty() {
        Ok("The audit log is empty.".into())
    } else {
        Ok(lines.join("\n"))
    }
}

async fn models(ctx: &Ctx) -> Result<String, String> {
    let v = get(ctx, "models").await?;
    let view = &v["view"];
    let names = |x: &Value| {
        x.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "none".into())
    };
    let mut lines = vec![format!(
        "Default: {}. Fallback: {}.",
        view["default"].as_str().unwrap_or("none"),
        names(&view["fallback"])
    )];
    for r in view["models"].as_array().into_iter().flatten() {
        let ready = r["key_present"] == true;
        lines.push(format!(
            "- {}{}{}",
            r["reference"].as_str().unwrap_or(""),
            if ready {
                String::new()
            } else {
                format!(" (not ready: {})", r["missing"].as_str().unwrap_or(""))
            },
            match names(&r["aliases"]).as_str() {
                "none" => String::new(),
                a => format!(" — aliases {a}"),
            }
        ));
    }
    for p in v["problems"].as_array().into_iter().flatten() {
        lines.push(format!(
            "Problem: {}",
            p["what"].as_str().or(p.as_str()).unwrap_or("")
        ));
    }
    Ok(lines.join("\n"))
}

async fn model_test(ctx: &Ctx, model: &str) -> Result<String, String> {
    match api::act(ctx, "models/test", &json!({ "model": model }), "chat").await {
        Some((200, v)) => {
            let said = v["said"].as_str().unwrap_or("");
            if v["ok"] == true {
                Ok(format!("{model} answered: {said}"))
            } else {
                Err(format!("{model} didn't answer: {said}"))
            }
        }
        other => said(other),
    }
}

async fn settings(ctx: &Ctx) -> Result<String, String> {
    let v = get(ctx, "settings").await?;
    let mut lines = Vec::new();
    for c in v["caps"].as_array().into_iter().flatten() {
        lines.push(format!(
            "cap {}: {} {}",
            c["key"].as_str().unwrap_or(""),
            c["value"],
            c["unit"].as_str().unwrap_or("")
        ));
    }
    for s in v["skills"].as_array().into_iter().flatten() {
        lines.push(format!(
            "skill {} ({})",
            s["name"].as_str().unwrap_or(""),
            if s["disabled"] == true { "off" } else { "on" }
        ));
    }
    for s in v["mcp"].as_array().into_iter().flatten() {
        lines.push(format!(
            "MCP server {} ({})",
            s["name"].as_str().unwrap_or(""),
            if s["disabled"] == true { "off" } else { "on" }
        ));
    }
    let h = &v["workspace_hooks"];
    if !h.is_null() {
        lines.push(format!(
            "workspace hooks in {}: fingerprint {}, {}",
            h["file"].as_str().unwrap_or(""),
            h["sha"].as_str().unwrap_or(""),
            if h["trusted"] == true {
                "trusted"
            } else {
                "not trusted"
            }
        ));
    }
    Ok(lines.join("\n"))
}

async fn tasks(ctx: &Ctx) -> Result<String, String> {
    let v = get(ctx, "tasks").await?;
    let rows = v["tasks"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return Ok("There are no scheduled tasks.".into());
    }
    Ok(rows
        .iter()
        .map(|t| {
            format!(
                "- {} (id {}): {} {}{}",
                t["name"].as_str().unwrap_or(""),
                t["id"].as_str().unwrap_or(""),
                t["schedule"].as_str().unwrap_or(""),
                t["timezone"].as_str().unwrap_or(""),
                if t["enabled"] == true { "" } else { ", paused" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

async fn connections(ctx: &Ctx) -> Result<String, String> {
    let v = get(ctx, "connections").await?;
    if v["available"] == false {
        return Ok("Connections aren't available in this process.".into());
    }
    let mut lines: Vec<String> = v["connections"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| format!("- {} (connected)", c["name"].as_str().unwrap_or("")))
        .collect();
    if lines.is_empty() {
        lines.push("Nothing is connected.".into());
    }
    for a in v["attention"].as_array().into_iter().flatten() {
        lines.push(format!(
            "Needs attention: {}",
            a["text"].as_str().unwrap_or("")
        ));
    }
    Ok(lines.join("\n"))
}

/// The settings a chat may show: the form's own keys. Anything else,
/// guarded or secret, isn't shown.
async fn config_get(ctx: &Ctx, key: Option<&str>) -> Result<String, String> {
    let v = get(ctx, "config").await?;
    let fields = v["fields"].as_array().cloned().unwrap_or_default();
    let show = |f: &Value| match &f["value"] {
        Value::Null => "the default".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match key {
        None => {
            let mut lines = vec!["The settings I can show and change:".to_string()];
            for f in &fields {
                lines.push(format!(
                    "- {} = {} ({})",
                    f["key"].as_str().unwrap_or(""),
                    show(f),
                    f["help"].as_str().unwrap_or("")
                ));
            }
            Ok(lines.join("\n"))
        }
        Some(k) => match fields.iter().find(|f| f["key"] == k) {
            Some(f) => Ok(format!("{k} = {}", show(f))),
            None => Err(format!("`{k}` isn't shown in a chat.")),
        },
    }
}

async fn backup(ctx: &Ctx, by: &str) -> Result<String, String> {
    said(api::act(ctx, "backup", &json!({}), by).await)?;
    backup_page::finished(ctx, Duration::from_secs(120)).await?;
    Ok("Backup saved. It has no keys or tokens; the dashboard's Backup page downloads it.".into())
}
