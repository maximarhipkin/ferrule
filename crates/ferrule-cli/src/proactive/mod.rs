//! M64: the proactive agent (docs/m64-proactive.md). The owner's only, off
//! until the owner turns it on: commitments and open loops learned from
//! the owner's own messages, reminders through the scheduler, and every
//! idea offered with Do it / Not now / Never, or, at `act`, done with a
//! receipt and Undo.

pub mod door;
pub mod loops;
pub mod reminder;
pub mod store;
pub mod tool;

use crate::config::Config;
use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use ferrule_trust::{ChatRef, Hub};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use store::{Action, Commitment, NewItem, Store};

/// `[proactive]` (docs/m64-proactive.md §9).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ProactiveConfig {
    /// Off until the owner turns it on.
    pub enabled: bool,
    pub level: Level,
    /// How often the pass runs, in hours (1–24).
    pub every_hours: u32,
    /// `22:00-07:00` in the owner's timezone: no pass. Empty: none.
    pub quiet_hours: String,
    /// The owner's IANA zone; unset: `[trust] timezone`.
    pub timezone: Option<String>,
    pub max_messages_per_day: u32,
    pub min_gap_minutes: u32,
    /// The passes' own spend a day, inside the `[trust]` caps.
    pub max_usd_per_day: f64,
    /// The pass's model (a reference or alias); unset: `[routing]`'s first
    /// tier when routing is on, else the default model.
    pub model: Option<String>,
    /// Read the next 24 hours of connected calendars.
    pub calendar: bool,
    /// Read unread mail headers from connected mailboxes.
    pub mail: bool,
    /// Look for requests the owner repeats.
    pub patterns: bool,
}

impl Default for ProactiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            level: Level::Suggest,
            every_hours: 3,
            quiet_hours: "22:00-07:00".into(),
            timezone: None,
            max_messages_per_day: 3,
            min_gap_minutes: 120,
            max_usd_per_day: 0.25,
            model: None,
            calendar: true,
            mail: true,
            patterns: true,
        }
    }
}

/// How far the pass goes on its own (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Notices, says nothing.
    Off,
    /// Every idea is a question with buttons.
    Suggest,
    /// Low-risk actions are done, with a receipt and Undo.
    Act,
}

impl ProactiveConfig {
    /// What parsing can't check.
    pub fn check(&self, trust_tz: &str) -> Result<()> {
        if !(1..=24).contains(&self.every_hours) {
            bail!(
                "[proactive] every_hours must be 1 to 24, not {}",
                self.every_hours
            );
        }
        quiet(&self.quiet_hours).map_err(|e| anyhow!("[proactive] quiet_hours: {e}"))?;
        self.tz(trust_tz)?;
        if !self.max_usd_per_day.is_finite() || self.max_usd_per_day < 0.0 {
            bail!("[proactive] max_usd_per_day must be 0 or more");
        }
        if self.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
            bail!("[proactive] model is empty: leave it out for the default");
        }
        Ok(())
    }

    /// The owner's timezone: `[proactive] timezone`, else `trust_tz`.
    pub fn tz(&self, trust_tz: &str) -> Result<Tz> {
        let name = self.timezone.as_deref().unwrap_or(trust_tz);
        name.parse()
            .map_err(|_| anyhow!("[proactive] timezone `{name}` isn't an IANA zone"))
    }
}

/// `HH:MM-HH:MM` as minutes of the day; `None` for an empty string.
pub fn quiet(spec: &str) -> Result<Option<(u32, u32)>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(None);
    }
    let minutes = |s: &str| -> Result<u32, String> {
        let t = NaiveTime::parse_from_str(s.trim(), "%H:%M")
            .map_err(|_| format!("`{s}` isn't a time like 22:00"))?;
        Ok(t.format("%H").to_string().parse::<u32>().unwrap_or(0) * 60
            + t.format("%M").to_string().parse::<u32>().unwrap_or(0))
    };
    let (from, to) = spec
        .split_once('-')
        .ok_or_else(|| format!("`{spec}` isn't a range like 22:00-07:00"))?;
    Ok(Some((minutes(from)?, minutes(to)?)))
}

