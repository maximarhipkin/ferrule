//! M52.5: a key pasted into a chat stops at the gateway. A message that
//! looks like it holds an API key or a token never reaches a lane, so no
//! transcript keeps it and no model sees it; the chat is told why, and the
//! message is deleted where the channel lets the bot do that. Nothing of it
//! is logged, only that one was stopped (docs/channels.md, "Keys pasted
//! into a chat").

use crate::health::Redactor;

/// Prefixes the credential proxy's list doesn't have: Google's API keys
/// (Gemini's among them).
const MORE_PREFIXES: &[&str] = &["AIza"];

/// The fewest key characters after a prefix.
const MIN_BODY: usize = 20;

/// The fewest switches between a letter and a digit among them. A random
/// key switches all the time (about one pair in four); a word, a version
/// or a `snake_case` name almost never does.
const MIN_SWITCHES: usize = 3;

/// Whether `text` holds what looks like an API key or a token:
/// - one of this machine's own secret values (the redactor knows them);
/// - a shape the redactor knows: a Telegram or Discord bot token (and so a
///   JWT), a Slack token, a Meta access token;
/// - a known prefix at the start of a word (the credential proxy's list,
///   `ghp_`, `sk-ant-`, `xoxb-`, …, and Google's `AIza`), then 20 or more
///   key characters that switch between letters and digits at least 3
///   times.
pub fn holds_a_key(text: &str, redactor: &Redactor) -> bool {
    prefixed_key(text) || redactor.redact(text) != text
}

fn key_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

fn prefixed_key(text: &str) -> bool {
    let b = text.as_bytes();
    ferrule_proxy::KNOWN_PREFIXES
        .iter()
        .chain(MORE_PREFIXES)
        .any(|prefix| {
            text.match_indices(prefix).any(|(at, _)| {
                if at > 0 && key_char(b[at - 1]) {
                    return false;
                }
                let body: Vec<u8> = b[at + prefix.len()..]
                    .iter()
                    .copied()
                    .take_while(|c| key_char(*c))
                    .collect();
                let switches = body
                    .windows(2)
                    .filter(|w| {
                        (w[0].is_ascii_digit() && w[1].is_ascii_alphabetic())
                            || (w[0].is_ascii_alphabetic() && w[1].is_ascii_digit())
                    })
                    .count();
                body.len() >= MIN_BODY && switches >= MIN_SWITCHES
            })
        })
}

/// What the chat is told: never the message, nor which part of it looked
/// like a key.
pub fn reply(deleted: bool) -> String {
    let gone = if deleted {
        "I deleted your message."
    } else {
        "This app doesn't let me delete it, so please delete your message yourself."
    };
    format!(
        "That looked like an API key or a token, so I stopped it here: the agent never saw it and nothing kept it. {gone}\n\n\
         Keys don't go in a chat, where the model and the chat's history would see them. Send /dashboard and add it on the Connect page (a model's key, a channel's token or a key for a tool), or run ferrule setup on the machine.\n\n\
         It has been through the chat app's servers, so if it's a real key, replace it with a new one."
    )
}
