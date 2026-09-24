//! Pending approvals, and reading the owner's replies to them.
//!
//! Each question gets a short code. Only "yes" runs anything: a bare `yes`
//! when exactly one question is pending in the chat, or `yes <code>`.
//! `no <code>` refuses that one, and any other text refuses every pending
//! question in the chat, so nothing waits on a reply that meant something
//! else. A message that could mean either of two questions approves neither.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// How long an expired code is remembered, to answer a late reply.
const REMEMBER: Duration = Duration::from_secs(24 * 3600);
const LETTERS: &[u8] = b"abcdefghijkmnpqrstuvwxyz";
const DIGITS: &[u8] = b"23456789";

/// The owner's answer to one question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Yes,
    /// Refused, with what the owner said.
    No(String),
}

struct Pending {
    code: String,
    chat: i64,
    what: String,
    tx: oneshot::Sender<Answer>,
}

#[derive(Default)]
struct Inner {
    pending: Vec<Pending>,
    expired: VecDeque<(String, i64, Instant)>,
    counter: u64,
}

#[derive(Default)]
pub struct Approvals {
    inner: Mutex<Inner>,
}

enum Said {
    Yes(Option<String>),
    No(Option<String>),
    Other,
}

fn parse(text: &str) -> Said {
    let t = text
        .trim()
        .trim_end_matches(['.', '!'])
        .trim()
        .to_lowercase();
    let mut words = t.split_whitespace();
    let first = words.next().unwrap_or("");
    let code = words.next().map(str::to_string);
    if words.next().is_some() {
        return Said::Other;
    }
    match first {
        "yes" => Said::Yes(code),
        "no" if code.is_some() => Said::No(code),
        _ => Said::Other,
    }
}

impl Approvals {
    /// A new question for `chat`: its code, and where the answer arrives.
    pub fn open(&self, chat: i64, what: &str) -> (String, oneshot::Receiver<Answer>) {
        let mut inner = self.inner.lock().unwrap();
        let (tx, rx) = oneshot::channel();
        let code = loop {
            inner.counter += 1;
            let seed = uuid::Uuid::new_v4().as_u128() as usize + inner.counter as usize;
            let code = format!(
                "{}{}",
                LETTERS[seed % LETTERS.len()] as char,
                DIGITS[(seed / LETTERS.len()) % DIGITS.len()] as char
            );
            let taken = inner.pending.iter().any(|p| p.code == code)
                || inner.expired.iter().any(|(c, _, _)| *c == code);
            if !taken {
                break code;
            }
        };
        inner.pending.push(Pending {
            code: code.clone(),
            chat,
            what: what.to_string(),
            tx,
        });
        (code, rx)
    }

    /// Takes a question back (it timed out, or the run stopped waiting): a
    /// reply to it later is told it expired.
    pub fn withdraw(&self, code: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(i) = inner.pending.iter().position(|p| p.code == code) {
            let p = inner.pending.remove(i);
            inner.expired.push_back((p.code, p.chat, Instant::now()));
        }
        while inner
            .expired
            .front()
            .is_some_and(|(_, _, at)| at.elapsed() > REMEMBER)
        {
            inner.expired.pop_front();
        }
    }

