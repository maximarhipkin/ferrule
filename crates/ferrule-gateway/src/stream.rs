//! M27: a reply that grows in place while the model writes it. The agent's
//! reply stream feeds a task that posts a first message once there's
//! enough to show, edits it at most once a second, rolls over to a new
//! message before the channel's size cap, and ends with an edit that
//! carries exactly the text `send` would have sent. Only for a channel
//! that can edit; everything else gets the final text as before.

use crate::channel::Channel;
use crate::error::GatewayError;
use crate::message::OutboundMessage;
use ferrule_core::{Delta, DeltaSink};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// How a streamed reply is paced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPacing {
    /// The first message goes out this long after the turn started…
    pub first_after: Duration,
    /// …or once there are this many characters, whichever comes first.
    pub first_chars: usize,
    /// At most one send or edit per chat this often.
    pub every: Duration,
    /// A message holds at most this much, counted in UTF-16 units as
    /// Telegram counts (its cap is 4096; the rest is margin).
    pub limit: usize,
}

impl Default for StreamPacing {
    fn default() -> Self {
        Self {
            first_after: Duration::from_secs(1),
            first_chars: 60,
            every: Duration::from_secs(1),
            limit: 4000,
        }
    }
}

/// What stands in for a message a longer preview needed and the final text
/// doesn't.
const LEFTOVER: &str = "…";

/// Tries at the final text a 429 gets before it gives up on edits.
const FINAL_TRIES: u32 = 3;

enum Cmd {
    Delta(Delta),
    Final(String),
}

/// One turn's streamed reply. [`StreamingReply::sink`] goes to the agent;
/// [`StreamingReply::finish`] delivers the answer.
pub(crate) struct StreamingReply {
    tx: mpsc::UnboundedSender<Cmd>,
    task: JoinHandle<()>,
}

impl StreamingReply {
    /// Starts the editor for a reply to `base` (its channel, chat and
    /// reply-to; the text is ignored). `progress` is called on every delta,
    /// so the watchdog sees a streaming turn moving.
    pub(crate) fn start(
        channel: Arc<dyn Channel>,
        base: OutboundMessage,
        pacing: StreamPacing,
        progress: impl Fn() + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let editor = Editor {
            channel,
            base,
            pacing,
            started: Instant::now(),
            text: String::new(),
            shown: Vec::new(),
            last_call: None,
            blocked_until: None,
            broken: false,
        };
        let task = tokio::spawn(editor.run(rx, progress));
        Self { tx, task }
    }

    /// The agent's end: every delta goes to the editor, which never blocks
    /// the model call.
    pub(crate) fn sink(&self) -> DeltaSink {
        let tx = self.tx.clone();
        DeltaSink::new(move |d| {
            let _ = tx.send(Cmd::Delta(d));
        })
    }

    /// Delivers `text` as the reply: by the normal `send` when nothing was
    /// shown yet, else by editing what was.
    pub(crate) async fn finish(self, text: String) {
        let _ = self.tx.send(Cmd::Final(text));
        let _ = self.task.await;
    }
}

struct Editor {
    channel: Arc<dyn Channel>,
    base: OutboundMessage,
    pacing: StreamPacing,
    started: Instant,
    /// The current model call's text so far.
    text: String,
    /// The messages sent so far: their ids and what each shows.
    shown: Vec<(String, String)>,
    last_call: Option<Instant>,
    /// A 429's `retry_after`: nothing goes to the chat before this.
    blocked_until: Option<Instant>,
    /// A send or edit failed for another reason: the preview stops, and
    /// the final text still goes out.
    broken: bool,
}

