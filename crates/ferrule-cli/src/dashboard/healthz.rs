//! M44 part 5: `/healthz` and `/busyz`. Answered before the page's own
//! host and session checks, so a container's health check and the panel
//! can ask without signing in. Everything is read from memory: no model
//! call, no disk write.

use std::time::Duration;

use serde_json::json;

use super::http::{Request, Response};
use super::Dashboard;

/// What the judgement needs, gathered by [`facts`].
pub struct Facts {
    pub dispatch_stuck: Option<Duration>,
    /// Each configured channel (the dashboard's chat left out) and its problem, if any.
    pub channels: Vec<(String, Option<String>)>,
    pub stale: Vec<String>,
    pub no_model: bool,
    pub kill: Option<String>,
    pub on_copy: bool,
    pub commands_off: Option<String>,
    pub closing: bool,
    pub managed: bool,
    pub turns: usize,
    pub queued: usize,
    pub uptime_secs: u64,
}

/// `ok`, `degraded` or `failing`, and why.
pub fn judge(f: &Facts) -> (&'static str, Vec<String>) {
    let mut failing = Vec::new();
    if let Some(d) = f.dispatch_stuck {
        failing.push(format!(
            "the gateway's dispatch loop has been stuck for {}s",
            d.as_secs()
        ));
    }
    if !f.channels.is_empty() && f.channels.iter().all(|(_, p)| p.is_some()) {
        let each: Vec<String> = f
            .channels
            .iter()
            .filter_map(|(n, p)| p.as_ref().map(|p| format!("{n}: {p}")))
            .collect();
        failing.push(format!("every channel has a problem: {}", each.join("; ")));
    }
    if !failing.is_empty() {
        return ("failing", failing);
    }
    let mut why = Vec::new();
    if f.no_model {
        why.push("no model is set up yet".to_string());
    }
    if f.channels.is_empty() {
        why.push("no channel is set up yet".to_string());
    }
    for (name, problem) in &f.channels {
        if let Some(p) = problem {
            why.push(format!("{name}: {p}"));
        }
    }
    for name in &f.stale {
        why.push(format!("{name} has gone quiet"));
    }
    if f.on_copy {
        why.push("the config has an error, so the gateway runs on its last good copy".into());
    }
    if let Some(k) = &f.kill {
        why.push(format!("the kill switch is on: {k}"));
    }
    if let Some(c) = &f.commands_off {
        why.push(format!("commands are off: {c}"));
    }
    if f.closing {
        why.push("the bot is stopping".into());
    }
    (if why.is_empty() { "ok" } else { "degraded" }, why)
}

fn facts(d: &Dashboard) -> Facts {
    let mut f = Facts {
        dispatch_stuck: None,
        channels: Vec::new(),
        stale: Vec::new(),
        no_model: false,
        kill: None,
        on_copy: crate::last_good::on_copy().is_some(),
        commands_off: None,
        closing: false,
        managed: crate::managed::on(),
        turns: 0,
        queued: 0,
        uptime_secs: d.started.elapsed().as_secs(),
    };
    f.no_model = match &d.ctx.models {
        None => true,
        Some(m) => !m.view().models.iter().any(|r| r.default && r.key_present),
    };
    f.kill = d.ctx.hub.as_ref().and_then(|h| h.stopped()).map(|s| s.by);
    if let Some(live) = &d.ctx.live {
        f.dispatch_stuck = live
            .health
            .dispatch_busy_for()
            .filter(|s| *s > ferrule_gateway::health::DISPATCH_STUCK);
        f.channels = live
            .channels
            .iter()
            .filter(|c| c.name() != super::chat::CHANNEL)
            .map(|c| (c.name().to_string(), c.problem()))
            .collect();
        f.stale = live.health.stale_channels(&live.channels);
        f.commands_off = live.commands_off.clone();
        if let Some(r) = live.router.upgrade() {
            f.turns = r.running();
            f.queued = r.queued();
            f.closing = r.closing();
        }
    }
    f
}

fn counts(d: &Dashboard) -> (usize, usize) {
    let f = facts(d);
    (f.turns, f.queued)
}

fn allowed(req: &Request) -> bool {
    matches!(req.method.as_str(), "GET" | "HEAD")
}

pub fn healthz(d: &Dashboard, req: &Request) -> Response {
    if !allowed(req) {
        return Response::text(405, "GET only");
    }
    let f = facts(d);
    let (status, reasons) = if d.ctx.live.is_none() {
        ("failing", vec!["the gateway isn't running".to_string()])
    } else {
        judge(&f)
    };
    let reasons: Vec<String> = reasons.iter().map(|r| d.ctx.redactor.redact(r)).collect();
    let body = json!({
        "status": status,
        "reasons": reasons,
        "version": env!("CARGO_PKG_VERSION"),
        "managed": f.managed,
        "busy": f.turns + f.queued > 0,
        "turns": f.turns,
        "queued": f.queued,
        "uptime_secs": f.uptime_secs,
    });
    let code = if status == "failing" { 503 } else { 200 };
    Response::json(code, &body).with_header("Cache-Control", "no-store".into())
}

pub fn busyz(d: &Dashboard, req: &Request) -> Response {
    if !allowed(req) {
        return Response::text(405, "GET only");
    }
    let (turns, queued) = counts(d);
    let resp = if turns + queued == 0 {
        Response::json(200, &json!({ "busy": false }))
    } else {
        Response::json(
            409,
            &json!({ "busy": true, "turns": turns, "queued": queued }),
        )
    };
    resp.with_header("Cache-Control", "no-store".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet() -> Facts {
        Facts {
            dispatch_stuck: None,
            channels: vec![("telegram".into(), None)],
            stale: vec![],
            no_model: false,
            kill: None,
            on_copy: false,
            commands_off: None,
            closing: false,
            managed: true,
            turns: 0,
            queued: 0,
            uptime_secs: 1,
        }
    }

    #[test]
    fn judge_says_ok_degraded_and_failing() {
        assert_eq!(judge(&quiet()), ("ok", vec![]));

        let f = Facts {
            no_model: true,
            ..quiet()
        };
        assert_eq!(
            judge(&f),
            ("degraded", vec!["no model is set up yet".to_string()])
        );

        let f = Facts {
            dispatch_stuck: Some(Duration::from_secs(90)),
            ..quiet()
        };
        let (s, why) = judge(&f);
        assert_eq!(s, "failing");
        assert_eq!(why, ["the gateway's dispatch loop has been stuck for 90s"]);

        let f = Facts {
            channels: vec![
                ("telegram".into(), Some("bad token".into())),
                ("discord".into(), Some("socket closed".into())),
            ],
            ..quiet()
        };
        let (s, why) = judge(&f);
        assert_eq!(s, "failing");
        assert_eq!(
            why,
            ["every channel has a problem: telegram: bad token; discord: socket closed"]
        );

        let f = Facts {
            channels: vec![
                ("telegram".into(), Some("bad token".into())),
                ("discord".into(), None),
            ],
            stale: vec!["discord".into()],
            ..quiet()
        };
        let (s, why) = judge(&f);
        assert_eq!(s, "degraded");
        assert_eq!(why, ["telegram: bad token", "discord has gone quiet"]);

        let f = Facts {
            channels: vec![],
            closing: true,
            kill: Some("dashboard".into()),
            ..quiet()
        };
        let (s, why) = judge(&f);
        assert_eq!(s, "degraded");
        assert_eq!(
            why,
            [
                "no channel is set up yet",
                "the kill switch is on: dashboard",
                "the bot is stopping"
            ]
        );
    }
}
