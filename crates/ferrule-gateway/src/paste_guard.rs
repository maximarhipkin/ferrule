//! M52.5: a key pasted into a chat stops at the gateway. A message that
//! looks like it holds an API key or a token never reaches a lane, so no
//! transcript keeps it and no model sees it; the chat is told why, and the
//! message is deleted where the channel lets the bot do that. Nothing of it
//! is logged, only that one was stopped (docs/channels.md, "Keys pasted
//! into a chat"). In the owner's private chat the reply comes with a
//! one-time link to the dashboard's Safe instead (M54 §6.3's
//! `secret_request` flow), through a hook the CLI implements.

use crate::health::Redactor;
use std::future::Future;
use std::pin::Pin;

/// Prefixes the credential proxy's list doesn't have: Google's API keys
/// (Gemini's among them), and OpenRouter's, to tell them from any `sk-`.
const MORE_PREFIXES: &[&str] = &["AIza", "sk-or-"];

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
    prefixed(text).next().is_some() || redactor.redact(text) != text
}

fn key_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

/// Each known prefix that starts a key in `text`, and where.
fn prefixed(text: &str) -> impl Iterator<Item = (usize, &'static str)> + '_ {
    let b = text.as_bytes();
    ferrule_proxy::KNOWN_PREFIXES
        .iter()
        .chain(MORE_PREFIXES)
        .copied()
        .flat_map(move |prefix| {
            text.match_indices(prefix)
                .map(|(at, _)| at)
                .filter(move |&at| starts_key(b, at, prefix.len()))
                .map(move |at| (at, prefix))
        })
}

/// Whether the prefix at `at` (`len` bytes) starts a word and is followed
/// by a key's body.
fn starts_key(b: &[u8], at: usize, len: usize) -> bool {
    if at > 0 && key_char(b[at - 1]) {
        return false;
    }
    let body: Vec<u8> = b[at + len..]
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
}

/// What a key's prefix says it is: the name `ferrule setup` gives that
/// kind of key and the hosts it may go to (setup's own presets). Only a
/// guess, filled in on a page where the owner can change both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyGuess {
    pub name: &'static str,
    pub hosts: &'static [&'static str],
}

const GITHUB: KeyGuess = KeyGuess {
    name: "GITHUB_TOKEN",
    hosts: &["api.github.com", "github.com", "*.githubusercontent.com"],
};
const STRIPE: KeyGuess = KeyGuess {
    name: "STRIPE_API_KEY",
    hosts: &["api.stripe.com"],
};

/// The prefixes that name one provider. A bare `sk-` (OpenAI's older
/// keys, DeepSeek's, Moonshot's and others') names none, nor does a key
/// known only by its value or its shape.
const GUESSES: &[(&str, KeyGuess)] = &[
    (
        "sk-ant-",
        KeyGuess {
            name: "ANTHROPIC_API_KEY",
            hosts: &["api.anthropic.com"],
        },
    ),
    (
        "sk-or-",
        KeyGuess {
            name: "OPENROUTER_API_KEY",
            hosts: &["openrouter.ai"],
        },
    ),
    (
        "sk-proj-",
        KeyGuess {
            name: "OPENAI_API_KEY",
            hosts: &["api.openai.com"],
        },
    ),
    (
        "AIza",
        KeyGuess {
            name: "GEMINI_API_KEY",
            hosts: &["generativelanguage.googleapis.com"],
        },
    ),
    ("github_pat_", GITHUB),
    ("ghp_", GITHUB),
    (
        "glpat-",
        KeyGuess {
            name: "GITLAB_TOKEN",
            hosts: &["gitlab.com"],
        },
    ),
    (
        "hf_",
        KeyGuess {
            name: "HF_TOKEN",
            hosts: &["huggingface.co"],
        },
    ),
    (
        "xoxb-",
        KeyGuess {
            name: "SLACK_BOT_TOKEN",
            hosts: &["slack.com"],
        },
    ),
    ("sk_live_", STRIPE),
    ("sk_test_", STRIPE),
    ("rk_live_", STRIPE),
];

/// What the first key in `text` is, by the longest prefix that starts it;
/// `None` when that prefix names no one provider.
pub fn guess(text: &str) -> Option<KeyGuess> {
    let (_, prefix) = prefixed(text).min_by_key(|(at, p)| (*at, std::cmp::Reverse(p.len())))?;
    GUESSES.iter().find(|(p, _)| *p == prefix).map(|(_, g)| *g)
}

/// A one-time link to the dashboard's Safe, offered in place of a pasted
/// key (M54 §6.3's `secret_request` flow).
pub struct LinkOffer {
    /// The reply's words about the link.
    pub said: String,
    /// Sends it; started once the reply has gone, so it arrives second.
    pub send: Pin<Box<dyn Future<Output = ()> + Send>>,
}

/// Implemented by the CLI, which holds the page. `Some` only for the
/// owner's private chat; anyone else gets the plain reply. It is told
/// where the key was pasted and what its prefix says, never the text.
pub trait PastedKeyHook: Send + Sync {
    fn offer(&self, channel: &str, chat_id: &str, guess: Option<KeyGuess>) -> Option<LinkOffer>;
}

/// What the chat is told: never the message, nor which part of it looked
/// like a key. `link`: the hook's words about the link that follows.
pub fn reply(deleted: bool, link: Option<&str>) -> String {
    let gone = if deleted {
        "I deleted your message."
    } else {
        "This app doesn't let me delete it, so please delete your message yourself."
    };
    let (instead, replace) = match link {
        Some(said) => (said, "make a new one and paste that on the page instead"),
        None => (
            "Send /dashboard and add it on the Connect page (a model's key, a channel's token or a key for a tool), or run ferrule setup on the machine.",
            "replace it with a new one",
        ),
    };
    format!(
        "That looked like an API key or a token, so I stopped it here: the agent never saw it and nothing kept it. {gone}\n\n\
         Keys don't go in a chat, where the model and the chat's history would see them. {instead}\n\n\
         It has been through the chat app's servers, so if it's a real key, {replace}."
    )
}