impl Editor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>, progress: impl Fn()) {
        loop {
            let due = self.next_due();
            let cmd = match due {
                Some(at) => tokio::select! {
                    cmd = rx.recv() => cmd,
                    _ = tokio::time::sleep_until(at.into()) => {
                        self.step().await;
                        continue;
                    }
                },
                None => rx.recv().await,
            };
            match cmd {
                Some(Cmd::Delta(d)) => {
                    progress();
                    match d {
                        Delta::Text(t) => self.text.push_str(&t),
                        Delta::Reset => self.text.clear(),
                        Delta::Progress => {}
                    }
                }
                Some(Cmd::Final(text)) => return self.finish(text).await,
                // The lane went away without an answer.
                None => return,
            }
        }
    }

    /// The chunks the preview should show (a message a longer preview
    /// needed shows "…"), when they differ from what's shown.
    fn wanted(&self) -> Option<Vec<String>> {
        if self.broken || self.text.trim().is_empty() {
            return None;
        }
        let mut want = chunks(&self.text, self.pacing.limit);
        while want.len() < self.shown.len() {
            want.push(LEFTOVER.into());
        }
        let differs =
            want.len() > self.shown.len() || self.shown.iter().zip(&want).any(|((_, s), w)| s != w);
        differs.then_some(want)
    }

    /// When the preview should next change, if it should.
    fn next_due(&self) -> Option<Instant> {
        self.wanted()?;
        let mut at = match self.last_call {
            Some(t) => t + self.pacing.every,
            None if self.text.encode_utf16().count() >= self.pacing.first_chars => Instant::now(),
            None => self.started + self.pacing.first_after,
        };
        if let Some(b) = self.blocked_until {
            at = at.max(b);
        }
        Some(at)
    }

    /// One send or edit toward the preview: the first message that differs.
    async fn step(&mut self) {
        let Some(want) = self.wanted() else { return };
        self.last_call = Some(Instant::now());
        let at = self
            .shown
            .iter()
            .zip(&want)
            .position(|((_, s), w)| s != w)
            .unwrap_or(self.shown.len());
        let result = match self.shown.get(at) {
            Some((id, _)) => {
                let id = id.clone();
                self.edit(&id, &want[at]).await.map(|()| None)
            }
            None => self.post(&want[at], at == 0).await,
        };
        match result {
            Ok(None) => self.shown[at].1 = want[at].clone(),
            Ok(Some(id)) => self.shown.push((id, want[at].clone())),
            Err(GatewayError::RateLimited { retry_after }) => {
                self.blocked_until = Some(Instant::now() + retry_after);
            }
            Err(e) => {
                tracing::warn!(error = %e, "streamed reply: the preview stopped; the answer still goes out");
                self.broken = true;
            }
        }
    }

    async fn edit(&self, id: &str, text: &str) -> Result<(), GatewayError> {
        match self.channel.edit(&self.base.chat_id, id, text).await {
            Err(GatewayError::Channel(e)) if e.contains("message is not modified") => Ok(()),
            other => other,
        }
    }

    /// A new message; `Ok(None)` can't be, since a channel that gives no id
    /// can't be edited — that ends the preview.
    async fn post(&self, text: &str, first: bool) -> Result<Option<String>, GatewayError> {
        let id = self.channel.post(self.message(text, first)).await?;
        id.map(Some)
            .ok_or_else(|| GatewayError::Channel("the channel gave no message id".into()))
    }

    fn message(&self, text: &str, first: bool) -> OutboundMessage {
        OutboundMessage {
            text: text.to_string(),
            reply_to: if first {
                self.base.reply_to.clone()
            } else {
                None
            },
            ..self.base.clone()
        }
    }

    /// The final text: today's `send` when nothing was shown, else an edit
    /// per shown message and a send per extra chunk. Leftover messages
    /// become "…". If an edit fails for anything but a 429, the whole
    /// answer goes out as new messages, so it's never lost.
    async fn finish(self, text: String) {
        if self.shown.is_empty() {
            self.wait().await;
            if let Err(e) = self.channel.send(self.message(&text, true)).await {
                tracing::error!(error = %e, "failed to deliver reply");
            }
            return;
        }
        let want = chunks(&text, self.pacing.limit);
        let n = want.len().max(self.shown.len());
        for i in 0..n {
            let wanted = want.get(i).map(String::as_str).unwrap_or(LEFTOVER);
            let done = match self.shown.get(i) {
                Some((_, s)) if s == wanted => Ok(()),
                Some((id, _)) => self.retrying(Some(id), wanted).await,
                None => self.retrying(None, wanted).await,
            };
            if let Err(e) = done {
                tracing::warn!(error = %e, "streamed reply: the final edit failed; sending the answer anew");
                for (j, chunk) in want.iter().enumerate() {
                    if let Err(e) = self.channel.send(self.message(chunk, j == 0)).await {
                        tracing::error!(error = %e, "failed to deliver reply");
                        return;
                    }
                }
                return;
            }
        }
    }

    /// Edits message `id` (or posts a new one) to `text`, waiting out a
    /// 429 before the call and after each one it gets, a few times.
    async fn retrying(&self, id: Option<&String>, text: &str) -> Result<(), GatewayError> {
        self.wait().await;
        let mut tries = 1;
        loop {
            let result = match id {
                Some(id) => self.edit(id, text).await,
                None => self
                    .channel
                    .post(self.message(text, false))
                    .await
                    .map(|_| ()),
            };
            match result {
                Err(GatewayError::RateLimited { retry_after }) if tries < FINAL_TRIES => {
                    tries += 1;
                    tokio::time::sleep(retry_after).await;
                }
                other => return other,
            }
        }
    }

    async fn wait(&self) {
        if let Some(b) = self.blocked_until {
            tokio::time::sleep_until(b.into()).await;
        }
    }
}

