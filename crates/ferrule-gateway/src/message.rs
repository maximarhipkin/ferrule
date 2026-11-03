use serde::{Deserialize, Serialize};

/// A file/image/audio blob riding alongside a message. Adapters decide
/// whether `url` is a remote URL, a local path, or an opaque provider id —
/// the gateway itself never fetches or interprets it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// A message normalized from any channel into one shape. `channel` + `chat_id`
/// together select the session lane (see `session::session_id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub channel: String,
    pub chat_id: String,
    pub sender: String,
    /// The sender's id on the channel (Telegram's `from.id`), when it says:
    /// an owner-only command in a group checks it (M21).
    #[serde(default)]
    pub sender_id: Option<String>,
    pub message_id: String,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Unix seconds, as reported by the channel (or capture time if unknown).
    pub ts: i64,
}

/// A reply to deliver back out through a channel adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub channel: String,
    pub chat_id: String,
    pub text: String,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// M53.5: someone's reaction on a message in a chat (Telegram's
/// `message_reaction`), what it is now. Not a message: it never reaches a
/// session, only a [`ReactionSink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    pub channel: String,
    pub chat_id: String,
    /// The message reacted to.
    pub message_id: String,
    /// Who reacted: the channel's id for them.
    pub user_id: String,
    /// The emoji now set on it, in order; empty when they took theirs back.
    pub emoji: Vec<String>,
    /// Unix seconds.
    pub at: i64,
}

/// Where a channel hands the reactions it hears.
pub type ReactionSink = std::sync::Arc<dyn Fn(Reaction) + Send + Sync>;

/// What the model answers when a message needs nothing from it: the
/// gateway posts nothing.
pub const NO_REPLY: &str = "NO_REPLY";

/// Whether `chat_id` on `channel` is a group, where several people write
/// and a turn may end without a word: a Telegram group (a negative id), a
/// Signal group (a base64 id) or a WhatsApp group (`…@g.us`).
pub fn is_group_chat(channel: &str, chat_id: &str) -> bool {
    match channel {
        "telegram" => chat_id.parse::<i64>().is_ok_and(|id| id < 0),
        "signal" => !chat_id.is_empty() && crate::channels::signal::is_group(chat_id),
        crate::channels::waha::NAME => crate::channels::waha::is_group(chat_id),
        _ => false,
    }
}

/// Whether `answer` is the [`NO_REPLY`] sentinel: any case, around it only
/// whitespace, backticks or brackets.
pub fn is_no_reply(answer: &str) -> bool {
    answer
        .trim()
        .trim_matches(|c: char| c.is_whitespace() || "`[]()<>{}".contains(c))
        .eq_ignore_ascii_case(NO_REPLY)
}

/// Whether a turn that ended with `answer` posts nothing: [`NO_REPLY`]
/// anywhere, and in a group an empty answer too.
pub fn is_silent(answer: &str, group: bool) -> bool {
    is_no_reply(answer) || (group && answer.trim().is_empty())
}

/// While a streamed answer could still become [`NO_REPLY`], nothing of it
/// is shown.
pub(crate) fn may_be_no_reply(so_far: &str) -> bool {
    let t = so_far.trim_start_matches(|c: char| c.is_whitespace() || "`[(<{".contains(c));
    let t = t.trim_end_matches(|c: char| c.is_whitespace() || "`])>}".contains(c));
    t.len() <= NO_REPLY.len()
        && NO_REPLY
            .get(..t.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sentinel_in_any_dress() {
        for s in [
            "NO_REPLY",
            " no_reply \n",
            "`NO_REPLY`",
            "[NO_REPLY]",
            "```\nNO_REPLY\n```",
        ] {
            assert!(is_no_reply(s), "{s:?}");
        }
        for s in ["", "NO_REPLY please", "No reply needed", "NO_REPLY.", "OK"] {
            assert!(!is_no_reply(s), "{s:?}");
        }
    }

    #[test]
    fn groups_by_channel() {
        assert!(is_group_chat("telegram", "-1001234"));
        assert!(!is_group_chat("telegram", "950867454"));
        assert!(is_group_chat("whatsapp_personal", "1203630@g.us"));
        assert!(!is_group_chat("whatsapp_personal", "972501234567@c.us"));
        assert!(!is_group_chat("signal", "+972501234567"));
        assert!(is_group_chat(
            "signal",
            "kKPJ+Qm3yK8Qn0bM1F2dA2l8Q3mK0yQ4pD5e6f7g8h0="
        ));
        assert!(!is_group_chat("dashboard", "owner"));
    }

    #[test]
    fn silence_is_no_reply_anywhere_and_empty_in_a_group() {
        assert!(is_silent("NO_REPLY", false));
        assert!(is_silent("  ", true));
        assert!(!is_silent("  ", false));
        assert!(!is_silent("hello", true));
    }

    #[test]
    fn a_stream_waits_while_it_could_be_the_sentinel() {
        for s in ["", "N", "`no_", "NO_REPLY", "[NO_REPLY]\n"] {
            assert!(may_be_no_reply(s), "{s:?}");
        }
        for s in ["Hi", "NO_REPLY and", "Nobody"] {
            assert!(!may_be_no_reply(s), "{s:?}");
        }
    }
}
