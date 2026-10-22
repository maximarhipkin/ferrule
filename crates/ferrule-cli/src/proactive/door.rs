//! M64: the proactive agent's door, the last of the gateway's
//! interceptors. It records each plain message the owner sends in their
//! own private chat or the dashboard's chat (with its time, while proactive
//! is on: the pass extracts commitments from them and counts patterns over
//! them), and it reads the owner's `/commitments`, `/proactive` (M64.5:
//! `status`, `on`, `off`, `level`, `never`, `clear`, `patterns`) and the
//! answers the proactive messages' buttons send (`/proactive do|later|
//! never|useful|undo <id>`). Everything here is the owner's only; the chat
//! model never reaches these settings.

use super::store::{self, Action, Item, Store};
use super::{audit, reminder, settings, undo_button, when};
use crate::trust::by_owner;
use ferrule_gateway::{Button, ButtonAction, InboundMessage};
use ferrule_trust::Hub;
use serde_json::{json, Value};
use std::sync::Arc;

/// How long a suggestion's buttons work.
pub const ANSWER_FOR_SECS: i64 = 7 * 86_400;

/// Longest owner message kept.
const REQUEST_CHARS: usize = 500;

pub struct ProactiveDoor {
    pub hub: Arc<Hub>,
    /// M64.5: the page's handlers (or the gateway's own context), so
    /// `/proactive on|off|level` writes the config the way the page does.
    pub admin: Option<Arc<crate::self_service::Admin>>,
}

impl ProactiveDoor {
    /// The commands read here.
    #[cfg(test)]
    pub const HANDLES: &'static [&'static str] = &["commitments", "proactive"];
}

/// `/name` (and `/name@bot`) as the first word, lower-cased.
fn command(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    let name = first.strip_prefix('/')?.split('@').next()?;
    Some(name.to_ascii_lowercase())
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for ProactiveDoor {
    async fn intercept(&self, msg: &InboundMessage) -> Option<String> {
        self.intercept_with_buttons(msg).await.map(|(text, _)| text)
    }

    async fn intercept_with_buttons(&self, msg: &InboundMessage) -> Option<(String, Vec<Button>)> {
        let Some(name) = command(&msg.text) else {
            self.record(msg);
            return None;
        };
        let args: Vec<&str> = msg.text.split_whitespace().skip(1).collect();
        let reply = match name.as_str() {
            "commitments" => commitments(&self.hub, msg, &args),
            "proactive" => self.proactive(msg, &args).await,
            _ => return None,
        };
        Some(reply)
    }
}

impl ProactiveDoor {
    /// Keeps the owner's own message, when proactive is on and it's the
    /// owner's private chat or the dashboard's. Nobody else's message is
    /// ever kept.
    fn record(&self, msg: &InboundMessage) {
        if !super::on() || msg.text.trim().is_empty() || !by_owner(&self.hub, msg) {
            return;
        }
        let session = ferrule_gateway::session::session_id(&msg.channel, &msg.chat_id);
        if crate::self_service::offered(&self.hub, &session).is_none() {
            return;
        }
        let text = ferrule_gateway::health::clip(msg.text.trim(), REQUEST_CHARS);
        let at = if msg.ts > 0 { msg.ts } else { store::now() };
        if let Err(e) = Store::open_default().and_then(|s| s.add_request(at, &session, &text)) {
            tracing::warn!("proactive: the owner's message wasn't kept: {e:#}");
        }
    }

    async fn proactive(&self, msg: &InboundMessage, args: &[&str]) -> (String, Vec<Button>) {
        if !by_owner(&self.hub, msg) {
            return plain("The proactive agent is the owner's only.");
        }
        match args {
            [] | ["status"] => status(),
            ["on"] => self.set(json!({"enabled": true})).await,
            ["off"] => self.set(json!({"enabled": false})).await,
            ["level", level] => self.set(json!({"level": level.to_ascii_lowercase()})).await,
            ["never"] => never_list(),
            ["never", what] => never(&self.hub, what).await,
            ["clear"] | ["clear", "all"] => clear(&self.hub, None),
            ["clear", kind] => clear(&self.hub, Some(kind)),
            [verb @ ("do" | "later" | "useful" | "undo"), id] => answer(&self.hub, verb, id).await,
            ["patterns"] => patterns(&self.hub, msg).await,
            _ => plain(
                "Usage: `/proactive [status]`, `/proactive on|off`, `/proactive level off|suggest|act`, `/proactive never [<kind> or <id>]`, `/proactive clear [<kind>|all]`, `/proactive patterns`. The buttons under a proactive message send `/proactive do|later|never|useful|undo <id>`.",
            ),
        }
    }

    /// `[proactive]` changed through the page's own handler (checked with
    /// the real loader, guarded, the file kept as a version), then the
    /// pass's task made to match. The owner typed it, so it's confirmed.
    async fn set(&self, mut body: Value) -> (String, Vec<Button>) {
        let Some(ctx) = self.admin.as_ref().and_then(|a| a.ctx()) else {
            return plain("Not changed: the settings can't be written from this process.");
        };
        body["confirm"] = json!(true);
        let by = "the owner (/proactive)";
        match crate::dashboard::api::act(ctx, "proactive/settings", &body, by).await {
            Some((200, v)) => plain(v["said"].as_str().unwrap_or("Saved.").to_string()),
            Some((_, v)) => plain(format!(
                "Not changed: {}",
                v["error"].as_str().unwrap_or("it failed")
            )),
            None => plain("Not changed: the settings aren't available here."),
        }
    }
}

/// `/proactive status`.
fn status() -> (String, Vec<Button>) {
    let (cfg, tz) = match settings() {
        Ok(s) => s,
        Err(e) => return plain(format!("The settings can't be read: {e:#}")),
    };
    match Store::open_default().and_then(|s| super::status::read(&cfg, tz, &s)) {
        Ok(st) => plain(st.words(tz)),
        Err(e) => plain(format!("The proactive store can't be read: {e:#}")),
    }
}

/// Whether `kind` is something the Never list can hold.
pub fn known_kind(kind: &str) -> bool {
    matches!(kind, "reminder" | "loop" | "calendar" | "mail" | "note")
        || kind
            .strip_prefix("pattern:")
            .is_some_and(|k| !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '-'))
}