/// Whether proactive is on in this process: the config's at the start,
/// then whatever `/proactive on|off` and the page said. The door records
/// the owner's messages and the owner's sessions get the `commitments`
/// tool only while it is.
static ON: AtomicBool = AtomicBool::new(false);

pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

pub fn set_on(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

/// The settings as the config file has them now, with the owner's zone.
pub fn settings() -> Result<(Config, Tz)> {
    let (cfg, _) = Config::load()?;
    let tz = cfg.proactive.tz(&cfg.trust.timezone)?;
    Ok((cfg, tz))
}

/// `Fri 24 Oct 09:00` in the owner's zone.
pub fn when(at: i64, tz: Tz) -> String {
    DateTime::from_timestamp(at, 0)
        .map(|t| t.with_timezone(&tz).format("%a %-d %b %H:%M").to_string())
        .unwrap_or_else(|| at.to_string())
}

/// `Fri 24 Oct` in the owner's zone.
pub fn day(at: i64, tz: Tz) -> String {
    DateTime::from_timestamp(at, 0)
        .map(|t| t.with_timezone(&tz).format("%a %-d %b").to_string())
        .unwrap_or_else(|| at.to_string())
}

/// A due time as the owner or the model writes it: `2026-10-24` (a day),
/// `2026-10-24T17:00` or `2026-10-24 17:00` (the owner's clock), or a full
/// RFC 3339 time. The second value says it named a day.
pub fn parse_due(text: &str, tz: Tz) -> Result<(i64, bool), String> {
    let text = text.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(text) {
        return Ok((t.timestamp(), false));
    }
    if let Ok(d) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        let noon = d.and_hms_opt(0, 0, 0).unwrap_or_default();
        return local(noon, tz).map(|t| (t, true));
    }
    for f in ["%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(text, f) {
            return local(t, tz).map(|t| (t, false));
        }
    }
    Err(format!(
        "`{text}` isn't a time like 2026-10-24, 2026-10-24T17:00 or an RFC 3339 time"
    ))
}

fn local(t: NaiveDateTime, tz: Tz) -> Result<i64, String> {
    tz.from_local_datetime(&t)
        .earliest()
        .map(|t| t.timestamp())
        .ok_or_else(|| format!("{t} doesn't exist in {tz}: the clocks skip it"))
}

/// When to remind about something due at `due`: 09:00 on the day when
/// only a day was said, else an hour before (a quarter of an hour when
/// that has passed). `None` when that time has passed too.
pub fn reminder_time(due: i64, due_day: bool, tz: Tz, now: i64) -> Option<i64> {
    let at = if due_day {
        let day = DateTime::from_timestamp(due, 0)?
            .with_timezone(&tz)
            .date_naive();
        local(day.and_hms_opt(9, 0, 0)?, tz).ok()?
    } else if due - 3600 > now {
        due - 3600
    } else {
        due - 900
    };
    (at > now).then_some(at)
}

/// What the owner sees for a commitment's due time.
pub fn due_words(c: &Commitment, tz: Tz) -> Option<String> {
    let due = c.due?;
    Some(if c.due_day {
        day(due, tz)
    } else {
        when(due, tz)
    })
}

/// The buttons under a suggestion.
pub fn suggestion_buttons(item: &str) -> Vec<(String, String)> {
    vec![
        ("Do it".into(), format!("/proactive do {item}")),
        ("Not now".into(), format!("/proactive later {item}")),
        ("Never".into(), format!("/proactive never {item}")),
    ]
}

/// The button under a receipt.
pub fn undo_button(item: &str) -> (String, String) {
    ("Undo".into(), format!("/proactive undo {item}"))
}

/// Records a proactive event in the trust audit log (`proactive_*`).
pub fn audit(hub: &Hub, event: &str, detail: serde_json::Value) {
    hub.audit().record(
        Utc::now(),
        &format!("proactive_{event}"),
        None,
        None,
        detail,
    );
}

