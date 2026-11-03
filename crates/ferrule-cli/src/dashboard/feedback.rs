//! The owner's 👍/👎 on a reply in the page's own chat (`POST
//! /api/chat/react`, `docs/m16-learning-loop.md`, "Signals"). Kept always,
//! learning on or off, as a line of `learn/signals.jsonl`: which reply,
//! when, the owner's "why" if given, never the reply's text. A 👎 also
//! keeps the chat's transcript cut where that reply ended, so the next
//! pass takes it as an episode, as M53.5 does with a Telegram 👎.
//!
//! The "why" goes to the reflector's model, so it passes the chat's own
//! paste guard (M52.5): one that looks like it holds a key or a token is
//! refused, in the chat's words, and nothing is noted; what's kept is
//! scrubbed of the known secrets as well.

use super::api::{arg, bad, ok, Answer};
use super::chat::{CHANNEL, CHAT};
use super::Ctx;
use ferrule_gateway::{paste_guard, Redactor};
use ferrule_learn::LearnDir;
use serde_json::{json, Value};

/// The most of the owner's "why" the page may send.
const MAX_WHY_BYTES: usize = 4 * 1024;

/// What the page says for a "why" that looks like a key: the chat's own
/// words for a pasted key (`paste_guard::reply`), then that nothing was
/// noted.
fn pasted_key() -> String {
    let words = paste_guard::reply(true, Some("Add it on the Connect page instead."));
    let mut parts = words.split("\n\n");
    let first = parts.next().unwrap_or_default();
    let stopped = first.split_once(". ").map_or(first, |(s, _)| s);
    let keys = parts.next().unwrap_or_default();
    format!("{stopped}. {keys} Nothing was noted: send your 👍/👎 again without it.")
}

/// The owner's "why", ready to keep: refused when it looks like it holds
/// a key or a token, else with the known secrets taken out.
fn checked_why(why: Option<&str>, redactor: &Redactor) -> Result<Option<String>, String> {
    let Some(why) = why.map(str::trim).filter(|w| !w.is_empty()) else {
        return Ok(None);
    };
    if paste_guard::holds_a_key(why, redactor) {
        return Err(pasted_key());
    }
    Ok(Some(ferrule_core::scrub::text(redactor.redact(why))))
}

/// `POST /api/chat/react {id, thumb: "up"|"down", why?}`.
pub fn react(ctx: &Ctx, body: &Value) -> Answer {
    let n = match &body["id"] {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    let Some(n) = n else {
        return bad(400, "`id` is the reply's id in the page's chat");
    };
    let id = n.to_string();
    let up = match arg(body, "thumb") {
        Ok("up") => true,
        Ok("down") => false,
        Ok(other) => return bad(400, format!("`thumb` is up or down, not `{other}`")),
        Err(answer) => return answer,
    };
    let why = body.get("why").and_then(Value::as_str);
    if why.is_some_and(|w| w.len() > MAX_WHY_BYTES) {
        return bad(413, "that's a long why: keep it under 4 KB");
    }
    let why = match checked_why(why, &ctx.redactor) {
        Ok(w) => w,
        Err(said) => return bad(400, said),
    };
    let why = why.as_deref();
    let Some(ch) = &ctx.chat else {
        return bad(
            503,
            "chat from the page works when the dashboard runs inside the gateway (the service)",
        );
    };
    let view = ch.since(0);
    let entries = view["entries"].as_array().cloned().unwrap_or_default();
    let Some(at) = entries.iter().position(|e| e["id"] == n) else {
        return bad(
            404,
            format!("there's no reply {id} in the page's chat any more; nothing was noted"),
        );
    };
    if entries[at]["who"] != "agent" {
        return bad(
            400,
            "👍 and 👎 go on the agent's replies, not your own messages",
        );
    }
    // Nothing came after it: the transcript as it stands ends with it.
    let latest = at + 1 == entries.len() && view["waiting"] != true;
    let session = ferrule_gateway::session::session_id(CHANNEL, CHAT);
    let learnable = match crate::learn::live_thumbs() {
        Some(t) => t.page_react(&session, &id, up, why, latest),
        None => match &ctx.data {
            Some(data) => ferrule_learn::thumbs::signal(
                &LearnDir::new(data),
                CHANNEL,
                &session,
                &id,
                up,
                why,
                None,
                chrono::Utc::now().timestamp(),
            ),
            None => Err("there's no data directory to keep it in".to_string()),
        },
    };
    match learnable {
        Ok(learnable) if up || learnable => ok(json!({ "ok": true, "said": "Thanks — noted." })),
        Ok(_) => ok(json!({
            "ok": true,
            "said": "Thanks — noted. That reply is too far back to re-run, so it counts as feedback only.",
        })),
        Err(e) => bad(500, format!("Not noted: {}", ctx.redactor.redact(&e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_why_that_looks_like_a_key_is_refused_in_the_chats_words() {
        let r = Redactor::new(["machine-secret-value".to_string()]);
        let said = checked_why(
            Some("it used ghp_a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8 again"),
            &r,
        )
        .unwrap_err();
        assert!(
            said.starts_with("That looked like an API key or a token, so I stopped it here"),
            "{said}"
        );
        assert!(said.contains("Keys don't go in a chat") && said.contains("Nothing was noted"));
        assert!(!said.contains("I deleted"), "{said}");
        assert!(checked_why(Some("it leaked machine-secret-value"), &r).is_err());
        assert_eq!(
            checked_why(Some("  too vague  "), &r).unwrap().as_deref(),
            Some("too vague")
        );
        assert_eq!(checked_why(Some(" "), &r).unwrap(), None);
    }
}
