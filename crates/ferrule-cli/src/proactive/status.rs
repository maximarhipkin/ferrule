//! M64.5: what the proactive pass is doing, for `/proactive status` and
//! the dashboard's Proactive page (docs/m64-proactive.md §8): its
//! settings, today's messages and spend against its own caps, the last
//! pass and the next, the Never list, and the kinds waiting after Not now
//! or 👎.

use super::heartbeat::{self, TASK};
use super::store::{self, Store};
use super::{door::kind_words, when, Level, ProactiveConfig};
use crate::config::{self, Config};
use anyhow::Result;
use chrono::Utc;
use chrono_tz::Tz;
use ferrule_gateway::{TaskStore, BUILTIN_CHANNEL};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub settings: ProactiveConfig,
    /// The zone it runs in: `[proactive] timezone`, else `[trust]`'s.
    pub timezone: String,
    pub sent_today: u32,
    pub last_sent: Option<i64>,
    /// Today's proactive dollars; `None` when the ledger can't be read.
    pub spent_today: Option<f64>,
    pub spend_error: Option<String>,
    pub last_pass: Option<LastPass>,
    /// The built-in task, when there is one.
    pub task: Option<PassTask>,
    /// The Never list: each kind, and since when.
    pub never: Vec<Kind>,
    /// Kinds waiting after Not now or 👎, and until when.
    pub resting: Vec<Kind>,
}

/// What the last pass did (the store's `last_pass`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastPass {
    pub at: i64,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PassTask {
    pub id: String,
    pub enabled: bool,
    pub next: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Kind {
    pub kind: String,
    /// What it stops, in words.
    pub words: String,
    /// On the Never list since, or waiting until.
    pub at: i64,
}

/// The built-in task as the scheduler has it now.
pub fn task() -> Result<Option<PassTask>> {
    let db = config::data_dir()?.join("tasks.db");
    if !db.exists() {
        return Ok(None);
    }
    Ok(TaskStore::open(&db)?
        .list()?
        .into_iter()
        .find(|t| t.channel == BUILTIN_CHANNEL && t.name == TASK)
        .map(|t| PassTask {
            id: t.id,
            enabled: t.enabled,
            next: t.next_run_at,
        }))
}

pub fn read(cfg: &Config, tz: Tz, store: &Store) -> Result<Status> {
    let now = Utc::now();
    let (sent_today, last_sent) = heartbeat::sent(store, tz, now)?;
    let (spent_today, spend_error) = match heartbeat::spent_today(tz, now) {
        Ok(usd) => (Some(usd), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    let last_pass = store
        .state("last_pass")?
        .and_then(|v| serde_json::from_str(&v).ok());
    let kind = |(kind, at): (String, i64)| Kind {
        words: kind_words(&kind),
        kind,
        at,
    };
    Ok(Status {
        settings: cfg.proactive.clone(),
        timezone: tz.name().into(),
        sent_today,
        last_sent,
        spent_today,
        spend_error,
        last_pass,
        task: task()?,
        never: store.suppressed()?.into_iter().map(kind).collect(),
        resting: store
            .resting_kinds(store::now())?
            .into_iter()
            .map(kind)
            .collect(),
    })
}

impl Status {
    /// `/proactive status`, in a few lines.
    pub fn words(&self, tz: Tz) -> String {
        let p = &self.settings;
        let mut lines = vec![if p.enabled {
            format!(
                "Proactive is on, at level {}: {}",
                level_name(p.level),
                level_words(p.level)
            )
        } else {
            "Proactive is off. `/proactive on` turns it on (it asks before doing anything until you choose `act`).".into()
        }];
        let quiet = if p.quiet_hours.trim().is_empty() {
            "no quiet hours".to_string()
        } else {
            format!("quiet {}", p.quiet_hours)
        };
        lines.push(format!(
            "Every {} h at :07, {} ({quiet}).",
            p.every_hours, self.timezone
        ));
        match &self.task {
            Some(t) if !t.enabled => lines.push(
                "Its task is paused on the Tasks page: no pass runs until it's resumed.".into(),
            ),
            Some(t) => {
                if let Some(next) = t.next {
                    lines.push(format!("Next pass: {}.", when(next, tz)));
                }
            }
            None if p.enabled => {
                lines.push("No pass is scheduled yet: it's added when the bot starts.".into())
            }
            None => {}
        }
        let spend = match (self.spent_today, &self.spend_error) {
            (Some(usd), _) => format!("${usd:.4} of ${:.2}", p.max_usd_per_day),
            (None, Some(e)) => format!("spend unknown ({e})"),
            (None, None) => "spend unknown".into(),
        };
        lines.push(format!(
            "Today: {} of {} message(s), at least {} min apart; {spend} on passes (a model without prices counts as $0).",
            self.sent_today, p.max_messages_per_day, p.min_gap_minutes
        ));
        lines.push(match &self.last_pass {
            Some(l) => format!("Last pass {}: {}", when(l.at, tz), l.summary),
            None => "No pass has run yet.".into(),
        });
        if !self.never.is_empty() {
            let list: Vec<String> = self
                .never
                .iter()
                .map(|k| format!("{} (`{}`)", k.words, k.kind))
                .collect();
            lines.push(format!("Never: {}.", list.join(", ")));
        }
        for k in &self.resting {
            lines.push(format!(
                "Holding off on {} until {} (Not now or 👎).",
                k.words,
                when(k.at, tz)
            ));
        }
        if p.enabled {
            lines.push(if super::feedback::read() {
                "👍/👎 on its Telegram messages count as feedback.".into()
            } else {
                "👍/👎 on its Telegram messages count from the next restart (it was off when this one started); the buttons work now.".into()
            });
        }
        lines.push("`/proactive on|off`, `level off|suggest|act`, `never <kind or id>`, `clear [kind]`, `patterns`; `/commitments`.".into());
        lines.join("\n")
    }
}

pub fn level_name(level: Level) -> &'static str {
    match level {
        Level::Off => "off",
        Level::Suggest => "suggest",
        Level::Act => "act",
    }
}

/// What a level does, in words.
pub fn level_words(level: Level) -> &'static str {
    match level {
        Level::Off => "it notices and says nothing.",
        Level::Suggest => "every idea is a question with Do it, Not now and Never.",
        Level::Act => "reminders for what you said and your own pattern tasks are made without asking, each with Undo; the rest is asked.",
    }
}
