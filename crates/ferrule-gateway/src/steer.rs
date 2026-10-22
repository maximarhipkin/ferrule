//! M56.1: a message a person sends while their chat's turn runs reaches
//! that turn, after its current tool calls and before its next model call,
//! instead of waiting for the turn to end. [`WhileBusy::Queue`] keeps the
//! old way: it waits, and joins the next turn (PR #46's merging).

use crate::message::{Attachment, InboundMessage};
use ferrule_core::{ImageRef, Inbox, Message};
use serde::Deserialize;
use std::path::Path;
use std::sync::Mutex;

/// What a person's message does while their chat is busy (`[gateway]
/// while_busy`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WhileBusy {
    /// It reaches the running turn at its next step.
    #[default]
    Steer,
    /// It waits for the turn to end and joins the next one.
    Queue,
}

/// How a steered message starts, so the model reads it as the person
/// speaking now, not as a report it was handed.
pub const SENT_WHILE_WORKING: &str = "[sent while you were working]";

/// One lane's way into its running turn. The lane opens it for a turn that
/// answers a person and closes it when the turn ends; while it's open the
/// agent drains it before every model call ([`Inbox::take_messages`]).
#[derive(Default)]
pub(crate) struct Steering {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Open for messages from this sender (`sender_id`); `None` while the
    /// lane runs no person's turn.
    open: Option<Option<String>>,
    /// Taken, not yet read by the turn.
    waiting: Vec<InboundMessage>,
    /// The ids of the messages the turn read.
    read: Vec<String>,
}

/// What a closed turn left: the ids it read (answered with its reply) and
/// what came too late for it (the next turn's).
pub(crate) struct Closed {
    pub read: Vec<String>,
    pub late: Vec<InboundMessage>,
}

impl Steering {
    /// Takes messages from `sender` until [`Steering::close`].
    pub(crate) fn open(&self, sender: Option<String>) {
        let mut st = self.state.lock().unwrap();
        st.open = Some(sender);
        st.read.clear();
    }

    /// Hands `msg` to the running turn: false when no person's turn runs,
    /// or it's someone else's (a group chat).
    pub(crate) fn offer(&self, msg: &InboundMessage) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.open.as_ref() != Some(&msg.sender_id) {
            return false;
        }
        st.waiting.push(msg.clone());
        true
    }

    /// Ends taking; under the same lock as [`Steering::offer`], so a
    /// message is either read by the turn, late, or never taken.
    pub(crate) fn close(&self) -> Closed {
        let mut st = self.state.lock().unwrap();
        st.open = None;
        Closed {
            read: std::mem::take(&mut st.read),
            late: std::mem::take(&mut st.waiting),
        }
    }

    /// Closes and forgets what waits (`/new`, the bot stopping): how many.
    pub(crate) fn drop_waiting(&self) -> usize {
        self.close().late.len()
    }
}

impl Inbox for Steering {
    fn take(&self) -> Vec<String> {
        Vec::new()
    }

    fn take_messages(&self) -> Vec<Message> {
        let mut st = self.state.lock().unwrap();
        let taken = std::mem::take(&mut st.waiting);
        st.read.extend(taken.iter().map(|m| m.message_id.clone()));
        taken.into_iter().map(framed).collect()
    }
}

/// `msg` as the person's own words, with its photos (the file lines are
/// already in its text).
fn framed(msg: InboundMessage) -> Message {
    let images = msg
        .attachments
        .iter()
        .filter(|a| is_image(a))
        .map(|a| ImageRef {
            path: a.url.clone(),
            mime: a.kind.clone(),
            name: a.name.clone(),
            ..Default::default()
        })
        .collect();
    let text = if msg.text.trim().is_empty() {
        SENT_WHILE_WORKING.to_string()
    } else {
        format!("{SENT_WHILE_WORKING} {}", msg.text)
    };
    Message::user_with_images(text, images)
}

/// A photo an adapter saved (M39's inbox), not a link (M47).
pub(crate) fn is_image(a: &Attachment) -> bool {
    a.kind.starts_with("image/") && Path::new(&a.url).is_file()
}

/// An audio file an adapter saved (M39's inbox), not a link.
pub(crate) fn is_audio(a: &Attachment) -> bool {
    a.kind.starts_with("audio/") && Path::new(&a.url).is_file()
}
