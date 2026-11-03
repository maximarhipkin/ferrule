//! M53.5: the owner's 👍/👎 on the bot's chat replies, as the check a chat
//! episode lacks (`docs/m53-contrastive-probing.md`, "M53.5"). A 👎 keeps
//! the chat's transcript cut where the reacted reply ended, in
//! `learn/reactions/`, and becomes an episode for the next pass; a later
//! 👍 on a reply in the same chat marks it fixed. Each reaction is one line
//! of `learn/reactions.jsonl`. Who reacted and on which reply is the
//! gateway's to decide: only the owner's count, only on its own replies.
//! The learning page: a 👍/👎 on a dashboard chat reply is a line of
//! `learn/signals.jsonl` ([`crate::signals`]); both files are read here.

use crate::episode::{content, messages, meta, role, Episode, SCOPE_META};
use crate::files::{write_atomic, LearnDir};
use crate::signals::{self, Signal};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const UP: &str = "👍";
pub const DOWN: &str = "👎";
/// A 👎 episode's outcome.
pub const OUTCOME: &str = "thumbs-down";
/// Bytes of a transcript kept to recognise it again at the reaction.
const MARK_BYTES: u64 = 256;

/// One line of `learn/reactions.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thumb {
    /// Unix seconds.
    pub at: i64,
    /// The chat's session (`<channel>__<chat>`).
    pub session: String,
    /// The reply reacted to, by the channel's id.
    pub message: String,
    pub up: bool,
    /// A 👎's transcript cut, under `learn/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    /// The owner's words on why (a dashboard 👎 may carry them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// Where a reply ended in its chat's transcript: the length when it was
/// sent, and the bytes just before, so a transcript that `/new` replaced
/// since isn't taken for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub len: u64,
    tail: Vec<u8>,
}

impl Mark {
    /// `transcript` as it stands now; `None` when it can't be read.
    pub fn of(transcript: &Path) -> Option<Mark> {
        let mut f = fs::File::open(transcript).ok()?;
        let len = f.metadata().ok()?.len();
        let from = len.saturating_sub(MARK_BYTES);
        f.seek(SeekFrom::Start(from)).ok()?;
        let mut tail = Vec::new();
        f.take(len - from).read_to_end(&mut tail).ok()?;
        Some(Mark { len, tail })
    }

    /// `bytes` up to the mark, when they are still the transcript marked.
    fn cut<'a>(&self, bytes: &'a [u8]) -> Option<&'a [u8]> {
        let len = usize::try_from(self.len).ok()?;
        let head = bytes.get(..len)?;
        head.ends_with(&self.tail).then_some(head)
    }
}

impl LearnDir {
    pub fn reactions_path(&self) -> PathBuf {
        self.root().join("reactions.jsonl")
    }

    /// Every reaction kept, oldest first; unreadable lines skipped:
    /// Telegram's (`reactions.jsonl`) and the dashboard's thumbs among the
    /// signals (`signals.jsonl`).
    pub fn thumbs(&self) -> Vec<Thumb> {
        let mut all: Vec<Thumb> = fs::read_to_string(self.reactions_path())
            .map(|t| {
                t.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            })
            .unwrap_or_default();
        all.extend(
            self.signals()
                .into_iter()
                .filter(Signal::is_thumb)
                .map(|s| Thumb {
                    at: s.at,
                    up: s.kind == signals::UP,
                    session: s.session.unwrap_or_default(),
                    message: s.message.unwrap_or_default(),
                    transcript: s.transcript,
                    why: s.why,
                }),
        );
        // Stable: lines of one file keep their order.
        all.sort_by_key(|t| t.at);
        all
    }

    fn add_thumb(&self, t: &Thumb) -> Result<(), String> {
        fs::create_dir_all(self.root()).map_err(|e| e.to_string())?;
        let line = serde_json::to_string(t).map_err(|e| e.to_string())?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.reactions_path())
            .and_then(|mut f| writeln!(f, "{line}"))
            .map_err(|e| format!("couldn't write {}: {e}", self.reactions_path().display()))
    }
}

/// Records the owner's 👍 on `message`, a reply in `session`.
pub fn up(dir: &LearnDir, session: &str, message: &str, at: i64) -> Result<(), String> {
    dir.add_thumb(&Thumb {
        at,
        session: session.into(),
        message: message.into(),
        up: true,
        transcript: None,
        why: None,
    })
}

/// Records the owner's 👎 on `message`, a reply in `session` that ended at
/// `mark` in `transcript`: the transcript up to there is kept for the pass.
pub fn down(
    dir: &LearnDir,
    session: &str,
    message: &str,
    transcript: &Path,
    mark: &Mark,
    at: i64,
) -> Result<(), String> {
    let rel = keep_cut(dir, session, message, transcript, mark, at)?;
    dir.add_thumb(&Thumb {
        at,
        session: session.into(),
        message: message.into(),
        up: false,
        transcript: Some(rel),
        why: None,
    })
}

