//! M66: the extensions the agent asked to install and what the scan
//! suspended (`ferrule extensions pending`), and Deny. Approving one stays
//! at the terminal (`ferrule extensions approve <id>`): it fetches and
//! scans the extension there and shows the owner what the scan found
//! before asking, which one request from the page can't do.

use super::api::{arg, bad, by, missing, need, ok, Answer};
use super::Ctx;
use ferrule_extensions::{Finding, Layout, Level, LockStore, PendingQueue, Request, Status};
use serde_json::{json, Value};

fn layout(ctx: &Ctx) -> Option<Layout> {
    ctx.data.clone().map(Layout::new)
}

fn finding_json(f: &Finding) -> Value {
    json!({
        "rule": f.rule,
        "level": match f.level { Level::Block => "block", Level::Warn => "warn" },
        "item": f.item,
        "field": f.field,
        // The owner's to read; never the model's.
        "excerpt": f.excerpt,
    })
}

/// A request's kind as `ferrule extensions list` names them, and a name.
fn kind_and_name(r: &Request) -> (&'static str, String) {
    let tail = |s: &str| {
        s.trim_end_matches('/')
            .rsplit(['/', ':'])
            .next()
            .unwrap_or(s)
            .split('@')
            .next()
            .unwrap_or(s)
            .to_string()
    };
    match r {
        Request::Mcp(m) => ("server", m.name.clone()),
        Request::Skill(s) => (
            "skill",
            s.path.as_deref().map_or_else(|| tail(&s.source), tail),
        ),
        Request::Plugin(p) => (
            "plugin",
            p.path.as_deref().map_or_else(|| tail(&p.source), tail),
        ),
    }
}

/// `GET /api/extensions/pending`: requests waiting, oldest first, then
/// what the scan suspended.
pub fn pending(ctx: &Ctx) -> Answer {
    let Some(layout) = layout(ctx) else {
        return missing("the data directory");
    };
    let queued = match PendingQueue::new(layout.pending_dir()).list() {
        Ok(q) => q,
        Err(e) => return bad(500, format!("The install requests can't be read: {e}")),
    };
    let mut items: Vec<Value> = queued
        .iter()
        .map(|p| {
            let (kind, name) = kind_and_name(&p.request);
            json!({
                "id": p.id,
                "kind": kind,
                "name": name,
                "source": p.request.source(),
                "what": p.request.describe(),
                "why": p.reason,
                "findings": p.findings.iter().map(finding_json).collect::<Vec<_>>(),
                "status": if p.findings.is_empty() { "pending" } else { "flagged" },
                "at": p.created_at,
            })
        })
        .collect();
    match LockStore::new(layout.lock_path()).load() {
        Ok(lock) => {
            let suspended =
                |kind: &'static str, name: &str, source: &str, at: &str, why: &Option<String>| {
                    json!({
                        "id": name,
                        "kind": kind,
                        "name": name,
                        "source": source,
                        "why": why,
                        "findings": [],
                        "status": "suspended",
                        "at": at,
                    })
                };
            for (n, e) in lock
                .servers
                .iter()
                .filter(|(_, e)| e.status == Status::Suspended)
            {
                items.push(suspended(
                    "server",
                    n,
                    &e.source,
                    &e.installed_at,
                    &e.reason,
                ));
            }
            for (n, e) in lock
                .skills
                .iter()
                .filter(|(_, e)| e.status == Status::Suspended)
            {
                items.push(suspended("skill", n, &e.source, &e.installed_at, &e.reason));
            }
            for (n, e) in lock
                .plugins
                .iter()
                .filter(|(_, e)| e.status == Status::Suspended)
            {
                items.push(suspended(
                    "plugin",
                    n,
                    &e.source,
                    &e.installed_at,
                    &e.reason,
                ));
            }
        }
        Err(e) => tracing::warn!("the extensions lock can't be read: {e}"),
    }
    ok(json!({
        "items": items,
        "approve": "Approve or resume one at the terminal: `ferrule extensions approve <id>` (or `resume <name>`) fetches and scans it there and shows you what it found before you say yes.",
    }))
}

/// `POST /api/extensions/deny {id}`: drops a waiting request; nothing of
/// it was fetched.
pub fn deny(ctx: &Ctx, body: &Value) -> Answer {
    let id = need!(arg(body, "id"));
    let Some(layout) = layout(ctx) else {
        return missing("the data directory");
    };
    let queue = PendingQueue::new(layout.pending_dir());
    let what = match queue.get(id) {
        Ok(Some(p)) => p.request.describe(),
        Ok(None) => {
            return bad(
                404,
                format!("No install request `{id}` is waiting; it may have been answered already."),
            )
        }
        Err(e) => return bad(500, format!("The install request can't be read: {e}")),
    };
    match queue.remove(id) {
        Ok(true) => {
            if let Some(hub) = &ctx.hub {
                hub.audit().record(
                    chrono::Utc::now(),
                    "extension_denied",
                    None,
                    None,
                    json!({ "by": by(), "id": id, "what": what }),
                );
            }
            ok(json!({ "ok": true, "said": format!("Denied: {what}. Nothing was installed.") }))
        }
        Ok(false) => bad(404, format!("Install request `{id}` was already gone.")),
        Err(e) => bad(500, format!("Install request `{id}` wasn't denied: {e}")),
    }
}
