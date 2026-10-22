//! M64.5: the owner's 👍/👎 on the proactive agent's messages
//! (docs/m64-proactive.md §7), read off the same Telegram reaction sink as
//! M53.5's learning loop. A reaction on a message the pass or a button
//! sent is the proactive store's (its item's id is kept with the message's
//! id); any other goes on to learning's sink, when learning is on. Only
//! the owner's 👍 or 👎 counts: 👎 backs the item's kind off like Not now,
//! 👍 puts it back to normal.

use super::store::Store;
use super::{audit, door::kind_words};
use ferrule_gateway::{Reaction, ReactionSink};
use ferrule_learn::thumbs;
use ferrule_trust::Hub;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Whether this process asked Telegram for reactions for the proactive
/// agent (it was on when the gateway started).
static READ: AtomicBool = AtomicBool::new(false);

pub fn read() -> bool {
    READ.load(Ordering::Relaxed)
}

/// The sink the channels get: the proactive agent's reactions here,
/// `then` (learning's) for the rest. `None` when neither wants any, so
/// Telegram isn't asked for reactions at all.
pub fn sink(hub: Option<Arc<Hub>>, then: Option<ReactionSink>) -> Option<ReactionSink> {
    let Some(hub) = hub else {
        return then;
    };
    READ.store(true, Ordering::Relaxed);
    Some(Arc::new(move |r| {
        let (hub, then) = (hub.clone(), then.clone());
        let run = move || {
            if !reacted(&hub, &r) {
                if let Some(then) = then {
                    then(r);
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn_blocking(run);
            }
            Err(_) => run(),
        }
    }))
}

/// Whether `r` was on one of the proactive agent's messages; the owner's
/// 👍 or 👎 on one is kept on its item and its kind's feedback.
fn reacted(hub: &Hub, r: &Reaction) -> bool {
    let store = match Store::open_default() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("proactive: a reaction wasn't read: {e:#}");
            return false;
        }
    };
    let item = match store.item_by_message(&r.channel, &r.chat_id, &r.message_id) {
        Ok(Some(i)) => i,
        Ok(None) => return false,
        Err(e) => {
            tracing::warn!("proactive: a reaction wasn't read: {e:#}");
            return false;
        }
    };
    let set: Vec<&str> = r
        .emoji
        .iter()
        .map(String::as_str)
        .filter(|e| matches!(*e, thumbs::UP | thumbs::DOWN))
        .collect();
    // Neither, both, or taken back: no verdict.
    let [thumb] = set.as_slice() else {
        return true;
    };
    if hub.owner_on(&r.channel).map(|o| o.chat).as_ref() != Some(&r.user_id) {
        return true;
    }
    let verdict = if *thumb == thumbs::UP { "up" } else { "down" };
    if let Err(e) = store.add_feedback(&item, verdict) {
        tracing::warn!(item = %item.id, "proactive: the owner's {thumb} wasn't kept: {e:#}");
        return true;
    }
    // An answer (Do it, Not now, Never, Undo) stays the item's outcome;
    // the thumb is kept beside it.
    if matches!(
        item.outcome.as_deref(),
        None | Some("useful" | "up" | "down")
    ) {
        let _ = store.set_outcome(&item.id, verdict);
    }
    let until = store
        .resting(&item.kind, super::store::now())
        .ok()
        .flatten();
    audit(
        hub,
        "feedback",
        json!({"item": item.id, "kind": item.kind, "verdict": verdict, "resting_until": until}),
    );
    tracing::info!(
        item = %item.id,
        "proactive: the owner's {thumb} on {} is kept",
        kind_words(&item.kind)
    );
    true
}
