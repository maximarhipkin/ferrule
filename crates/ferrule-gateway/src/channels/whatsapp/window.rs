//! M39 §3.4: WhatsApp's 24-hour window, per chat, and what waits while it
//! is closed. `windows.json` holds each chat's last inbound time,
//! `held.json` the messages that couldn't go (≤ 20 a chat, ≤ 7 days). Both
//! live under `<data>/gateway/whatsapp/` and survive a restart.

use crate::message::Attachment;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// WhatsApp's customer service window.
pub const WINDOW_SECS: i64 = 24 * 3600;
/// A send this close to the window's end is treated as closed: Meta's
/// clock and ours needn't agree to the second.
const MARGIN_SECS: i64 = 60;
pub const HELD_MAX: usize = 20;
pub const HELD_DAYS: i64 = 7;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Held {
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Unix seconds.
    pub at: i64,
}

/// Whether a chat's window is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Open,
    Closed,
    /// Nobody wrote since ferrule started keeping track: try, and let
    /// Meta's answer (or a `failed` status) say.
    Unknown,
}

#[derive(Default, Serialize, Deserialize)]
struct HeldFile {
    #[serde(default)]
    chats: BTreeMap<String, Vec<Held>>,
    /// Messages dropped from a full queue, per chat, said at the flush.
    #[serde(default)]
    dropped: BTreeMap<String, usize>,
}

pub struct Windows {
    dir: Option<PathBuf>,
    last: Mutex<BTreeMap<String, i64>>,
    held: Mutex<HeldFile>,
}

impl Windows {
    /// Kept in `dir`; `None` keeps it in memory only.
    pub fn open(dir: Option<PathBuf>) -> Self {
        let read = |name: &str| {
            dir.as_ref()
                .and_then(|d| std::fs::read_to_string(d.join(name)).ok())
                .unwrap_or_default()
        };
        let last = serde_json::from_str(&read("windows.json")).unwrap_or_default();
        let held = serde_json::from_str(&read("held.json")).unwrap_or_default();
        Self {
            dir,
            last: Mutex::new(last),
            held: Mutex::new(held),
        }
    }

    /// `chat` wrote at `at`: its window is open for 24 hours.
    pub fn opened(&self, chat: &str, at: i64) {
        let mut last = self.last.lock().unwrap();
        let at = at.max(last.get(chat).copied().unwrap_or(0));
        last.insert(chat.to_string(), at);
        self.write("windows.json", &*last);
    }

    /// Meta said the window is closed (131047): -1 marks it closed until
    /// they write again.
    pub fn closed(&self, chat: &str) {
        let mut last = self.last.lock().unwrap();
        last.insert(chat.to_string(), -1);
        self.write("windows.json", &*last);
    }

    pub fn state(&self, chat: &str, now: i64) -> Window {
        match self.last.lock().unwrap().get(chat) {
            None => Window::Unknown,
            Some(&t) if t >= 0 && now - t < WINDOW_SECS - MARGIN_SECS => Window::Open,
            Some(_) => Window::Closed,
        }
    }

    /// Keeps a message for `chat`; whether the queue was empty before (the
    /// first held message is the one a template announces).
    pub fn hold(&self, chat: &str, text: &str, attachments: &[Attachment], now: i64) -> bool {
        let mut h = self.held.lock().unwrap();
        let queue = h.chats.entry(chat.to_string()).or_default();
        let before = queue.len();
        queue.retain(|m| now - m.at < HELD_DAYS * 86_400);
        let mut dropped = before - queue.len();
        let first = queue.is_empty();
        queue.push(Held {
            text: text.to_string(),
            attachments: attachments.to_vec(),
            at: now,
        });
        if queue.len() > HELD_MAX {
            let over = queue.len() - HELD_MAX;
            queue.drain(..over);
            dropped += over;
        }
        if dropped > 0 {
            tracing::warn!(
                "whatsapp: {dropped} held message(s) for {chat} dropped (older than {HELD_DAYS} days, or over {HELD_MAX})"
            );
            *h.dropped.entry(chat.to_string()).or_default() += dropped;
        }
        self.write("held.json", &*h);
        first
    }

