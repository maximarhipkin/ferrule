//! What the bot remembers, on the page (M47 §5c). A window onto
//! `<data>/memory.db` through the same store `ferrule memory` and the
//! agent's tools use: search is the store's keyword ranking, so nothing
//! here calls a model or an embedder. The learning page's upkeep: each
//! fact's use (how often recall put it in a prompt, and when), its pin and
//! who wrote it; `?view=stale` lists what's never been recalled in 30 days;
//! the owner adds, edits and pins facts here, through the chat's paste
//! guard. Viewing or searching here doesn't count as a recall.

use super::api::{arg, bad, by, confirmed, need, ok, Answer};
use super::http::Request;
use super::Ctx;
use ferrule_gateway::paste_guard;
use ferrule_memory::{Memory, MemoryStore, FAILURE_NOTE, OWNER};
use serde_json::{json, Value};

const DEFAULT: usize = 50;
const MOST: usize = 100;
/// A fact this old that recall never used is stale.
const STALE_DAYS: i64 = 30;
/// The longest fact the page may write.
const MAX_FACT: usize = 2_000;

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
    let stale = req.query.get("view").is_some_and(|v| v == "stale");
    let found = tokio::task::spawn_blocking(move || {
        let store = MemoryStore::open(path)?;
        if stale {
            return store.stale(STALE_DAYS, limit);
        }
        match q.filter(|q| !q.is_empty()) {
            Some(q) => store.recall(&q, limit),
            None => store.recent(limit),
        }
    })
    .await;
    match found {
        Ok(Ok(rows)) => ok(json!({
            "available": true,
            "memories": rows.iter().map(row).collect::<Vec<_>>(),
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

/// One fact as the page lists it.
fn row(m: &Memory) -> Value {
    json!({
        "id": m.id,
        "text": m.content,
        "tags": m.tags,
        "at": m.created_at,
        "recalls": m.recalls,
        "last_recalled": m.last_recalled,
        "pinned": m.pinned,
        "source": source(m),
    })
}

/// Who wrote it: the row's source, a failure note by its tag, else
/// `unknown` (a fact from before sources were kept).
fn source(m: &Memory) -> String {
    match &m.source {
        Some(s) => s.clone(),
        None if ferrule_memory::is_note(m) => FAILURE_NOTE.into(),
        None => "unknown".into(),
    }
}

/// The page's writes: `memory/add`, `memory/edit`, `memory/pin`.
pub async fn post(ctx: &Ctx, path: &str, body: &Value) -> Answer {
    let Some(data) = &ctx.data else {
        return bad(503, "the data directory isn't available in this process");
    };
    let db = data.join("memory.db");
    match path {
        "memory/add" => add(ctx, db, body).await,
        "memory/edit" => edit(ctx, db, body).await,
        "memory/pin" => pin(ctx, db, body).await,
        _ => None,
    }
}

/// A fact's text as the page sent it: refused when it looks like a key or
/// a token (the chat's own guard and words), or too long.
fn fact(ctx: &Ctx, body: &Value) -> Result<String, Answer> {
    let text = arg(body, "text")?;
    if text.chars().count() > MAX_FACT {
        return Err(bad(413, format!("keep a fact under {MAX_FACT} characters")));
    }
    if paste_guard::holds_a_key(text, &ctx.redactor) {
        return Err(bad(
            400,
            super::feedback::pasted_key_then("Nothing was saved."),
        ));
    }
    Ok(text.to_string())
}

fn audit(ctx: &Ctx, event: &str, detail: Value) {
    if let Some(hub) = &ctx.hub {
        hub.audit()
            .record(chrono::Utc::now(), event, None, None, detail);
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ferrule_memory::MemoryError> + Send + 'static,
) -> Result<T, Answer> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(ferrule_memory::MemoryError::Refused(why))) => Err(bad(409, why)),
        Ok(Err(e)) => Err(bad(500, format!("the memory couldn't be changed: {e}"))),
        Err(e) => Err(bad(500, format!("the memory couldn't be changed: {e}"))),
    }
}

/// `POST /api/memory/add {text, tags?}`: a fact from the owner.
async fn add(ctx: &Ctx, db: std::path::PathBuf, body: &Value) -> Answer {
    let text = need!(fact(ctx, body));
    let tags: Vec<String> = match &body["tags"] {
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Value::String(s) => s.split(',').map(str::to_string).collect(),
        _ => Vec::new(),
    }
    .into_iter()
    .map(|t| t.trim().to_string())
    .filter(|t| !t.is_empty() && !t.contains(','))
    .collect();
    let ins = need!(
        blocking(move || {
            let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
            MemoryStore::open(&db)?.insert_from(&text, &tags, &[], Some(OWNER))
        })
        .await
    );
    let said = match ins.decision {
        ferrule_memory::Decision::Noop => format!("Already remembered (#{}).", ins.id),
        _ => format!("Remembered (#{}).", ins.id),
    };
    audit(ctx, "memory.add", json!({ "id": ins.id, "by": by() }));
    ok(json!({ "ok": true, "said": said, "id": ins.id }))
}

/// `POST /api/memory/edit {id, text}`: the owner's correction; the old
/// text stays as the fact's history.
async fn edit(ctx: &Ctx, db: std::path::PathBuf, body: &Value) -> Answer {
    let Some(id) = body.get("id").and_then(Value::as_i64) else {
        return bad(400, "`id` is missing");
    };
    let text = need!(fact(ctx, body));
    let ins = need!(blocking(move || MemoryStore::open(&db)?.edit(id, &text, OWNER)).await);
    audit(
        ctx,
        "memory.edit",
        json!({ "id": id, "now": ins.id, "by": by() }),
    );
    let said = if ins.id == id {
        "Nothing changed: it already says that.".to_string()
    } else {
        format!("Updated: #{} replaces #{id}.", ins.id)
    };
    ok(json!({ "ok": true, "said": said, "id": ins.id }))
}

/// `POST /api/memory/pin {id, pinned}`: a pinned fact never fades and is
/// preferred in recall.
async fn pin(ctx: &Ctx, db: std::path::PathBuf, body: &Value) -> Answer {
    let Some(id) = body.get("id").and_then(Value::as_i64) else {
        return bad(400, "`id` is missing");
    };
    let Some(pinned) = body.get("pinned").and_then(Value::as_bool) else {
        return bad(400, "`pinned` takes true or false");
    };
    let found = need!(blocking(move || MemoryStore::open(&db)?.set_pinned(id, pinned)).await);
    if !found {
        return bad(404, format!("There's no live memory {id}."));
    }
    audit(
        ctx,
        "memory.pin",
        json!({ "id": id, "pinned": pinned, "by": by() }),
    );
    let said = if pinned {
        format!("Pinned #{id}: it won't fade and comes first when it matches.")
    } else {
        format!("Unpinned #{id}.")
    };
    ok(json!({ "ok": true, "said": said }))
}

fn missing_store() -> Answer {
    ok(json!({ "available": false, "memories": [], "why": NONE_YET }))
}
