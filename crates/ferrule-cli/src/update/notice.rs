//! The gateway's side (docs/m36-self-update.md §3.5): one owner line per
//! update, rollback or failure the apply unit recorded; without the units, a
//! daily look at the releases and one "vX is out" per version; with
//! `auto = false`, one install question per version, whose Allow asks the
//! apply unit to run.

use super::release::{self, Source};
use super::state::{self, Event, EventKind, Request, State, Told};
use super::Channel;
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The first look, after the gateway has settled.
const FIRST: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(600);
/// The gateway's own release check, without the units.
const CHECK_EVERY: u64 = 24 * 3600;
/// With no `told.json` yet, only this recent an event is told.
const RECENT: u64 = 24 * 3600;
/// How long the install question stays open.
const ASK_FOR: Duration = Duration::from_secs(12 * 3600);

/// Who hears about updates.
#[async_trait]
pub trait Owner: Send + Sync {
    fn tell(&self, text: String);
    /// `true` on Allow.
    async fn ask(&self, subject: &str, question: &str) -> bool;
}

/// The owner's chat, through the M19 hub.
pub struct HubOwner(pub Arc<ferrule_trust::Hub>);

#[async_trait]
impl Owner for HubOwner {
    fn tell(&self, text: String) {
        self.0.tell_owner(text);
    }

    async fn ask(&self, subject: &str, question: &str) -> bool {
        self.0
            .ask_owner("update", subject, question, ASK_FOR)
            .await
            .is_ok()
    }
}

pub struct Watch {
    /// The gateway's data dir.
    pub data: PathBuf,
    pub auto: Option<bool>,
    pub channel: Channel,
    /// The apply units are installed (they check and install; the gateway
    /// only tells).
    pub units: bool,
    pub source: Source,
    pub current: semver::Version,
    pub target: String,
    /// Added to the daily check's day, so installs don't all ask at once.
    pub jitter: u64,
}

impl Watch {
    pub fn new(data: PathBuf, settings: &crate::config::UpdateConfig) -> Self {
        Watch {
            data,
            auto: settings.auto,
            channel: settings.channel,
            units: super::units_installed(),
            source: Source::github(),
            current: release::current(),
            target: release::TARGET.to_string(),
            jitter: u64::from(uuid::Uuid::new_v4().as_u128() as u16) % 7200,
        }
    }

    /// Tell what's new; ask or offer what's out.
    pub async fn tick(&self, owner: &Arc<dyn Owner>) -> Result<()> {
        let state = State::load(&super::state_dir(&self.data));
        let loaded = Told::load(&self.data);
        let mut told = loaded.clone().unwrap_or_default();
        let last = state.events.last().map_or(0, |e| e.id);
        // No told file yet, or a state that started over: only what's recent.
        let fresh = loaded.is_none() || told.id > last;
        if told.id > last {
            told.id = 0;
        }
        for event in state.events.iter().filter(|e| e.id > told.id) {
            if fresh && state::now().saturating_sub(event.at) > RECENT {
                continue;
            }
            if event.kind == EventKind::Failed {
                // One line per version that won't install, not one a day.
                if told.failed.as_deref() == Some(event.to.as_str()) {
                    continue;
                }
                told.failed = Some(event.to.clone());
            }
            if let Some(text) = event_text(event) {
                owner.tell(text);
            }
        }
        told.id = last;
        self.offer(&state, &mut told, owner).await;
        told.save(&self.data)
    }

    async fn offer(&self, state: &State, told: &mut Told, owner: &Arc<dyn Owner>) {
        if self.units {
            if self.auto != Some(false) {
                return;
            }
            // The unit's daily check found it; the owner decides.
            let Some(tag) = state.latest.clone() else {
                return;
            };
            let newer = release::parse_version(&tag).is_some_and(|v| v > self.current);
            if !newer || state.is_pinned(&tag) || told.offered.as_deref() == Some(tag.as_str()) {
                return;
            }
            told.offered = Some(tag.clone());
            let (owner, data, current) = (owner.clone(), self.data.clone(), self.current.clone());
            tokio::spawn(async move {
                let question = format!(
                    "Ferrule {tag} is out (this is {current}). Install it now? \
                     It goes in when no task is running, and comes back to {current} \
                     if it doesn't start properly."
                );
                if owner.ask(&format!("update {tag}"), &question).await {
                    let request = Request {
                        ferrule: true,
                        ..Request::default()
                    };
                    if let Err(e) = state::write_request(&data, &request) {
                        owner.tell(format!("Couldn't start the update: {e:#}"));
                    }
                }
            });
            return;
        }
        let due = told
            .checked
            .is_none_or(|at| state::now() >= at + CHECK_EVERY + self.jitter);
        if !due {
            return;
        }
        told.checked = Some(state::now());
        let releases = match self.source.list().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("update check: {e:#}");
                return;
            }
        };
        let Some(found) = release::choose(
            &releases,
            &self.current,
            self.channel,
            &state.pinned,
            &self.target,
        ) else {
            return;
        };
        if told.offered.as_deref() != Some(found.tag.as_str()) {
            told.offered = Some(found.tag.clone());
            owner.tell(format!(
                "Ferrule {} is out: run `ferrule update`.",
                found.tag
            ));
        }
    }
}

/// The owner's line for an event, if it gets one.
pub fn event_text(e: &Event) -> Option<String> {
    Some(match e.kind {
        EventKind::Updated if e.notes.is_empty() => {
            format!("Updated Ferrule {} → {}.", e.from, e.to)
        }
        EventKind::Updated => format!("Updated Ferrule {} → {}: {}", e.from, e.to, e.notes),
        EventKind::RolledBack => format!(
            "Ferrule {to} didn't start properly, so I went back to {from} and won't try {to} \
             again. `ferrule update --to v{to}` retries it.",
            to = e.to,
            from = e.from
        ),
        EventKind::Failed => format!(
            "Ferrule {} is out but wasn't installed: {}",
            e.to,
            release::clip(&e.notes, 300)
        ),
        EventKind::ClaudeUpdated => {
            format!("Updated the claude CLI {} → {}.", e.from, e.to)
        }
        EventKind::ClaudeFailed => format!(
            "The claude CLI couldn't be updated: {}",
            release::clip(&e.notes, 300)
        ),
    })
}

/// Tell and ask from the gateway, every ten minutes.
pub fn spawn(watch: Watch, owner: Arc<dyn Owner>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST).await;
        loop {
            if let Err(e) = watch.tick(&owner).await {
                tracing::debug!("update notices: {e:#}");
            }
            tokio::time::sleep(EVERY).await;
        }
    })
}
