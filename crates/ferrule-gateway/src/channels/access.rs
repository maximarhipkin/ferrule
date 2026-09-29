//! M31: who reaches the agent through Discord and Slack. Both follow
//! Telegram's rule: a DM gets in when its author is allow-listed, and a
//! shared channel when the channel is allow-listed and the bot was
//! addressed (the adapter checks the addressing). A stranger's DM is told
//! its id once while nobody is allowed yet, then silence; an ignored chat is
//! a warning once an hour. `ferrule setup` pairs by a one-time code.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

/// A chat that isn't allowed is logged at most this often.
const IGNORED_WARN_EVERY: Duration = Duration::from_secs(3600);

/// What to do with a DM.
#[derive(Debug, PartialEq, Eq)]
pub enum Dm {
    /// Its author is allowed.
    Admit,
    /// Not allowed; send this reply (the stranger's id, once).
    Tell(String),
    /// Setup's code, from this author: now allowed, answer with this.
    Paired(String),
    /// Not allowed, and nothing to say.
    Drop,
}

pub struct Access {
    /// "discord", "slack": for the log and the replies.
    channel: &'static str,
    /// "Discord user id": for the stranger's reply.
    id_name: &'static str,
    users: RwLock<HashSet<String>>,
    channels: HashSet<String>,
    /// Strangers already told their id.
    told: Mutex<HashSet<String>>,
    /// When each ignored chat was last logged.
    ignored: Mutex<HashMap<String, Instant>>,
    /// `ferrule setup`'s one-time code, and who sent it.
    pairing: Option<String>,
    paired: Mutex<Option<(String, String)>>,
    /// The config keys named in the log and the stranger's reply
    /// (M39's sub-tables: `[gateway.matrix] allowed_users`).
    users_key: String,
    chats_key: String,
    /// Whether a stranger is told their id at all (M39: not on WhatsApp,
    /// where a reply opens a billed conversation, nor by email).
    tell: bool,
}

impl Access {
    pub fn new(
        channel: &'static str,
        id_name: &'static str,
        users: Vec<String>,
        channels: Vec<String>,
    ) -> Self {
        Self {
            channel,
            id_name,
            users: RwLock::new(users.into_iter().map(|u| u.trim().to_string()).collect()),
            channels: channels.into_iter().map(|c| c.trim().to_string()).collect(),
            told: Mutex::new(HashSet::new()),
            ignored: Mutex::new(HashMap::new()),
            pairing: None,
            paired: Mutex::new(None),
            users_key: format!("{channel}_allowed_users"),
            chats_key: format!("{channel}_allowed_channels"),
            tell: true,
        }
    }

    /// M39: the config keys to name, e.g. `[gateway.matrix] allowed_users`
    /// and `[gateway.matrix] allowed_rooms`.
    pub fn with_keys(mut self, users: impl Into<String>, chats: impl Into<String>) -> Self {
        self.users_key = users.into();
        self.chats_key = chats.into();
        self
    }

    /// M39: a stranger is never told anything.
    pub fn silent(mut self) -> Self {
        self.tell = false;
        self
    }

    /// Setup only: the first DM that is exactly `code` pairs its author.
    pub fn with_pairing(mut self, code: impl Into<String>) -> Self {
        self.pairing = Some(code.into());
        self
    }

    /// Who paired, `(id, name)`, if anyone has.
    pub fn paired(&self) -> Option<(String, String)> {
        self.paired.lock().unwrap().clone()
    }

    pub fn user_allowed(&self, user: &str) -> bool {
        self.users.read().unwrap().contains(user)
    }

    /// M39: allows one more user (Mattermost's usernames, resolved to ids
    /// at start).
    pub fn allow(&self, user: &str) {
        self.users.write().unwrap().insert(user.trim().to_string());
    }

    pub fn channel_allowed(&self, channel: &str) -> bool {
        self.channels.contains(channel)
    }

    /// A DM from `user` (`name` for the log) saying `text`.
    pub fn dm(&self, user: &str, name: &str, text: &str) -> Dm {
        if self.user_allowed(user) {
            return Dm::Admit;
        }
        if let Some(code) = &self.pairing {
            if text.trim() == code {
                let mut paired = self.paired.lock().unwrap();
                if paired.is_none() {
                    *paired = Some((user.to_string(), name.to_string()));
                    self.users.write().unwrap().insert(user.to_string());
                    return Dm::Paired(format!(
                        "Paired. You ({name}) can talk to this bot now; ferrule setup saved your {} {user}.",
                        self.id_name
                    ));
                }
            }
        }
        self.ignore(user, name, "DM");
        let empty = self.users.read().unwrap().is_empty();
        if self.tell
            && empty
            && self.pairing.is_none()
            && self.told.lock().unwrap().insert(user.to_string())
        {
            return Dm::Tell(format!(
                "This bot is private. Your {} is {user} — add it to {} in the ferrule config, or run `ferrule setup`.",
                self.id_name, self.users_key
            ));
        }
        Dm::Drop
    }