/// What became of offering a reminder for a commitment just noted.
pub enum Offered {
    /// Nothing to remind about, or it's too late, or reminders are on the
    /// Never list, or the level is `off`; the reason.
    Not(String),
    /// Asked, with the buttons, for this time.
    Asked(i64),
    /// Done at `act`, with a receipt and Undo, for this time.
    Done(i64),
    /// It should have gone out and didn't: why.
    Failed(String),
}

/// A commitment with a due time gets a reminder offered (`suggest`) or
/// made (`act`) through the scheduler (docs/m64-proactive.md §6), in
/// `here` (the chat it was said in) or the owner's primary chat. Its
/// basis is the owner's own words, so `act` may do it.
pub async fn offer_reminder(
    hub: &Hub,
    here: Option<&ChatRef>,
    store: &Store,
    cfg: &ProactiveConfig,
    tz: Tz,
    c: &Commitment,
) -> Offered {
    let Some(due) = c.due else {
        return Offered::Not("no due time was said".into());
    };
    let now = store::now();
    let Some(at) = reminder_time(due, c.due_day, tz, now) else {
        return Offered::Not("it's due too soon to remind".into());
    };
    if cfg.level == Level::Off {
        return Offered::Not("proactive's level is off: it only notices".into());
    }
    if store.is_suppressed("reminder").unwrap_or(false) {
        return Offered::Not("reminders are on the Never list (/proactive never)".into());
    }
    let action = Action::Reminder {
        at,
        text: c.text.clone(),
    };
    let act = cfg.level == Level::Act;
    let text = if act {
        format!("I'll remind you {}: {}", when(at, tz), c.text)
    } else {
        format!("Remind you {}: {}?", when(at, tz), c.text)
    };
    let item = match store.add_item(&NewItem {
        kind: "reminder".into(),
        mode: if act { "done" } else { "suggest" }.into(),
        text: text.clone(),
        action: Some(action.clone()),
        basis: "owner".into(),
        commitment: Some(c.id),
    }) {
        Ok(i) => i,
        Err(e) => return Offered::Failed(format!("the proactive store: {e:#}")),
    };
    if act {
        return match door::perform(hub, store, &item, "proactive (act)").await {
            Ok(said) => {
                let message = format!("Done without asking: {said}");
                let sent = deliver(
                    hub,
                    here,
                    store,
                    &item.id,
                    &message,
                    vec![undo_button(&item.id)],
                )
                .await;
                audit(
                    hub,
                    "done",
                    json!({"item": item.id, "kind": item.kind, "commitment": c.id, "sent": sent.is_ok()}),
                );
                match sent {
                    Ok(()) => Offered::Done(at),
                    Err(e) => Offered::Failed(format!(
                        "the reminder is set, but the receipt didn't go out: {e}"
                    )),
                }
            }
            Err(e) => {
                let _ = store.set_outcome(&item.id, "failed");
                Offered::Failed(e)
            }
        };
    }
    let sent = deliver(
        hub,
        here,
        store,
        &item.id,
        &text,
        suggestion_buttons(&item.id),
    )
    .await;
    audit(
        hub,
        "suggested",
        json!({"item": item.id, "kind": item.kind, "commitment": c.id, "sent": sent.is_ok()}),
    );
    match sent {
        Ok(()) => Offered::Asked(at),
        Err(e) => {
            let _ = store.set_outcome(&item.id, "failed");
            Offered::Failed(e)
        }
    }
}

/// Sends `text` with `buttons` to `here` (an owner chat) or the owner's
/// primary chat and waits; keeps where it went on the item, for its 👍/👎.
pub async fn deliver(
    hub: &Hub,
    here: Option<&ChatRef>,
    store: &Store,
    item: &str,
    text: &str,
    buttons: Vec<(String, String)>,
) -> Result<(), String> {
    let (chat, id) = hub.post_in(here, text, &buttons).await?;
    if let Some(id) = id {
        let _ = store.set_message(item, &chat.channel, &chat.chat, &id);
    }
    Ok(())
}