/// `/proactive never`: the list, and what's holding off.
fn never_list() -> (String, Vec<Button>) {
    let tz = settings().map(|(_, tz)| tz).unwrap_or(chrono_tz::UTC);
    let read =
        Store::open_default().and_then(|s| Ok((s.suppressed()?, s.resting_kinds(store::now())?)));
    let (never, resting) = match read {
        Ok(r) => r,
        Err(e) => return plain(format!("The proactive store can't be read: {e:#}")),
    };
    let mut lines = Vec::new();
    if never.is_empty() {
        lines.push("The Never list is empty.".to_string());
    } else {
        lines.push("Never (`/proactive clear <kind>` brings one back, `/proactive clear all` all of them):".into());
        lines.extend(
            never.iter().map(|(k, at)| {
                format!("- {} (`{k}`), since {}", kind_words(k), super::day(*at, tz))
            }),
        );
    }
    if !resting.is_empty() {
        lines.push("Holding off after Not now or 👎:".into());
        lines.extend(
            resting.iter().map(|(k, until)| {
                format!("- {} (`{k}`) until {}", kind_words(k), when(*until, tz))
            }),
        );
    }
    lines.push("Add one: `/proactive never reminder|loop|calendar|mail|note`, `pattern:<key>`, or a message's id.".into());
    plain(lines.join("\n"))
}

/// `/proactive never <id or kind>`: an item's id answers it with Never (its
/// kind goes on the list); a kind goes on the list by itself.
async fn never(hub: &Hub, what: &str) -> (String, Vec<Button>) {
    let store = match Store::open_default() {
        Ok(s) => s,
        Err(e) => return plain(format!("Not done: the proactive store: {e:#}")),
    };
    if matches!(store.item(what), Ok(Some(_))) {
        return answer(hub, "never", what).await;
    }
    let kind = what.to_ascii_lowercase();
    if !known_kind(&kind) {
        return plain(format!(
            "`{what}` isn't a proactive message's id or a kind: reminder, loop, calendar, mail, note or pattern:<key>."
        ));
    }
    if let Err(e) = store.suppress(&kind, None) {
        return plain(format!("Not done: the proactive store: {e:#}"));
    }
    audit(hub, "suppressed", json!({"kind": kind, "by": "command"}));
    plain(format!(
        "OK: no more {}. `/proactive clear {kind}` brings them back.",
        kind_words(&kind)
    ))
}

