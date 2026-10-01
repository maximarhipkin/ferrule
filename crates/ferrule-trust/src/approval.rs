//! Pending approvals, and reading the owner's replies to them.
//!
//! Each question gets a short code, used once. Only "yes" runs anything: a
//! bare `yes` when exactly one question is pending in the chat, or `yes
//! <code>`. `no <code>` refuses that one, and any other text refuses every
//! pending question in the chat, so nothing waits on a reply that meant
//! something else. A message that could mean either of two questions
//! approves neither. A slash command is never an answer (it passes through
//! and refuses nothing), and only the owner's `yes`/`no` counts (M48).

use crate::chat::ChatRef;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// How long an expired code is remembered, to answer a late reply.
const REMEMBER: Duration = Duration::from_secs(24 * 3600);
/// How long an answered code is remembered, so it isn't drawn again and a
/// replayed button runs nothing.
const USED_FOR: Duration = Duration::from_secs(3600);
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
    chat: ChatRef,
    what: String,
    asked: Instant,
    /// How long it may wait, when the asker said (M48).
    timeout: Option<Duration>,
    /// What an admin question is bound to: `{op}:{digest}` (M48).
    op: Option<String>,
    tx: oneshot::Sender<Answer>,
}

/// A question still waiting, as the dashboard lists it (M37).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub code: String,
    /// The chat it was asked in.
    pub chat: ChatRef,
    pub what: String,
    /// Seconds since it was asked.
    pub secs: u64,
    /// Seconds before it expires, for a question that has a timeout.
    pub left_secs: Option<u64>,
    /// The admin op it is bound to (`model_default:3fa9c1…`), if any.
    pub op: Option<String>,
}

#[derive(Default)]
struct Inner {
    pending: Vec<Pending>,
    expired: VecDeque<(String, ChatRef, Instant)>,
    /// Codes already answered, kept for `USED_FOR`.
    used: VecDeque<(String, ChatRef, Instant)>,
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

/// Whether `s` has the shape of a code: a letter and a digit, or those and
/// a letter more.
pub fn is_code(s: &str) -> bool {
    let b = s.as_bytes();
    let letter = |c: u8| LETTERS.contains(&c);
    let digit = |c: u8| DIGITS.contains(&c);
    match b.len() {
        2 => letter(b[0]) && digit(b[1]),
        3 => letter(b[0]) && digit(b[1]) && letter(b[2]),
        _ => false,
    }
}

/// Takes question `i` off the list, remembering its code as answered.
fn take(inner: &mut Inner, i: usize) -> Pending {
    let p = inner.pending.remove(i);
    inner
        .used
        .push_back((p.code.clone(), p.chat.clone(), Instant::now()));
    p
}

impl Approvals {
    /// A new question for `chat`: its code, and where the answer arrives.
    pub fn open(
        &self,
        chat: impl Into<ChatRef>,
        what: &str,
    ) -> (String, oneshot::Receiver<Answer>) {
        self.open_for(chat, what, None, None)
    }

