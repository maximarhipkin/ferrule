//! M53.5: the owner's 👍/👎 on the bot's Telegram replies, for the learning
//! pass (`docs/m53-contrastive-probing.md`, "M53.5"). The router says where
//! each reply ended in its chat's transcript; the Telegram channel hands
//! over the reactions. Only the owner's 👍 or 👎, only on a reply this
//! gateway sent since it started, reaches `learn/reactions.jsonl`.

use crate::config::{self, Config};
use anyhow::Result;
use ferrule_gateway::{Reaction, ReactionSink, Replied};
use ferrule_learn::thumbs::{self, Mark};
use ferrule_learn::LearnDir;
use ferrule_trust::Hub;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Replies remembered, newest last; a reaction on an older one is left.
const REPLIES_KEPT: usize = 500;

/// One reply the bot sent, and where it ended in its transcript.
struct Sent {
    channel: String,
    chat: String,
    id: String,
    session: String,
    mark: Mark,
}

pub struct Thumbs {
    dir: LearnDir,
    sessions: PathBuf,
    hub: Arc<Hub>,
    replies: Mutex<VecDeque<Sent>>,
}

/// The sink, when `[learning]` is on; learning off (and, M64.5, the
/// proactive agent off) asks Telegram for no reactions at all.
pub fn thumbs(cfg: &Config, hub: Arc<Hub>) -> Result<Option<Arc<Thumbs>>> {
    if !cfg.learning.enabled {
        return Ok(None);
    }
    let data = config::data_dir()?;
    Ok(Some(Arc::new(Thumbs {
        dir: LearnDir::new(&data),
        sessions: data.join("sessions"),
        hub,
        replies: Mutex::new(VecDeque::new()),
    })))
}

impl Thumbs {
    /// For the router: each reply's end in its transcript.
    pub fn replied_hook(self: &Arc<Self>) -> Replied {
        let me = self.clone();
        Arc::new(move |session, channel, chat, id| me.replied(session, channel, chat, id))
    }

    /// For the Telegram channel: its reactions, read off its poll loop.
    pub fn sink(self: &Arc<Self>) -> ReactionSink {
        let me = self.clone();
        Arc::new(move |r| {
            let me = me.clone();
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => {
                    rt.spawn_blocking(move || me.reacted(r));
                }
                Err(_) => me.reacted(r),
            }
        })
    }

    fn transcript(&self, session: &str) -> PathBuf {
        self.sessions.join(format!("{session}.jsonl"))
    }

    fn replied(&self, session: &str, channel: &str, chat: &str, id: &str) {
        // Only Telegram's reactions are read.
        if channel != "telegram" {
            return;
        }
        let Some(mark) = Mark::of(&self.transcript(session)) else {
            return;
        };
        let mut replies = self.replies.lock().unwrap_or_else(|e| e.into_inner());
        if replies.len() >= REPLIES_KEPT {
            replies.pop_front();
        }
        replies.push_back(Sent {
            channel: channel.into(),
            chat: chat.into(),
            id: id.into(),
            session: session.into(),
            mark,
        });
    }

    fn reacted(&self, r: Reaction) {
        let set: Vec<&str> = r
            .emoji
            .iter()
            .map(String::as_str)
            .filter(|e| matches!(*e, thumbs::UP | thumbs::DOWN))
            .collect();
        // Neither, or both at once: no verdict.
        let [thumb] = set.as_slice() else {
            return;
        };
        if self.hub.owner_on(&r.channel).map(|o| o.chat).as_ref() != Some(&r.user_id) {
            return;
        }
        let found = self
            .replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find(|s| s.channel == r.channel && s.chat == r.chat_id && s.id == r.message_id)
            .map(|s| (s.session.clone(), s.mark.clone()));
        let Some((session, mark)) = found else {
            tracing::info!(
                chat = %r.chat_id,
                message = %r.message_id,
                "learning: the owner's {thumb} isn't on a reply this gateway sent since it started; not kept"
            );
            return;
        };
        let at = if r.at > 0 {
            r.at
        } else {
            chrono::Utc::now().timestamp()
        };
        let kept = if *thumb == thumbs::UP {
            thumbs::up(&self.dir, &session, &r.message_id, at)
        } else {
            thumbs::down(
                &self.dir,
                &session,
                &r.message_id,
                &self.transcript(&session),
                &mark,
                at,
            )
        };
        match kept {
            Ok(()) => {
                tracing::info!(session = %session, "learning: kept the owner's {thumb} for the next pass")
            }
            Err(e) => {
                tracing::warn!(session = %session, "learning: the owner's {thumb} wasn't kept: {e}")
            }
        }
    }
}