/// The transcript up to `mark`, kept under `learn/reactions/` for the
/// pass; its path under `learn/`.
fn keep_cut(
    dir: &LearnDir,
    session: &str,
    message: &str,
    transcript: &Path,
    mark: &Mark,
    at: i64,
) -> Result<String, String> {
    let bytes =
        fs::read(transcript).map_err(|e| format!("the chat's transcript can't be read: {e}"))?;
    let cut = mark
        .cut(&bytes)
        .ok_or("the chat's transcript was replaced since that reply (/new)")?;
    let name = format!("{at}-{}-{}.jsonl", clean(session), clean(message));
    let rel = format!("reactions/{name}");
    write_atomic(&dir.root().join(&rel), cut).map_err(|e| format!("{e:#}"))?;
    Ok(rel)
}

/// The learning page: the owner's 👍 or 👎 on `message`, a reply in
/// `session`, given on `source` (`dashboard`), as a signal. A 👎 with the
/// reply's `place` (its transcript and where the reply ended there) keeps
/// the cut, so it becomes an episode; without one, or when the cut can't
/// be taken, it is kept as a signal only. `Ok(true)` when it can be
/// learned from.
#[allow(clippy::too_many_arguments)]
pub fn signal(
    dir: &LearnDir,
    source: &str,
    session: &str,
    message: &str,
    up: bool,
    why: Option<&str>,
    place: Option<(&Path, &Mark)>,
    at: i64,
) -> Result<bool, String> {
    let transcript = match (up, place) {
        (false, Some((path, mark))) => keep_cut(dir, session, message, path, mark, at).ok(),
        _ => None,
    };
    let learnable = transcript.is_some();
    let mut s = Signal::new(if up { signals::UP } else { signals::DOWN }, source, at);
    s.session = Some(session.into());
    s.message = Some(message.into());
    // The process's known secrets never reach the file (M54.3's scrubber;
    // the caller refuses a "why" that looks like a key).
    s.why = signals::clip_why(why.map(|w| ferrule_core::scrub::text(w.into())).as_deref());
    s.transcript = transcript;
    dir.add_signal(&s)?;
    Ok(learnable)
}

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The owner's 👎s after `cursor` as episodes, oldest first: the goal is
/// the request the reacted reply answered, `fixed` when a 👍 on a reply in
/// the same chat came later.
pub fn episodes(dir: &LearnDir, cursor: i64) -> Vec<Episode> {
    let all = dir.thumbs();
    let mut out = Vec::new();
    for (i, t) in all.iter().enumerate() {
        if t.up || t.at <= cursor {
            continue;
        }
        let Some(path) = t.transcript.as_deref().map(|r| dir.root().join(r)) else {
            continue;
        };
        let msgs = messages(&path);
        let Some(goal) = msgs
            .iter()
            .rev()
            .find(|m| role(m) == "user" && !content(m).starts_with("[ferrule]"))
            .map(|m| content(m).to_string())
        else {
            continue;
        };
        out.push(Episode {
            key: format!("reaction:{}:{}", t.session, t.message),
            label: t.session.clone(),
            goal,
            outcome: OUTCOME.into(),
            detail: match &t.why {
                // Scrubbed again on its way to the reflector: a line
                // written before a secret was known.
                Some(why) => format!(
                    "the owner reacted {DOWN} to the bot's reply that ends this transcript: that reply fell short. The owner said why: {}",
                    ferrule_core::scrub::text(why.clone())
                ),
                None => format!(
                    "the owner reacted {DOWN} to the bot's reply that ends this transcript: that reply fell short"
                ),
            },
            fixed: all[i + 1..]
                .iter()
                .any(|later| later.up && later.session == t.session),
            scope: meta(&path, SCOPE_META),
            transcript: Some(path),
            at: t.at,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_secret_in_the_why_is_scrubbed_before_it_is_kept_or_quoted() {
        const SECRET: &str = "s3cr3t-why-value-0042";
        ferrule_core::scrub::install(std::sync::Arc::new(|t: &str| {
            t.replace(SECRET, "[redacted]")
        }));
        let d = tempfile::tempdir().unwrap();
        let dir = LearnDir::new(d.path());
        let transcript = d.path().join("dashboard__owner.jsonl");
        fs::write(
            &transcript,
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"plan my day\"}}\n{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}\n",
        )
        .unwrap();
        let mark = Mark::of(&transcript).unwrap();
        let why = format!("it ignored {SECRET} again");
        let learnable = signal(
            &dir,
            "dashboard",
            "dashboard__owner",
            "7",
            false,
            Some(&why),
            Some((&transcript, &mark)),
            100,
        )
        .unwrap();
        assert!(learnable);
        let kept = fs::read_to_string(dir.signals_path()).unwrap();
        assert!(
            !kept.contains(SECRET) && kept.contains("[redacted]"),
            "{kept}"
        );
        // A line from before the secret was known is scrubbed when quoted.
        let mut old = Signal::new(signals::DOWN, "dashboard", 200);
        old.session = Some("dashboard__owner".into());
        old.message = Some("8".into());
        old.why = Some(format!("leaked {SECRET}"));
        old.transcript = dir.signals()[0].transcript.clone();
        dir.add_signal(&old).unwrap();
        let eps = episodes(&dir, 0);
        assert_eq!(eps.len(), 2, "{eps:?}");
        assert!(eps.iter().all(|e| !e.detail.contains(SECRET)), "{eps:?}");
        assert!(eps[1].detail.contains("leaked [redacted]"), "{eps:?}");
    }
}