    /// Logs an ignored chat, once an hour each.
    pub fn ignore(&self, chat: &str, sender: &str, what: &str) {
        let due = {
            let mut ignored = self.ignored.lock().unwrap();
            if ignored.len() > 10_000 {
                ignored.clear();
            }
            let due = ignored
                .get(chat)
                .is_none_or(|at| at.elapsed() >= IGNORED_WARN_EVERY);
            if due {
                ignored.insert(chat.to_string(), Instant::now());
            }
            due
        };
        if due {
            let list = if what == "DM" {
                &self.users_key
            } else {
                &self.chats_key
            };
            tracing::warn!(
                sender = %sender,
                "{}: ignored a {what} from {chat}: it isn't in {list} — add it there if it should reach the agent (logged once an hour per chat)",
                self.channel,
            );
        }
    }
}

/// Removes the leading address to the bot (`<@123>`, `<@!123>`) from
/// `text`, so `@Ferrule /status` is `/status`.
pub fn strip_mention(text: &str, bot: &str) -> String {
    strip_leading(text, &[format!("<@{bot}>"), format!("<@!{bot}>")])
}

/// Removes the first of `forms` that `text` starts with (case-blind), and
/// the `:`/`,` after it (M39: `@ferrule`, a Matrix id or display name).
pub fn strip_leading(text: &str, forms: &[String]) -> String {
    let t = text.trim_start();
    for form in forms.iter().filter(|f| !f.is_empty()) {
        let n = form.len();
        if t.len() >= n && t.is_char_boundary(n) && t[..n].eq_ignore_ascii_case(form) {
            return t[n..]
                .trim_start_matches([' ', ':', ','])
                .trim()
                .to_string();
        }
    }
    text.trim().to_string()
}

/// The kind of a file, in words for the sender, from its MIME type.
pub fn file_kind(mime: Option<&str>) -> &'static str {
    match mime.unwrap_or("").split('/').next().unwrap_or("") {
        "image" => "image",
        "audio" => "audio file",
        "video" => "video",
        _ => "file",
    }
}

/// The reply to a message with nothing to read in it.
pub fn cannot_read_text(kind: &str) -> String {
    format!(
        "I got your {kind}, but I can only read text for now, so I don't know what's in it. Please type your message instead."
    )
}

/// The note appended when a message has text and something unread.
pub fn unread_note(text: &str, kind: &str) -> String {
    format!(
        "{text}\n\n[The {kind} attached to this message wasn't read: ferrule reads only text for now.]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stranger_is_told_once_while_nobody_is_allowed_then_silence() {
        let a = Access::new("discord", "Discord user id", vec![], vec![]);
        let Dm::Tell(t) = a.dm("7", "eve", "hi") else {
            panic!()
        };
        assert!(t.contains("Your Discord user id is 7"), "{t}");
        assert!(t.contains("discord_allowed_users"), "{t}");
        assert_eq!(a.dm("7", "eve", "hi"), Dm::Drop);
        let b = Access::new("discord", "Discord user id", vec!["1".into()], vec![]);
        assert_eq!(b.dm("1", "max", "hi"), Dm::Admit);
        assert_eq!(b.dm("7", "eve", "hi"), Dm::Drop, "a list in use: silence");
    }

    #[test]
    fn the_setup_code_pairs_one_author_and_only_exactly() {
        let a = Access::new("slack", "Slack user id", vec![], vec![]).with_pairing("ferrule-4827");
        assert_eq!(a.dm("U2", "eve", "hi"), Dm::Drop, "no id told during setup");
        assert_eq!(a.dm("U2", "eve", "ferrule-4827 please"), Dm::Drop);
        assert!(matches!(a.dm("U1", "max", " ferrule-4827 "), Dm::Paired(_)));
        assert_eq!(a.paired(), Some(("U1".into(), "max".into())));
        assert_eq!(a.dm("U1", "max", "hello"), Dm::Admit);
        assert_eq!(a.dm("U3", "bob", "ferrule-4827"), Dm::Drop, "used once");
    }

    #[test]
    fn the_leading_mention_is_stripped() {
        assert_eq!(strip_mention("<@42> /status", "42"), "/status");
        assert_eq!(strip_mention("<@!42>: hi there", "42"), "hi there");
        assert_eq!(strip_mention("hi <@42>", "42"), "hi <@42>");
        assert_eq!(strip_mention("<@U1> hi", "U1"), "hi");
    }
}
