//! What the bot promised the owner before it restarted: say, in the chat
//! that asked, that it's back. The file survives the restart; it is read
//! and deleted by whoever keeps the promise.

use crate::update::notice::Owner;
use crate::update::state::{Event, EventKind};
use ferrule_trust::ChatRef;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A restart promise older than this is cleared without a word.
const RESTART_FRESH: u64 = 15 * 60;
/// An update promise nobody kept for this long is given up with a line.
const UPDATE_STALE: u64 = 7 * 3600;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    Restart,
    /// `after`: the highest update event id when the owner approved; the
    /// event that keeps the promise has a higher one.
    Update {
        from: String,
        after: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Promise {
    pub channel: String,
    pub chat: String,
    #[serde(flatten)]
    pub kind: Kind,
    pub at: u64,
}

impl Promise {
    pub fn chat(&self) -> ChatRef {
        ChatRef::new(&self.channel, &self.chat)
    }
}

pub fn path(data: &Path) -> PathBuf {
    data.join("self-service").join("promise.json")
}

pub fn exists(data: &Path) -> bool {
    path(data).exists()
}

pub fn make(data: &Path, chat: &ChatRef, kind: Kind) -> anyhow::Result<()> {
    let promise = Promise {
        channel: chat.channel.clone(),
        chat: chat.chat.clone(),
        kind,
        at: crate::update::state::now(),
    };
    let file = path(data);
    ferrule_tools::fs_tools::write_atomic(&file, &serde_json::to_vec(&promise)?)?;
    Ok(())
}

fn read(data: &Path) -> Option<Promise> {
    serde_json::from_slice(&std::fs::read(path(data)).ok()?).ok()
}

/// Reads and deletes.
pub fn take(data: &Path) -> Option<Promise> {
    let p = read(data);
    let _ = std::fs::remove_file(path(data));
    p
}

/// Called a few seconds after the gateway started: a fresh restart promise
/// is kept here; an update promise is left for the update watch.
pub fn on_start(data: &Path, owner: &dyn Owner) {
    let Some(p) = read(data) else {
        return;
    };
    if !matches!(p.kind, Kind::Restart) {
        return;
    }
    let _ = take(data);
    if crate::update::state::now().saturating_sub(p.at) < RESTART_FRESH {
        owner.tell_in(
            &p.chat(),
            format!(
                "Back after the restart (Ferrule {}).",
                crate::update::release::current()
            ),
        );
    }
}

/// The chat an update promise belongs to, when `event` is what it waited
/// for; the promise is used up.
pub fn claim_for(data: &Path, event: &Event) -> Option<ChatRef> {
    if !matches!(event.kind, EventKind::Updated | EventKind::RolledBack) {
        return None;
    }
    let p = read(data)?;
    match &p.kind {
        Kind::Update { after, .. } if event.id > *after => {
            let _ = take(data);
            Some(p.chat())
        }
        _ => None,
    }
}

/// An update promise nobody kept: said once, then gone.
pub fn expire(data: &Path, owner: &dyn Owner) {
    let Some(p) = read(data) else {
        return;
    };
    if matches!(p.kind, Kind::Update { .. })
        && crate::update::state::now().saturating_sub(p.at) > UPDATE_STALE
    {
        let _ = take(data);
        owner.tell_in(
            &p.chat(),
            "I restarted, but couldn't confirm the update; /status shows the version.".into(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::state::now;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Said(Mutex<Vec<(Option<ChatRef>, String)>>);

    #[async_trait]
    impl Owner for Said {
        fn tell(&self, text: String) {
            self.0.lock().unwrap().push((None, text));
        }
        fn tell_in(&self, chat: &ChatRef, text: String) {
            self.0.lock().unwrap().push((Some(chat.clone()), text));
        }
        async fn ask(&self, _: &str, _: &str) -> bool {
            true
        }
    }

    fn event(id: u64, kind: EventKind) -> Event {
        Event {
            id,
            kind,
            from: "0.5.1".into(),
            to: "0.6.0".into(),
            notes: String::new(),
            at: now(),
        }
    }

    fn here() -> ChatRef {
        ChatRef::new("telegram", "42")
    }

    #[test]
    fn a_restart_promise_is_kept_in_the_same_chat() {
        let dir = tempfile::tempdir().unwrap();
        let said = Said::default();
        make(dir.path(), &here(), Kind::Restart).unwrap();
        on_start(dir.path(), &said);
        let told = said.0.lock().unwrap().clone();
        assert_eq!(told.len(), 1);
        assert_eq!(told[0].0, Some(here()));
        assert!(told[0].1.starts_with("Back after the restart"), "{told:?}");
        assert!(!exists(dir.path()), "kept once");
        on_start(dir.path(), &said);
        assert_eq!(said.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_update_promise_is_claimed_by_its_event_and_not_told_twice() {
        let dir = tempfile::tempdir().unwrap();
        let said = Said::default();
        make(
            dir.path(),
            &here(),
            Kind::Update {
                from: "0.5.1".into(),
                after: 4,
            },
        )
        .unwrap();
        // The start leaves it for the update watch.
        on_start(dir.path(), &said);
        assert!(exists(dir.path()));
        assert!(said.0.lock().unwrap().is_empty());
        // An older event, or another kind, doesn't claim it.
        assert_eq!(claim_for(dir.path(), &event(4, EventKind::Updated)), None);
        assert_eq!(claim_for(dir.path(), &event(5, EventKind::Failed)), None);
        assert!(exists(dir.path()));
        // The new event does, once.
        assert_eq!(
            claim_for(dir.path(), &event(5, EventKind::Updated)),
            Some(here())
        );
        assert_eq!(claim_for(dir.path(), &event(6, EventKind::Updated)), None);
    }

    #[test]
    fn an_old_update_promise_is_cleared_with_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let said = Said::default();
        let old = Promise {
            channel: "telegram".into(),
            chat: "42".into(),
            kind: Kind::Update {
                from: "0.5.1".into(),
                after: 0,
            },
            at: now() - UPDATE_STALE - 60,
        };
        let file = path(dir.path());
        ferrule_tools::fs_tools::write_atomic(&file, &serde_json::to_vec(&old).unwrap()).unwrap();
        expire(dir.path(), &said);
        let told = said.0.lock().unwrap().clone();
        assert_eq!(told.len(), 1);
        assert!(
            told[0].1.contains("couldn't confirm the update"),
            "{told:?}"
        );
        assert!(!exists(dir.path()));
        // A fresh one stays.
        make(
            dir.path(),
            &here(),
            Kind::Update {
                from: "0.5.1".into(),
                after: 0,
            },
        )
        .unwrap();
        expire(dir.path(), &said);
        assert!(exists(dir.path()));
        // A stale restart promise is dropped without a word.
        let old = Promise {
            kind: Kind::Restart,
            at: now() - RESTART_FRESH - 60,
            ..old
        };
        ferrule_tools::fs_tools::write_atomic(&file, &serde_json::to_vec(&old).unwrap()).unwrap();
        on_start(dir.path(), &said);
        assert!(!exists(dir.path()));
        assert_eq!(said.0.lock().unwrap().len(), 1);
    }
}
