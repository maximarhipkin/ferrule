//! M64: requests the owner repeats, turned into offers to schedule them
//! (docs/m64-proactive.md §4). Counting, not a model: the owner's kept
//! messages from the last 28 days and their one-off tasks, as sets of
//! words, clustered greedily; a cluster with 3 or more texts on 3 or more
//! days around the same time of day is a candidate. The schedule, the name
//! and the prompt are ferrule's, from the owner's own words; a model only
//! phrases the question (the pass, M64.4), so nothing it reads can change
//! what would run.

use super::store::{self, Action, Store};
use crate::config;
use anyhow::Result;
use chrono::{DateTime, Datelike, Timelike};
use chrono_tz::Tz;
use ferrule_gateway::{TaskKind, TaskStore, BUILTIN_CHANNEL};
use ferrule_trust::{ChatRef, Hub};
use std::collections::{BTreeMap, BTreeSet};

/// The window counted.
pub const WINDOW_SECS: i64 = 28 * 86_400;
/// A pattern offered (or noticed) once isn't offered again for this long.
pub const REOFFER_SECS: i64 = 30 * 86_400;
const MIN_TEXTS: usize = 3;
const MIN_DAYS: usize = 3;
/// Every text within this many minutes of the median time of day.
const SPREAD_MINUTES: i64 = 90;
const SIMILAR: f64 = 0.5;

/// Words that say nothing about what is asked for (English; other
/// languages' words are kept as they are).
const STOP: &[&str] = &[
    "the", "and", "for", "with", "are", "was", "this", "that", "what", "whats", "how", "can",
    "could", "please", "pls", "tell", "give", "show", "get", "does", "today", "tomorrow", "now",
    "about", "there", "any", "some", "from", "just", "like", "want", "need", "would", "will",
    "let", "lets", "hey", "hello", "thanks", "thank", "you", "your", "our", "its",
];

/// A request the owner repeats, and the task that would answer it.
#[derive(Debug, Clone)]
pub struct Pattern {
    /// Its suppression key, after `pattern:`: its commonest words.
    pub key: String,
    /// How many texts, on how many days.
    pub texts: usize,
    pub days: usize,
    /// `every day at 08:00`, `on weekdays at 08:00`, `on Mondays at 08:00`.
    pub how_often: String,
    pub cron: String,
    pub timezone: String,
    /// The owner's most recent wording: the task's prompt.
    pub prompt: String,
}

impl Pattern {
    pub fn kind(&self) -> String {
        format!("pattern:{}", self.key)
    }

    /// The task it would schedule.
    pub fn action(&self) -> Action {
        Action::Task {
            name: self.key.replace('-', " "),
            schedule: self.cron.clone(),
            timezone: self.timezone.clone(),
            prompt: self.prompt.clone(),
        }
    }

    /// The question without a model: what the pass asks when it can't
    /// phrase one, and what `/proactive patterns` shows.
    pub fn question(&self) -> String {
        format!(
            "You asked \"{}\" {} times on {} days, around the same time. Schedule it {}?",
            ferrule_gateway::health::clip(&self.prompt, 80),
            self.texts,
            self.days,
            self.how_often
        )
    }
}

struct Seen {
    at: i64,
    text: String,
    words: BTreeSet<String>,
}

/// A text's words: lower-cased letters and digits, two letters or more
/// beyond the stop words, no bare numbers.
pub fn words(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 2 || (!w.is_empty() && !w.is_ascii()))
        .filter(|w| !STOP.contains(w) && !w.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
        .collect()
}

pub fn similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// The candidates now, best first: not covered by one of the owner's
/// tasks, not on the Never list, not offered in the last 30 days.
pub fn candidates(hub: &Hub, store: &Store, tz: Tz) -> Result<Vec<Pattern>> {
    let now = store::now();
    let mut seen: Vec<Seen> = store
        .requests_since(now - WINDOW_SECS)?
        .into_iter()
        .map(|r| Seen {
            at: r.at,
            words: words(&r.text),
            text: r.text,
        })
        .collect();
    let (one_offs, covered) = owner_tasks(hub)?;
    seen.extend(
        one_offs
            .into_iter()
            .filter(|(at, _)| *at >= now - WINDOW_SECS)
            .map(|(at, text)| Seen {
                at,
                words: words(&text),
                text,
            }),
    );
    seen.retain(|s| !s.words.is_empty());
    seen.sort_by_key(|s| s.at);
    let mut clusters: Vec<Vec<&Seen>> = Vec::new();
    for s in &seen {
        match clusters
            .iter_mut()
            .find(|c| similarity(&c[0].words, &s.words) >= SIMILAR)
        {
            Some(c) => c.push(s),
            None => clusters.push(vec![s]),
        }
    }
    let recent: Vec<String> = store
        .items_since(now - REOFFER_SECS)?
        .into_iter()
        .map(|i| i.kind)
        .collect();
    let mut out: Vec<Pattern> = Vec::new();
    for c in clusters {
        let Some(p) = pattern(&c, tz) else { continue };
        let kind = p.kind();
        if covered
            .iter()
            .any(|w| similarity(w, &c[0].words) >= SIMILAR)
            || recent.contains(&kind)
            || store.is_suppressed(&kind)?
            || out.iter().any(|o| o.key == p.key)
        {
            continue;
        }
        out.push(p);
    }
    out.sort_by(|a, b| b.texts.cmp(&a.texts).then(b.days.cmp(&a.days)));
    Ok(out)
}

