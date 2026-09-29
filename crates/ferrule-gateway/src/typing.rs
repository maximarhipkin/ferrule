//! M41: "typing…" in the chat while a turn runs (docs/m41-daily-use.md §3).
//! One refresher per turn; it ends before the reply goes out, on `/stop`,
//! and for the rest of the turn after a rate limit.

use crate::channel::Channel;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// What a channel did with a typing call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Typing {
    /// No typing indicator here (the default).
    Unsupported,
    /// Shown. It fades on its own, so it's shown again after `again_in`.
    Shown { again_in: Duration },
    /// Rate-limited: no more typing this turn.
    Limited,
    /// It didn't go: no more typing this turn.
    Failed,
}

/// A turn's typing refresher.
pub(crate) struct Typist {
    done: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Typist {
    /// Shows typing in `chat_id` until [`Self::stop`] or `stopped`
    /// resolves (a `/stop`).
    pub(crate) fn start(
        channel: Arc<dyn Channel>,
        chat_id: String,
        message_id: String,
        stopped: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        let (done, mut done_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            tokio::pin!(stopped);
            let mut shown = false;
            loop {
                match channel.typing(&chat_id, &message_id, true).await {
                    Typing::Shown { again_in } => {
                        shown = true;
                        tokio::select! {
                            _ = &mut done_rx => break,
                            _ = &mut stopped => break,
                            _ = tokio::time::sleep(again_in) => {}
                        }
                    }
                    Typing::Limited => {
                        tracing::debug!(chat = %chat_id, "typing: rate limited, none for the rest of this turn");
                        return;
                    }
                    Typing::Unsupported | Typing::Failed => break,
                }
            }
            if shown {
                let _ = channel.typing(&chat_id, &message_id, false).await;
            }
        });
        Self {
            done: Some(done),
            task,
        }
    }

    /// Ends it, and returns once nothing more will be sent: a call still in
    /// flight lands before the reply, not after it.
    pub(crate) async fn stop(mut self) {
        drop(self.done.take());
        let _ = (&mut self.task).await;
    }
}

impl Drop for Typist {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GatewayError, InboundMessage, OutboundMessage};
    use std::sync::Mutex;
    use tokio::sync::{mpsc, Notify};

    /// Typing calls, with what the n-th one answers.
    struct Chat {
        calls: Mutex<Vec<bool>>,
        answer: fn(usize) -> Typing,
    }

    #[async_trait::async_trait]
    impl Channel for Chat {
        fn name(&self) -> &str {
            "chat"
        }
        async fn run(&self, _tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
            Ok(())
        }
        async fn send(&self, _msg: OutboundMessage) -> Result<(), GatewayError> {
            Ok(())
        }
        async fn typing(&self, _chat: &str, _message: &str, on: bool) -> Typing {
            let mut calls = self.calls.lock().unwrap();
            calls.push(on);
            if on {
                (self.answer)(calls.iter().filter(|c| **c).count())
            } else {
                Typing::Unsupported
            }
        }
    }

    fn chat(answer: fn(usize) -> Typing) -> Arc<Chat> {
        Arc::new(Chat {
            calls: Mutex::default(),
            answer,
        })
    }

    fn every_50ms(_: usize) -> Typing {
        Typing::Shown {
            again_in: Duration::from_millis(50),
        }
    }

    #[tokio::test]
    async fn it_refreshes_until_stopped_then_clears() {
        let c = chat(every_50ms);
        let t = Typist::start(c.clone(), "1".into(), "2".into(), std::future::pending());
        tokio::time::sleep(Duration::from_millis(180)).await;
        t.stop().await;
        let calls = c.calls.lock().unwrap().clone();
        assert!(calls.len() >= 4, "{calls:?}");
        assert_eq!(calls.last(), Some(&false));
        assert_eq!(calls.iter().filter(|c| !**c).count(), 1);
        let n = calls.len();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(c.calls.lock().unwrap().len(), n, "nothing after stop");
    }

    #[tokio::test]
    async fn a_rate_limit_ends_it_for_the_turn() {
        let c = chat(|n| {
            if n >= 2 {
                Typing::Limited
            } else {
                every_50ms(n)
            }
        });
        let t = Typist::start(c.clone(), "1".into(), "2".into(), std::future::pending());
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(*c.calls.lock().unwrap(), vec![true, true]);
        t.stop().await;
        assert_eq!(*c.calls.lock().unwrap(), vec![true, true]);
    }

    #[tokio::test]
    async fn stop_ends_it_at_once() {
        let c = chat(|_| Typing::Shown {
            again_in: Duration::from_secs(60),
        });
        let stop = Arc::new(Notify::new());
        let s = stop.clone();
        let t = Typist::start(c.clone(), "1".into(), "2".into(), async move {
            s.notified().await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.notify_waiters();
        tokio::time::timeout(Duration::from_secs(1), async {
            while c.calls.lock().unwrap().len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cleared right after /stop");
        assert_eq!(*c.calls.lock().unwrap(), vec![true, false]);
        t.stop().await;
    }

    #[tokio::test]
    async fn a_channel_without_typing_is_asked_once() {
        let c = chat(|_| Typing::Unsupported);
        let t = Typist::start(c.clone(), "1".into(), "2".into(), std::future::pending());
        tokio::time::sleep(Duration::from_millis(50)).await;
        t.stop().await;
        assert_eq!(*c.calls.lock().unwrap(), vec![true]);
    }
}