    pub fn pending_in(&self, chat: i64) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.pending.iter().filter(|p| p.chat == chat).count()
    }

    /// Reads a message from `chat` as an answer. `Some(reply)` means it was
    /// one (the reply goes back to the chat, the message goes no further);
    /// `None` means it's an ordinary message.
    pub fn answer(&self, chat: i64, text: &str) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        let said = parse(text);
        let here: Vec<usize> = (0..inner.pending.len())
            .filter(|&i| inner.pending[i].chat == chat)
            .collect();
        if here.is_empty() {
            let code = match &said {
                Said::Yes(c) | Said::No(c) => c.clone(),
                Said::Other => return None,
            };
            let late = match code {
                Some(c) => inner
                    .expired
                    .iter()
                    .any(|(e, ch, _)| *e == c && *ch == chat),
                None => inner.expired.iter().any(|(_, ch, _)| *ch == chat),
            };
            return late.then(|| {
                "That question expired and was refused; nothing was run. Ask the agent again if it's still needed.".into()
            });
        }
        let find = |inner: &Inner, code: &str| {
            here.iter()
                .copied()
                .find(|&i| inner.pending[i].code == code)
        };
        match said {
            Said::Yes(None) if here.len() == 1 => {
                let p = inner.pending.remove(here[0]);
                let _ = p.tx.send(Answer::Yes);
                Some(format!("Approved ({}): {}", p.code, p.what))
            }
            Said::Yes(None) => {
                let codes: Vec<String> = here
                    .iter()
                    .map(|&i| {
                        format!(
                            "`yes {}` for {}",
                            inner.pending[i].code, inner.pending[i].what
                        )
                    })
                    .collect();
                Some(format!(
                    "{} questions are waiting, so a bare yes approves neither. Reply {}.",
                    here.len(),
                    codes.join(", or ")
                ))
            }
            Said::Yes(Some(code)) => match find(&inner, &code) {
                Some(i) => {
                    let p = inner.pending.remove(i);
                    let _ = p.tx.send(Answer::Yes);
                    Some(format!("Approved ({}): {}", p.code, p.what))
                }
                None => Some(format!(
                    "No question with code {code} is waiting; nothing was approved."
                )),
            },
            Said::No(Some(code)) => match find(&inner, &code) {
                Some(i) => {
                    let p = inner.pending.remove(i);
                    let _ = p.tx.send(Answer::No(text.trim().to_string()));
                    Some(format!("Refused ({}): {}", p.code, p.what))
                }
                None => Some(format!("No question with code {code} is waiting.")),
            },
            Said::No(None) | Said::Other => {
                let mut refused = Vec::new();
                for &i in here.iter().rev() {
                    let p = inner.pending.remove(i);
                    refused.push(format!("{} ({})", p.what, p.code));
                    let _ = p.tx.send(Answer::No(text.trim().to_string()));
                }
                refused.reverse();
                Some(format!(
                    "Refused: {}. Only `yes` approves; your message wasn't passed on to the agent.",
                    refused.join("; ")
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_yes_approves_the_only_question() {
        let a = Approvals::default();
        let (code, mut rx) = a.open(1, "rm -rf x");
        assert_eq!(code.len(), 2);
        assert!(
            a.answer(2, "yes").is_none(),
            "another chat has nothing pending"
        );
        assert!(a.answer(1, "  YES! ").unwrap().starts_with("Approved"));
        assert_eq!(rx.try_recv().unwrap(), Answer::Yes);
        assert!(a.answer(1, "hello").is_none(), "nothing pending: passes on");
    }

    #[test]
    fn two_pending_need_a_code_and_other_text_refuses_all() {
        let a = Approvals::default();
        let (c1, mut r1) = a.open(1, "one");
        let (c2, mut r2) = a.open(1, "two");
        assert_ne!(c1, c2);
        let reply = a.answer(1, "yes").unwrap();
        assert!(reply.contains(&c1) && reply.contains(&c2), "{reply}");
        assert!(r1.try_recv().is_err() && r2.try_recv().is_err());
        assert_eq!(a.pending_in(1), 2);

        a.answer(1, &format!("yes {c2}")).unwrap();
        assert_eq!(r2.try_recv().unwrap(), Answer::Yes);
        let (_c3, mut r3) = a.open(1, "three");
        a.answer(1, "wait, what is this?").unwrap();
        assert!(matches!(r1.try_recv().unwrap(), Answer::No(_)));
        assert!(matches!(r3.try_recv().unwrap(), Answer::No(_)));
        assert_eq!(a.pending_in(1), 0);
    }

    #[test]
    fn no_with_a_code_refuses_only_that_one() {
        let a = Approvals::default();
        let (c1, mut r1) = a.open(1, "one");
        let (_c2, mut r2) = a.open(1, "two");
        a.answer(1, &format!("no {c1}")).unwrap();
        assert!(matches!(r1.try_recv().unwrap(), Answer::No(_)));
        assert!(r2.try_recv().is_err());
        assert!(a
            .answer(1, "yes zz")
            .unwrap()
            .contains("nothing was approved"));
        assert_eq!(a.pending_in(1), 1);
    }

    #[test]
    fn a_late_reply_is_told_it_expired() {
        let a = Approvals::default();
        let (code, _rx) = a.open(1, "one");
        a.withdraw(&code);
        assert!(a.answer(1, "yes").unwrap().contains("expired"));
        assert!(a
            .answer(1, &format!("yes {code}"))
            .unwrap()
            .contains("expired"));
        assert!(a.answer(1, "good morning").is_none());
    }
}