/// The owner's one-off tasks: when each was set for, and its prompt.
type OneOffs = Vec<(i64, String)>;

/// The owner's one-off tasks, and the word sets of the owner's recurring
/// tasks, which a pattern they already answer is left out for.
fn owner_tasks(hub: &Hub) -> Result<(OneOffs, Vec<BTreeSet<String>>)> {
    let db = config::data_dir()?.join("tasks.db");
    if !db.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let owners = hub.owners();
    let (mut once, mut recurring) = (Vec::new(), Vec::new());
    for t in TaskStore::open(&db)?.list()? {
        let theirs = owners.contains(&ChatRef::new(&t.channel, &t.chat_id))
            || (t.channel == crate::dashboard::chat::CHANNEL
                && t.chat_id == crate::dashboard::chat::CHAT);
        if !theirs || t.channel == BUILTIN_CHANNEL {
            continue;
        }
        match t.kind {
            TaskKind::Once => {
                let at = DateTime::parse_from_rfc3339(&t.schedule)
                    .map(|d| d.timestamp())
                    .unwrap_or(t.created_at);
                once.push((at, t.prompt));
            }
            TaskKind::Cron if t.enabled => recurring.push(words(&t.prompt)),
            TaskKind::Cron => {}
        }
    }
    Ok((once, recurring))
}

/// A cluster as a pattern, when it is one.
fn pattern(c: &[&Seen], tz: Tz) -> Option<Pattern> {
    if c.len() < MIN_TEXTS {
        return None;
    }
    let local: Vec<_> = c
        .iter()
        .filter_map(|s| DateTime::from_timestamp(s.at, 0))
        .map(|t| t.with_timezone(&tz))
        .collect();
    let days: BTreeSet<_> = local.iter().map(|t| t.date_naive()).collect();
    if days.len() < MIN_DAYS {
        return None;
    }
    let mut minutes: Vec<i64> = local
        .iter()
        .map(|t| i64::from(t.hour()) * 60 + i64::from(t.minute()))
        .collect();
    minutes.sort_unstable();
    let median = minutes[minutes.len() / 2];
    if minutes.iter().any(|m| (m - median).abs() > SPREAD_MINUTES) {
        return None;
    }
    // A little before the owner usually asks.
    let at = median / 15 * 15;
    let (h, m) = (at / 60, at % 60);
    let weekdays: BTreeSet<u32> = days
        .iter()
        .map(|d| d.weekday().num_days_from_sunday())
        .collect();
    let clock = format!("{h:02}:{m:02}");
    let (dow, how_often) = match weekdays.iter().collect::<Vec<_>>().as_slice() {
        [one] => (
            one.to_string(),
            format!("on {}s at {clock}", WEEKDAY[**one as usize]),
        ),
        _ if weekdays.iter().all(|d| (1..=5).contains(d)) && days.len() >= 4 => {
            ("1-5".to_string(), format!("on weekdays at {clock}"))
        }
        _ => ("*".to_string(), format!("every day at {clock}")),
    };
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for s in c {
        for w in &s.words {
            *counts.entry(w.as_str()).or_default() += 1;
        }
    }
    let mut top: Vec<(&str, usize)> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut key: Vec<&str> = top.iter().take(3).map(|(w, _)| *w).collect();
    key.sort_unstable();
    let latest = c.iter().max_by_key(|s| s.at)?;
    Some(Pattern {
        key: key.join("-"),
        texts: c.len(),
        days: days.len(),
        how_often,
        cron: format!("{m} {h} * * {dow}"),
        timezone: tz.name().to_string(),
        prompt: latest.text.clone(),
    })
}

const WEEKDAY: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