/// `text` cut into pieces of at most `limit` UTF-16 units, each break at a
/// newline or else a space when one is in the piece's last fifth. Never
/// empty: an empty text is one empty piece.
pub fn chunks(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        // The byte index where the piece would pass the limit.
        let mut units = 0;
        let hard = rest.char_indices().find_map(|(i, c)| {
            units += c.len_utf16();
            (units > limit).then_some(i)
        });
        let Some(hard) = hard else {
            out.push(rest.to_string());
            return out;
        };
        let head = &rest[..hard];
        let floor = head
            .char_indices()
            .nth(head.chars().count() * 4 / 5)
            .map_or(0, |(i, _)| i);
        let cut = head[floor..]
            .rfind('\n')
            .or_else(|| head[floor..].rfind(' '))
            .map(|i| floor + i + 1)
            .filter(|&i| i > 0)
            .unwrap_or(hard);
        out.push(rest[..cut].trim_end().to_string());
        rest = &rest[cut..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_break_at_a_newline_then_a_space_near_the_end() {
        assert_eq!(chunks("short", 10), ["short"]);
        assert_eq!(chunks("", 10), [""]);
        assert_eq!(chunks("aaaaaaaa\nbbbbbbbb", 10), ["aaaaaaaa", "bbbbbbbb"]);
        assert_eq!(chunks("aaaaaaaa bbbbbbbb", 10), ["aaaaaaaa", "bbbbbbbb"]);
        // No break in the last fifth: a hard cut at the limit.
        assert_eq!(chunks("aaaa bbbbbbbbbbbb", 10), ["aaaa bbbbb", "bbbbbbb"]);
        let long = "word ".repeat(2000);
        let pieces = chunks(&long, 4000);
        assert!(pieces.iter().all(|p| p.encode_utf16().count() <= 4000));
        assert_eq!(pieces.concat().replace(' ', ""), long.replace(' ', ""));
    }

    #[test]
    fn chunks_count_utf16_units_as_telegram_does() {
        // Each emoji is two units: five fit in ten.
        let pieces = chunks(&"😀".repeat(7), 10);
        assert_eq!(pieces, ["😀".repeat(5), "😀".repeat(2)]);
    }
}
