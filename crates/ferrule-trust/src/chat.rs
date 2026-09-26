//! M31: a chat on some channel. The owner used to be a Telegram chat id
//! (an `i64`); with Discord and Slack it's a channel and an id there. An
//! `i64` still converts, as the Telegram chat it always meant.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChatRef {
    /// The gateway channel's name: `telegram`, `discord`, `slack`.
    pub channel: String,
    /// The chat's id on that channel (for a DM, the user's id).
    pub chat: String,
}

impl ChatRef {
    pub fn new(channel: impl Into<String>, chat: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            chat: chat.into(),
        }
    }

    /// The Telegram chat id, when this is a Telegram chat.
    pub fn telegram_id(&self) -> Option<i64> {
        (self.channel == "telegram")
            .then(|| self.chat.parse().ok())
            .flatten()
    }

    /// The chat for the audit log: a number for Telegram, as before M31.
    pub fn audit_value(&self) -> serde_json::Value {
        match self.telegram_id() {
            Some(id) => id.into(),
            None => self.chat.clone().into(),
        }
    }

    /// "Telegram", "Discord", "Slack": the channel for a sentence.
    pub fn channel_title(&self) -> String {
        let mut c = self.channel.chars();
        match c.next() {
            Some(first) => first.to_uppercase().chain(c).collect(),
            None => String::new(),
        }
    }
}

impl From<i64> for ChatRef {
    fn from(chat: i64) -> Self {
        Self::new("telegram", chat.to_string())
    }
}

impl From<&ChatRef> for ChatRef {
    fn from(chat: &ChatRef) -> Self {
        chat.clone()
    }
}

/// "telegram chat 42": who did something, for `/stop`'s `by`.
impl fmt::Display for ChatRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} chat {}", self.channel, self.chat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_i64_is_a_telegram_chat() {
        let c = ChatRef::from(42);
        assert_eq!(c.to_string(), "telegram chat 42");
        assert_eq!(c.telegram_id(), Some(42));
        assert_eq!(c.audit_value(), serde_json::json!(42));
        let d = ChatRef::new("discord", "1234");
        assert_eq!(d.telegram_id(), None);
        assert_eq!(d.audit_value(), serde_json::json!("1234"));
        assert_eq!(d.channel_title(), "Discord");
    }
}
