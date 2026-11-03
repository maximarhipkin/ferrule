//! M66: the Control page's summary (`GET /api/control`): the kill switch,
//! the autonomy mode, the caps, the standing approvals, the gates, and
//! what is running and waiting now. Read-only: every change goes through
//! the routes that already make it (`kill/on`, `kill/off`, `sandbox/mode`,
//! `settings/caps`, `grants/revoke`).

use super::api::{ok, Answer};
use super::Ctx;
use serde_json::{json, Value};

/// What the page calls each `sandbox/mode` choice (and the machine's own).
pub fn mode_label(mode: &str) -> &'static str {
    match mode {
        "owner" => "Protected autonomous",
        "ask" => "Ask me first",
        _ => "Sandbox off (set on the machine)",
    }
}

/// `GET /api/control`.
pub fn view(ctx: &Ctx) -> Answer {
    let mode = super::config_page::autonomy(ctx);
    let mut out = json!({
        "mode": mode,
        "mode_label": mode_label(mode),
        "kill": { "on": false, "by": null, "at": null, "at_unix": null, "reason": null },
        "caps": null,
        "grants": 0,
        "untrusted_send_gate": true,
        "approval_timeout_secs": null,
        "owner_chat": ctx.owner_chat.as_ref().map(|c| c.to_string()),
        "running": 0,
        "waiting": 0,
    });
    if let Some(hub) = &ctx.hub {
        let stop = hub.stopped();
        out["kill"] = json!({
            "on": stop.is_some(),
            "by": stop.as_ref().map(|s| s.by.clone()),
            "at": stop.as_ref().map(|s| s.at.clone()),
            "at_unix": stop
                .as_ref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s.at).ok())
                .map(|t| t.timestamp()),
            "reason": stop.and_then(|s| s.reason),
        });
        out["caps"] = super::api::spend(hub, &mut Vec::new());
        out["grants"] = json!(hub.grants().list().len());
        let cfg = hub.config();
        out["untrusted_send_gate"] = json!(cfg.untrusted_send_gate);
        out["approval_timeout_secs"] = json!(cfg.approval_timeout_secs);
        if let Some(primary) = hub.primary() {
            out["owner_chat"] = json!(primary.to_string());
        }
        // As the inbox lists them: a step-up is answered in the owner's chat.
        out["waiting"] = json!(hub
            .approvals()
            .list()
            .iter()
            .filter(|w| w.op.as_deref() != Some(super::STEP_UP_OP))
            .count());
    } else {
        out["why"] = Value::from(
            "The kill switch, caps and approvals are shown when this process has the trust hub.",
        );
    }
    if let Some(router) = ctx.live.as_ref().and_then(|l| l.router.upgrade()) {
        out["running"] = json!(router.running());
    }
    ok(out)
}
