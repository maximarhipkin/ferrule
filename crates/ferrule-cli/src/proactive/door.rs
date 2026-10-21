//! M64: the proactive agent's door, the last of the gateway's
//! interceptors. It records each plain message the owner sends in their
//! own private chat or the dashboard's chat (with its time, while proactive
//! is on: the pass extracts commitments from them and counts patterns over
//! them), and it reads the owner's `/commitments` and the answers the
//! proactive messages' buttons send (`/proactive do|later|never|useful|
//! undo <id>`). Everything here is the owner's only.

use super::store::{self, Action, Item, Store};
use super::{audit, reminder, settings, undo_button, when};
use crate::trust::by_owner;
use ferrule_gateway::{Button, ButtonAction, InboundMessage};
use ferrule_trust::Hub;
use serde_json::json;
use std::sync::Arc;

/// How long a suggestion's buttons work.
pub const ANSWER_FOR_SECS: i64 = 7 * 86_400;

/// Longest owner message kept.
const REQUEST_CHARS: usize = 500;

pub struct ProactiveDoor {
    pub hub: Arc<Hub>,
}

impl ProactiveDoor {
    /// The commands read here.
    #[cfg(test)]
    pub const HANDLES: &'static [&'static str] = &["commitments"];
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
            return plain("Only the owner can answer the proactive agent.");
        }
        match args {
            [verb @ ("do" | "later" | "never" | "useful" | "undo"), id] => {
                answer(&self.hub, verb, id).await
            }
            _ => plain(
                "The buttons under a proactive message send `/proactive do|later|never|useful|undo <id>`.",
            ),
        }
    }
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
        "do" | "later" | "never" if item.outcome.as_deref().is_some_and(|o| o != "useful") => {
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
            plain("OK, not now.")
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
