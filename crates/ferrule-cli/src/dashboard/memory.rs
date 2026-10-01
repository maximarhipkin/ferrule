//! What the bot remembers, on the page (M47 §5c). A window onto
//! `<data>/memory.db` through the same store `ferrule memory` and the
//! agent's tools use: search is the store's keyword ranking, so nothing
//! here calls a model or an embedder.

use super::api::{bad, confirmed, need, ok, Answer};
use super::http::Request;
use super::Ctx;
use ferrule_memory::MemoryStore;
use serde_json::{json, Value};

const DEFAULT: usize = 50;
const MOST: usize = 100;

const NONE_YET: &str =
    "No memories yet. Your bot saves facts when you ask it to remember something.";

/// The live memories, newest first, or the best matches for `q`.
pub async fn list(ctx: &Ctx, req: &Request) -> Answer {
    let Some(data) = &ctx.data else {
        return missing_store();
    };
    let path = data.join("memory.db");
    if !path.exists() {
        return ok(json!({ "available": false, "memories": [], "why": NONE_YET }));
    }
    let limit = req
        .query
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(DEFAULT)
        .clamp(1, MOST);
    let q = req.query.get("q").map(|q| q.trim().to_string());
    let found = tokio::task::spawn_blocking(move || {
        let store = MemoryStore::open(path)?;
        match q.filter(|q| !q.is_empty()) {
            Some(q) => store.recall(&q, limit),
            None => store.recent(limit),
        }
    })
    .await;
    match found {
        Ok(Ok(rows)) => ok(json!({
            "available": true,
            "memories": rows.iter().map(|m| json!({
                "id": m.id,
                "text": m.content,
                "tags": m.tags,
                "at": m.created_at,
            })).collect::<Vec<_>>(),
        })),
        Ok(Err(e)) => bad(500, format!("the memory couldn't be read: {e}")),
        Err(e) => bad(500, format!("the memory couldn't be read: {e}")),
    }
}

/// Deletes a fact and its older versions for good, after a confirm.
pub async fn forget(ctx: &Ctx, body: &Value) -> Answer {
    let Some(data) = &ctx.data else {
        return missing_store();
    };
    let Some(id) = body.get("id").and_then(Value::as_i64) else {
        return bad(400, "`id` is missing");
    };
    need!(confirmed(
        body,
        "Forget this for good? Your bot won't recall it again.".into()
    ));
    let path = data.join("memory.db");
    if !path.exists() {
        return bad(404, format!("There's no memory {id}."));
    }
    let gone = tokio::task::spawn_blocking(move || MemoryStore::open(path)?.forget(id)).await;
    let deleted = match gone {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return bad(500, format!("it couldn't be forgotten: {e}")),
        Err(e) => return bad(500, format!("it couldn't be forgotten: {e}")),
    };
    if deleted.is_empty() {
        return bad(404, format!("There's no memory {id}."));
    }
    if let Some(hub) = &ctx.hub {
        hub.audit().record(
            chrono::Utc::now(),
            "memory.forget",
            None,
            None,
            json!({ "id": id, "by": super::api::by() }),
        );
    }
    let older = deleted.len() - 1;
    let said = match older {
        0 => "Forgot it.".to_string(),
        1 => "Forgot it (and 1 older version).".to_string(),
        n => format!("Forgot it (and {n} older versions)."),
    };
    ok(json!({ "ok": true, "said": said }))
}

fn missing_store() -> Answer {
    ok(json!({ "available": false, "memories": [], "why": NONE_YET }))
}