/// Takes `kind` (or, with `None`, everything) off the Never list: what
/// happened, in words. The page's Clear and `/proactive clear` both.
pub fn clear_never(
    hub: Option<&Hub>,
    store: &Store,
    kind: Option<&str>,
    by: &str,
) -> anyhow::Result<String> {
    let said = match kind {
        None => match store.clear_suppressed()? {
            0 => return Ok("The Never list was empty already.".into()),
            n => format!("Cleared the Never list ({n} kind(s)); they can come again."),
        },
        Some(k) => {
            if !store.unsuppress(k)? {
                return Ok(format!("`{k}` isn't on the Never list."));
            }
            format!("{} can come again.", capital(&kind_words(k)))
        }
    };
    if let Some(hub) = hub {
        audit(
            hub,
            "cleared",
            json!({"kind": kind.unwrap_or("all"), "by": by}),
        );
    }
    Ok(said)
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// `/proactive clear [kind|all]`.
fn clear(hub: &Hub, kind: Option<&str>) -> (String, Vec<Button>) {
    let kind = kind.map(str::to_ascii_lowercase);
    match Store::open_default().and_then(|s| {
        clear_never(
            Some(hub),
            &s,
            kind.as_deref(),
            "the owner (/proactive clear)",
        )
    }) {
        Ok(said) => plain(said),
        Err(e) => plain(format!("Not done: the proactive store: {e:#}")),
    }
}

/// Patterns offered at once, at most this many to a command.
const PATTERNS_SHOWN: usize = 3;

/// `/proactive patterns`: the requests the owner repeats, each offered in
/// its own message with Do it / Not now / Never. Asked, never done
/// unasked, whatever the level: the owner asked to see them.
async fn patterns(hub: &Hub, msg: &InboundMessage) -> (String, Vec<Button>) {
    let (cfg, tz) = match settings() {
        Ok(s) => s,
        Err(e) => return plain(format!("The settings can't be read: {e:#}")),
    };
    if !cfg.proactive.patterns {
        return plain("Pattern offers are off ([proactive] patterns = false).");
    }
    let store = match Store::open_default() {
        Ok(s) => s,
        Err(e) => return plain(format!("The proactive store can't be read: {e:#}")),
    };
    let found = match super::patterns::candidates(hub, &store, tz) {
        Ok(f) => f,
        Err(e) => return plain(format!("The patterns couldn't be counted: {e:#}")),
    };
    let kept = if super::on() {
        "your messages since proactive was turned on"
    } else {
        "your one-off tasks only (proactive is off, so your messages aren't kept)"
    };
    if found.is_empty() {
        return plain(format!(
            "No new pattern: nothing asked 3 times on 3 days around the same time, in the last 28 days of {kept}."
        ));
    }
    let here = ferrule_gateway::session::session_id(&msg.channel, &msg.chat_id);
    let here = crate::self_service::offered(hub, &here);
    let mut lines = vec![format!(
        "{} pattern(s) in the last 28 days of {kept}; each comes as its own question.",
        found.len()
    )];
    for p in found.iter().take(PATTERNS_SHOWN) {
        let new = store::NewItem {
            kind: p.kind(),
            mode: String::new(),
            text: String::new(),
            action: Some(p.action()),
            basis: "owner".into(),
            commitment: None,
        };
        if let super::Offered::Failed(why) = super::offer(
            hub,
            here.as_ref(),
            &store,
            super::Level::Suggest,
            new,
            p.question(),
        )
        .await
        {
            lines.push(format!("\"{}\" couldn't be offered: {why}", p.key));
        }
    }
    if found.len() > PATTERNS_SHOWN {
        lines.push(format!(
            "{} more after these are answered.",
            found.len() - PATTERNS_SHOWN
        ));
    }
    plain(lines.join("\n"))
}

fn plain(text: impl Into<String>) -> (String, Vec<Button>) {
    (text.into(), Vec::new())
}

fn button((label, reply): (String, String)) -> Button {
    Button {
        text: label,
        action: ButtonAction::Command(reply),
    }
}

/// What "Never" on `kind` stops, in words.
pub fn kind_words(kind: &str) -> String {
    match kind {
        "reminder" => "reminder offers".into(),
        "loop" => "nudges about unfinished things".into(),
        "calendar" => "notes about your calendar".into(),
        "mail" => "notes about your mail".into(),
        "note" => "notes and summaries".into(),
        k => match k.strip_prefix("pattern:") {
            Some(p) => format!("the offer to automate \"{}\"", p.replace('-', " ")),
            None => k.to_string(),
        },
    }
}

/// Runs an item's action (Do it, or `act`): what it did, in words. The
/// item gets its Undo (the task made) and the outcome `done`.
pub async fn perform(hub: &Hub, store: &Store, item: &Item, by: &str) -> Result<String, String> {
    let tz = settings().map(|(_, tz)| tz).unwrap_or(chrono_tz::UTC);
    let (task, said) = match &item.action {
        None => return Err("there's nothing to do for that one".into()),
        Some(Action::Reminder { at, text }) => {
            let id = reminder::create(hub, *at, text, by).map_err(|e| format!("{e:#}"))?;
            if let Some(c) = item.commitment {
                let _ = store.set_reminder(c, Some(&id));
            }
            (id, format!("a reminder {}: {text}", when(*at, tz)))
        }
        Some(Action::Task {
            name,
            schedule,
            timezone,
            prompt,
        }) => {
            let here = hub.primary().ok_or("no owner chat is set")?;
            let admin = crate::tasks_admin::TasksAdmin::open().map_err(|e| format!("{e:#}"))?;
            let (task, next) = admin
                .add(
                    crate::tasks_admin::NewPageTask {
                        name: name.clone(),
                        prompt: prompt.clone(),
                        kind: ferrule_gateway::TaskKind::Cron,
                        schedule: schedule.clone(),
                        timezone: timezone.clone(),
                        channel: here.channel.clone(),
                        chat_id: here.chat.clone(),
                        model: None,
                    },
                    by,
                )
                .map_err(|e| format!("{e:#}"))?;
            let next = next.map(|n| format!(", first {}", when(n, tz)));
            (
                task.id,
                format!(
                    "the task \"{name}\" ({schedule}, {timezone}{})",
                    next.unwrap_or_default()
                ),
            )
        }
    };
    let _ = store.set_undo(&item.id, Some(&task));
    let _ = store.set_outcome(&item.id, "done");
    Ok(said)
}

/// A button's answer on item `id`.
async fn answer(hub: &Hub, verb: &str, id: &str) -> (String, Vec<Button>) {
    let store = match Store::open_default() {
        Ok(s) => s,
        Err(e) => return plain(format!("Not done: the proactive store: {e:#}")),
    };
    let item = match store.item(id) {
        Ok(Some(i)) => i,
        Ok(None) => return plain(format!("There's no proactive item {id}.")),
        Err(e) => return plain(format!("Not done: the proactive store: {e:#}")),
    };
    let by = "the owner (proactive button)";
    let answered = |outcome: &str| {
        let _ = store.set_outcome(&item.id, outcome);
        audit(
            hub,
            "answered",
            json!({"item": item.id, "kind": item.kind, "answer": outcome}),
        );
    };
    match verb {
        // A thumb or Useful is feedback, not an answer.
        "do" | "later" | "never"
            if item
                .outcome
                .as_deref()
                .is_some_and(|o| !matches!(o, "useful" | "up" | "down")) =>
        {
            plain(format!(
                "That one was answered already ({}).",
                item.outcome
                    .as_deref()
                    .unwrap_or_default()
                    .replace('_', " ")
            ))
        }
        "do" if store::now() - item.at > ANSWER_FOR_SECS => {
            let _ = store.set_outcome(&item.id, "expired");
            plain(
                "That offer is over a week old; it has lapsed. Ask me again if you still want it.",
            )
        }
        "do" => match perform(hub, &store, &item, by).await {
            Ok(said) => {
                audit(
                    hub,
                    "answered",
                    json!({"item": item.id, "kind": item.kind, "answer": "done"}),
                );
                (
                    format!("Done: {said}."),
                    vec![button(undo_button(&item.id))],
                )
            }
            Err(why) => {
                let _ = store.set_outcome(&item.id, "failed");
                plain(format!("Not done: {why}."))
            }
        },
        "later" => {
            answered("not_now");
            // M64.5: Not now backs the kind off (§7).
            let _ = store.add_feedback(&item, "not_now");
            let tz = settings().map(|(_, tz)| tz).unwrap_or(chrono_tz::UTC);
            match store.resting(&item.kind, store::now()).ok().flatten() {
                Some(until) => plain(format!(
                    "OK, not now: no more {} until {}.",
                    kind_words(&item.kind),
                    when(until, tz)
                )),
                None => plain("OK, not now."),
            }
        }
        "never" => {
            answered("never");
            if let Err(e) = store.suppress(&item.kind, Some(&item.id)) {
                return plain(format!("Not done: the proactive store: {e:#}"));
            }
            audit(
                hub,
                "suppressed",
                json!({"kind": item.kind, "item": item.id}),
            );
            plain(format!(
                "OK: no more {}. /proactive never lists what's off and clears it.",
                kind_words(&item.kind)
            ))
        }
        "useful" => {
            answered("useful");
            // M64.5: Useful ends a back-off of its kind.
            let _ = store.add_feedback(&item, "useful");
            plain("Thanks, noted.")
        }
        _ => undo(hub, &store, &item, by),
    }
}

/// Undo: deletes the task the item's action made.
fn undo(hub: &Hub, store: &Store, item: &Item, by: &str) -> (String, Vec<Button>) {
    let Some(task) = &item.undo else {
        return plain("There's nothing to undo for that one.");
    };
    let gone = match &item.action {
        Some(Action::Reminder { .. }) => {
            reminder::delete(hub, task, by).map_err(|e| format!("{e:#}"))
        }
        _ => crate::tasks_admin::TasksAdmin::open()
            .and_then(|a| a.delete(task, by))
            .map(|_| true)
            .map_err(|e| format!("{e:#}")),
    };
    match gone {
        Ok(existed) => {
            let _ = store.set_undo(&item.id, None);
            let _ = store.set_outcome(&item.id, "undone");
            if let Some(c) = item.commitment {
                let _ = store.set_reminder(c, None);
            }
            audit(
                hub,
                "undone",
                json!({"item": item.id, "kind": item.kind, "task": task}),
            );
            plain(if existed {
                "Undone: the task it made is deleted."
            } else {
                "Undone: the task it made was gone already."
            })
        }
        Err(why) => plain(format!("Not undone: {why}.")),
    }
}

/// `/commitments`: the open ones; `done|drop <id>`, `snooze <id> [days]`,
/// `all`.
fn commitments(hub: &Hub, msg: &InboundMessage, args: &[&str]) -> (String, Vec<Button>) {
    if !by_owner(hub, msg) {
        return plain("Only the owner's commitments are kept, and only the owner sees them.");
    }
    let store = match Store::open_default() {
        Ok(s) => s,
        Err(e) => return plain(format!("The proactive store can't be read: {e:#}")),
    };
    let tz = settings().map(|(_, tz)| tz).unwrap_or(chrono_tz::UTC);
    let id = |w: Option<&&str>| w.and_then(|w| w.trim_start_matches('#').parse::<i64>().ok());
    let change = |id: Option<i64>, status: &str, until: Option<i64>, said: &str| {
        let Some(id) = id else {
            return plain("Which one? `/commitments` numbers them.");
        };
        match store.set_status(id, status, until) {
            Ok(true) => {
                audit(hub, "commitment", json!({"id": id, "status": status}));
                plain(format!("#{id} {said}."))
            }
            Ok(false) => plain(format!("There's no commitment #{id}.")),
            Err(e) => plain(format!("Not done: {e:#}")),
        }
    };
    match args {
        ["done", rest @ ..] => change(id(rest.first()), "done", None, "is done"),
        ["drop", rest @ ..] => change(id(rest.first()), "dropped", None, "is dropped"),
        ["snooze", rest @ ..] => {
            let days = rest
                .get(1)
                .and_then(|d| d.parse::<i64>().ok())
                .filter(|d| (1..=365).contains(d))
                .unwrap_or(1);
            let until = store::now() + days * 86_400;
            change(
                id(rest.first()),
                "snoozed",
                Some(until),
                &format!("is snoozed until {}", when(until, tz)),
            )
        }
        [] | ["all"] => match super::loops::sync(hub, &store)
            .and_then(|_| store.commitments(!args.is_empty()))
        {
            Ok(list) if list.is_empty() => plain(if super::on() {
                "Nothing tracked. I note what you say you'll do, and you can ask me to track something."
            } else {
                "Nothing tracked. Proactive is off: `/proactive on` (or the dashboard's Proactive page) turns it on."
            }),
            Ok(list) => {
                let mut lines = vec![if args.is_empty() {
                    format!("Open commitments ({}):", list.len())
                } else {
                    format!("Every commitment, newest first ({}):", list.len())
                }];
                lines.extend(list.iter().map(|c| line(c, tz)));
                lines.push("`/commitments done|drop <id>`, `/commitments snooze <id> [days]`.".into());
                plain(lines.join("\n"))
            }
            Err(e) => plain(format!("The proactive store can't be read: {e:#}")),
        },
        _ => plain("Usage: /commitments [all], /commitments done|drop <id>, /commitments snooze <id> [days]."),
    }
}

/// One commitment, for the chat.
pub fn line(c: &store::Commitment, tz: chrono_tz::Tz) -> String {
    let mut s = format!("#{} ", c.id);
    if c.kind == "loop" {
        s.push_str("[unfinished] ");
    }
    s.push_str(&c.text);
    if let Some(due) = super::due_words(c, tz) {
        s.push_str(&format!(", due {due}"));
    }
    if c.reminder.is_some() {
        s.push_str(" (reminder set)");
    }
    if c.status != "open" {
        s.push_str(&format!(" [{}]", c.status));
    }
    s
}
