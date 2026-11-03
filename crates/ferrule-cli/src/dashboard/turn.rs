//! M66: a turn's steps, live (`GET /api/turn`): what the agent is doing in
//! one chat, step by step — model calls, tool calls (a tool's name and the
//! argument the `activity` line shows, never its output), checks, retries
//! and fallbacks — and, once it has answered, the steps of that last turn.

use super::api::{bad, ok, Answer};
use super::http::Request;
use super::Ctx;
use ferrule_gateway::health::clip;
use ferrule_gateway::{LaneSnapshot, TurnStep};
use serde_json::{json, Value};

/// The page's own chat's session (`dashboard__owner`).
pub fn dashboard_session() -> String {
    ferrule_gateway::session::session_id(super::chat::CHANNEL, super::chat::CHAT)
}

/// Seconds with a millisecond's precision.
fn secs(d: std::time::Duration) -> f64 {
    (d.as_secs_f64() * 1000.0).round() / 1000.0
}

/// The most of a step's name or summary the page shows.
const SHOWN: usize = 159;

/// A step's raw text as the page shows it: redacted, URL paths and
/// queries hidden (a provider's error can carry `?key=`), and only then cut
/// short, so a cut never leaves part of a key. Where the router cut it, the
/// last word (maybe part of a key) goes too.
pub fn shown(raw: &str, ctx: &Ctx) -> String {
    let mut text = super::api::url_paths_hidden(&ctx.redactor.redact(raw));
    if raw.chars().count() > ferrule_gateway::STEP_RAW
        && text.ends_with(ferrule_gateway::STEP_RAW_CUT)
    {
        let cut = text.rfind(char::is_whitespace).unwrap_or(0);
        text.truncate(cut);
        text.push(ferrule_gateway::STEP_RAW_CUT);
    }
    clip(&text, SHOWN)
}

fn step_json(s: &TurnStep, ctx: &Ctx) -> Value {
    json!({
        "at_secs": secs(s.at),
        "kind": s.kind,
        "name": shown(&s.name, ctx),
        "summary": shown(&s.summary, ctx),
        "status": s.status,
        "secs": s.took.map(secs),
    })
}

/// One lane as the page shows its turn.
pub fn lane_json(l: &LaneSnapshot, ctx: &Ctx) -> Value {
    let mut v = json!({
        "session": l.session_id,
        "place": ctx.redactor.redact(&l.place()),
        "busy": l.busy_for.is_some(),
        // The same preview as health's turns (docs/dashboard.md, "What it
        // never shows").
        "text": clip(&ctx.redactor.redact(&l.text), 80),
        "busy_secs": l.busy_for.map(|d| d.as_secs()),
        "activity": ctx.redactor.redact(&l.activity),
        "queued": l.queued,
        "steps": l.steps.iter().map(|s| step_json(s, ctx)).collect::<Vec<_>>(),
        "dropped": l.steps_dropped,
        "resumable": l.resumable,
        "last": l.steps_last,
    });
    if let Some((tokens_in, tokens_out)) = l.tokens {
        v["tokens_in"] = json!(tokens_in);
        v["tokens_out"] = json!(tokens_out);
    }
    v
}

/// Nothing to show: no turn has run there since the gateway started.
fn idle(session: &str, why: Option<&str>) -> Value {
    let mut v = json!({
        "session": session,
        "busy": false,
        "steps": [],
        "dropped": 0,
        "resumable": false,
        "last": false,
    });
    if let Some(why) = why {
        v["why"] = json!(why);
    }
    v
}

/// `GET /api/turn?session=<id>` (or `?lane=dashboard`, the page's own
/// chat): the turn running there, or the last one with `last: true`.
pub fn view(ctx: &Ctx, req: &Request) -> Answer {
    let dashboard = dashboard_session();
    let session = match (
        req.query.get("session").map(|s| s.trim()),
        req.query.get("lane").map(String::as_str),
    ) {
        (Some(s), _) if !s.is_empty() => s.to_string(),
        (_, Some("dashboard")) => dashboard.clone(),
        (_, Some(other)) => {
            return bad(
                400,
                format!("`lane` can only be `dashboard` (this page's chat), not `{other}`."),
            )
        }
        _ => {
            return bad(
                400,
                "Say which turn: `session=<id>`, or `lane=dashboard` for this page's chat.",
            )
        }
    };
    let Some(router) = ctx.live.as_ref().and_then(|l| l.router.upgrade()) else {
        return ok(idle(
            &session,
            Some("Turns are shown when the dashboard runs inside the gateway (the service)."),
        ));
    };
    match router.lane(&session) {
        Some(lane) => ok(lane_json(&lane, ctx)),
        // The page's chat has a lane once it's used; before, it's idle.
        None if session == dashboard => ok(idle(&session, None)),
        None => bad(
            404,
            format!(
                "No chat with session `{}` has run a turn since the gateway started.",
                ctx.redactor.redact(&session)
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_gateway::Redactor;
    use std::sync::Arc;

    #[test]
    fn a_steps_text_is_redacted_and_scrubbed_before_it_is_cut() {
        let key = format!("sk-LONGSEED-{}", "q7".repeat(95));
        let ctx = Ctx::bare(Arc::new(Redactor::new([key.clone()])));
        // A key straddling the page's cut: redacted whole first.
        let raw = format!("{} {key} tail", "x".repeat(150));
        let out = shown(&raw, &ctx);
        assert!(!out.contains("sk-LONG"), "{out}");
        assert!(out.chars().count() <= SHOWN + 1);
        // A provider's error with a key in a URL's query.
        let out = shown(
            "HTTP 403 from https://api.example.com/v1/models?key=AIzaSyQ1w2e3r4 refused",
            &ctx,
        );
        assert!(
            !out.contains("AIza") && out.contains("api.example.com"),
            "{out}"
        );
        // The router cut the text inside a key: that last word goes too,
        // even when what's left after redacting is short enough to show.
        let mut raw = format!("{key} {key} {key} {key} ");
        raw.push_str(&key[..ferrule_gateway::STEP_RAW - raw.chars().count()]);
        raw.push(ferrule_gateway::STEP_RAW_CUT);
        assert_eq!(raw.chars().count(), ferrule_gateway::STEP_RAW + 1);
        let out = shown(&raw, &ctx);
        assert!(!out.contains("sk-LONG"), "{out}");
        assert!(out.starts_with("[redacted] [redacted]"), "{out}");
        assert!(out.ends_with(ferrule_gateway::STEP_RAW_CUT), "{out}");
    }
}