    /// `open`, saying how long the question may wait and which admin op it
    /// is bound to (M48): the dashboard's inbox shows both.
    pub fn open_for(
        &self,
        chat: impl Into<ChatRef>,
        what: &str,
        timeout: Option<Duration>,
        op: Option<String>,
    ) -> (String, oneshot::Receiver<Answer>) {
        let chat = chat.into();
        let mut inner = self.inner.lock().unwrap();
        let (tx, rx) = oneshot::channel();
        while inner
            .used
            .front()
            .is_some_and(|(_, _, at)| at.elapsed() > USED_FOR)
        {
            inner.used.pop_front();
        }
        let mut tries = 0u32;
        let code = loop {
            inner.counter += 1;
            tries += 1;
            let seed = uuid::Uuid::new_v4().as_u128() as usize + inner.counter as usize;
            let mut code = format!(
                "{}{}",
                LETTERS[seed % LETTERS.len()] as char,
                DIGITS[(seed / LETTERS.len()) % DIGITS.len()] as char
            );
            // The 192 short codes can all be taken; then a letter more.
            if tries > 1000 {
                code.push(LETTERS[(seed / (LETTERS.len() * DIGITS.len())) % LETTERS.len()] as char);
            }
            let taken = inner.pending.iter().any(|p| p.code == code)
                || inner.expired.iter().any(|(c, _, _)| *c == code)
                || inner.used.iter().any(|(c, _, _)| *c == code);
            if !taken {
                break code;
            }
        };
        inner.pending.push(Pending {
            code: code.clone(),
            chat,
            what: what.to_string(),
            asked: Instant::now(),
            timeout,
            op,
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

    /// Every question still waiting, from any chat, oldest first.
    pub fn list(&self) -> Vec<Waiting> {
        let inner = self.inner.lock().unwrap();
        inner
            .pending
            .iter()
            .map(|p| Waiting {
                code: p.code.clone(),
                chat: p.chat.clone(),
                what: p.what.clone(),
                secs: p.asked.elapsed().as_secs(),
                left_secs: p
                    .timeout
                    .map(|t| t.saturating_sub(p.asked.elapsed()).as_secs()),
                op: p.op.clone(),
            })
            .collect()
    }

    /// Answers the question `code` whichever chat it was asked in, as its
    /// Allow/Refuse button would (M37: the dashboard). `None`: no such
    /// question is waiting.
    pub fn decide(&self, code: &str, allow: bool, said: &str) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        let i = inner.pending.iter().position(|p| p.code == code)?;
        let p = take(&mut inner, i);
        if allow {
            let _ = p.tx.send(Answer::Yes);
            Some(format!("Approved ({}): {}", p.code, p.what))
        } else {
            let _ = p.tx.send(Answer::No(said.to_string()));
            Some(format!("Refused ({}): {}", p.code, p.what))
        }
    }

    pub fn pending_in(&self, chat: impl Into<ChatRef>) -> usize {
        let chat = chat.into();
        let inner = self.inner.lock().unwrap();
        inner.pending.iter().filter(|p| p.chat == chat).count()
    }

    /// Reads a message from `chat` as an answer. `Some(reply)` means it was
    /// one (the reply goes back to the chat, the message goes no further);
    /// `None` means it's an ordinary message.
    pub fn answer(&self, chat: impl Into<ChatRef>, text: &str) -> Option<String> {
        self.answer_from(chat, text, true)
    }

    /// `answer`, knowing whether the sender is the owner (M48): another
    /// member's `yes` in a group approves nothing and refuses nothing.
    pub fn answer_from(
        &self,
        chat: impl Into<ChatRef>,
        text: &str,
        by_owner: bool,
    ) -> Option<String> {
        if text.trim_start().starts_with('/') {
            return None;
        }
        let chat = chat.into();
        let mut inner = self.inner.lock().unwrap();
        let said = parse(text);
        if let Said::Yes(Some(c)) | Said::No(Some(c)) = &said {
            if inner.used.iter().any(|(u, ch, _)| u == c && *ch == chat) {
                return Some(format!(
                    "Question {c} was already answered; nothing more was run."
                ));
            }
        }
        let here: Vec<usize> = (0..inner.pending.len())
            .filter(|&i| inner.pending[i].chat == chat)
            .collect();
        if !by_owner {
            return match said {
                Said::Yes(_) | Said::No(_) if !here.is_empty() => {
                    Some("Only the owner can answer that question.".into())
                }
                _ => None,
            };
        }
        if here.is_empty() {
            let code = match &said {
                Said::Yes(c) | Said::No(c) => c.clone(),
                Said::Other => return None,
            };
            let late = match &code {
                Some(c) => inner.expired.iter().any(|(e, ch, _)| e == c && *ch == chat),
                None => inner.expired.iter().any(|(_, ch, _)| *ch == chat),
            };
            if late {
                return Some("That question expired and was refused; nothing was run. Ask the agent again if it's still needed.".into());
            }
            return code.filter(|c| is_code(c)).map(|c| {
                format!(
                    "No question with code {c} is waiting (it may have expired, or the bot restarted); nothing was run."
                )
            });
        }
        let find = |inner: &Inner, code: &str| {
            here.iter()
                .copied()
                .find(|&i| inner.pending[i].code == code)
        };
        match said {
            Said::Yes(None) if here.len() == 1 => {
                let p = take(&mut inner, here[0]);
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
                    let p = take(&mut inner, i);
                    let _ = p.tx.send(Answer::Yes);
                    Some(format!("Approved ({}): {}", p.code, p.what))
                }
                None => Some(format!(
                    "No question with code {code} is waiting; nothing was approved."
                )),
            },
            Said::No(Some(code)) => match find(&inner, &code) {
                Some(i) => {
                    let p = take(&mut inner, i);
                    let _ = p.tx.send(Answer::No(text.trim().to_string()));
                    Some(format!("Refused ({}): {}", p.code, p.what))
                }
                None => Some(format!("No question with code {code} is waiting.")),
            },
            Said::No(None) | Said::Other => {
                let mut refused = Vec::new();
                for &i in here.iter().rev() {
                    let p = take(&mut inner, i);
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
    fn the_page_lists_every_question_and_answers_one_by_its_code() {
        let a = Approvals::default();
        let (c1, mut r1) = a.open(1, "one");
        let (c2, mut r2) = a.open(ChatRef::new("discord", "9"), "two");
        let list = a.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].code, c1);
        assert_eq!(list[1].chat, ChatRef::new("discord", "9"));
        assert!(a.decide(&c2, true, "").unwrap().starts_with("Approved"));
        assert_eq!(r2.try_recv().unwrap(), Answer::Yes);
        assert!(a.decide(&c2, true, "").is_none(), "answered once");
        assert!(a
            .decide(&c1, false, "refused on the page")
            .unwrap()
            .starts_with("Refused"));
        assert_eq!(
            r1.try_recv().unwrap(),
            Answer::No("refused on the page".into())
        );
        assert!(a.list().is_empty());
    }

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

    #[test]
    fn a_yes_from_another_chat_does_not_approve() {
        let a = Approvals::default();
        let (code, mut rx) = a.open(ChatRef::new("telegram", "1"), "one");
        assert!(a.answer(ChatRef::new("telegram", "2"), "yes").is_none());
        let said = a
            .answer(ChatRef::new("telegram", "2"), &format!("yes {code}"))
            .unwrap();
        assert!(said.contains("nothing was run"), "{said}");
        assert!(rx.try_recv().is_err());
        assert_eq!(a.pending_in(ChatRef::new("telegram", "1")), 1);
    }

    #[test]
    fn a_replayed_yes_runs_nothing_and_its_code_is_not_reused() {
        let a = Approvals::default();
        let (code, mut rx) = a.open(1, "one");
        assert!(a
            .answer(1, &format!("yes {code}"))
            .unwrap()
            .starts_with("Approved"));
        assert_eq!(rx.try_recv().unwrap(), Answer::Yes);
        let again = a.answer(1, &format!("yes {code}")).unwrap();
        assert!(again.contains("already answered"), "{again}");
        let mut rest = Vec::new();
        for _ in 0..191 {
            let (c, rx) = a.open(1, "more");
            assert_ne!(c, code, "a used code is never drawn again");
            rest.push(rx);
        }
        assert!(a
            .answer(1, &format!("yes {code}"))
            .unwrap()
            .contains("already answered"));
    }

    #[test]
    fn codes_never_run_out() {
        let a = Approvals::default();
        let mut codes = std::collections::HashSet::new();
        let mut keep = Vec::new();
        for _ in 0..200 {
            let (c, rx) = a.open(1, "q");
            assert!(is_code(&c), "{c}");
            assert!(codes.insert(c), "distinct");
            keep.push(rx);
        }
        assert!(codes.iter().any(|c| c.len() == 3), "later ones grow");
    }

    #[test]
    fn a_slash_command_is_not_an_answer_and_refuses_nothing() {
        let a = Approvals::default();
        let (_c, mut rx) = a.open(1, "one");
        assert!(a.answer(1, "/model").is_none());
        assert!(a.answer(1, "  /status").is_none());
        assert!(rx.try_recv().is_err());
        assert_eq!(a.pending_in(1), 1);
    }

    #[test]
    fn a_non_owner_yes_approves_nothing() {
        let a = Approvals::default();
        let (code, mut rx) = a.open(1, "one");
        let said = a.answer_from(1, "yes", false).unwrap();
        assert!(said.contains("Only the owner"), "{said}");
        assert!(a.answer_from(1, &format!("yes {code}"), false).is_some());
        assert!(a.answer_from(1, "what is this?", false).is_none());
        assert!(rx.try_recv().is_err(), "nothing answered, nothing refused");
        assert_eq!(a.pending_in(1), 1);
        assert!(a.answer_from(2, "yes", false).is_none());
    }

    #[test]
    fn a_coded_yes_with_nothing_pending_runs_nothing() {
        let a = Approvals::default();
        let said = a.answer(1, "yes k4").unwrap();
        assert!(said.contains("No question with code k4"), "{said}");
        assert!(a.answer(1, "yes please").is_none());
        assert!(a.answer(1, "yes zz").is_none(), "not code-shaped");
    }

    #[test]
    fn the_list_says_how_long_is_left() {
        let a = Approvals::default();
        a.open_for(
            1,
            "one",
            Some(Duration::from_secs(600)),
            Some("model_default:abc".into()),
        );
        a.open(1, "two");
        let list = a.list();
        let left = list[0].left_secs.unwrap();
        assert!((598..=600).contains(&left), "{left}");
        assert_eq!(list[0].op.as_deref(), Some("model_default:abc"));
        assert!(list[1].left_secs.is_none());
    }
}