    /// Takes `chat`'s held messages, and how many were dropped.
    pub fn take(&self, chat: &str, now: i64) -> (Vec<Held>, usize) {
        let mut h = self.held.lock().unwrap();
        let mut queue = h.chats.remove(chat).unwrap_or_default();
        let mut dropped = h.dropped.remove(chat).unwrap_or(0);
        let before = queue.len();
        queue.retain(|m| now - m.at < HELD_DAYS * 86_400);
        dropped += before - queue.len();
        if !queue.is_empty() || dropped > 0 {
            self.write("held.json", &*h);
        }
        (queue, dropped)
    }

    /// Puts messages back at the head of `chat`'s queue (a flush that
    /// failed part-way).
    pub fn put_back(&self, chat: &str, mut msgs: Vec<Held>) {
        if msgs.is_empty() {
            return;
        }
        let mut h = self.held.lock().unwrap();
        let queue = h.chats.entry(chat.to_string()).or_default();
        msgs.append(queue);
        *queue = msgs;
        self.write("held.json", &*h);
    }

    /// Chat → how many are held.
    pub fn held_counts(&self) -> BTreeMap<String, usize> {
        self.held
            .lock()
            .unwrap()
            .chats
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(c, q)| (c.clone(), q.len()))
            .collect()
    }

    fn write<T: Serialize>(&self, name: &str, value: &T) {
        let Some(dir) = &self.dir else { return };
        let body = match serde_json::to_string_pretty(value) {
            Ok(b) => b,
            Err(_) => return,
        };
        let tmp = dir.join(format!(".{name}.tmp"));
        let r = std::fs::create_dir_all(dir)
            .and_then(|_| std::fs::write(&tmp, body))
            .and_then(|_| std::fs::rename(&tmp, dir.join(name)));
        if let Err(e) = r {
            tracing::warn!(error = %e, "whatsapp: couldn't save {name}");
        }
    }
}

/// What `held.json` holds, for doctor: chat → count (≤ 7 days old).
pub fn held_on_disk(dir: &std::path::Path, now: i64) -> BTreeMap<String, usize> {
    let h: HeldFile = std::fs::read_to_string(dir.join("held.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    h.chats
        .into_iter()
        .map(|(c, q)| {
            let n = q.iter().filter(|m| now - m.at < HELD_DAYS * 86_400).count();
            (c, n)
        })
        .filter(|(_, n)| *n > 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_open_for_a_day_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let w = Windows::open(Some(dir.path().into()));
        assert_eq!(w.state("97250", 1000), Window::Unknown);
        w.opened("97250", 1000);
        assert_eq!(w.state("97250", 1000 + 3600), Window::Open);
        assert_eq!(w.state("97250", 1000 + WINDOW_SECS - 30), Window::Closed);
        let again = Windows::open(Some(dir.path().into()));
        assert_eq!(again.state("97250", 2000), Window::Open);
        again.closed("97250");
        assert_eq!(again.state("97250", 2000), Window::Closed);
    }

    #[test]
    fn held_messages_are_capped_kept_and_said_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let w = Windows::open(Some(dir.path().into()));
        assert!(w.hold("a", "one", &[], 10));
        assert!(!w.hold("a", "two", &[], 11));
        for n in 0..HELD_MAX {
            w.hold("a", &format!("m{n}"), &[], 12);
        }
        assert_eq!(held_on_disk(dir.path(), 13)["a"], HELD_MAX);
        let again = Windows::open(Some(dir.path().into()));
        let (msgs, dropped) = again.take("a", 13);
        assert_eq!((msgs.len(), dropped), (HELD_MAX, 2));
        assert_eq!(msgs[0].text, "m0");
        assert!(again.take("a", 13).0.is_empty());
        // A week later the rest is gone, and counted.
        w.hold("b", "old", &[], 0);
        let (msgs, dropped) = w.take("b", HELD_DAYS * 86_400 + 1);
        assert_eq!((msgs.len(), dropped), (0, 1));
    }
}
