//! A channel with no external service at all: reads one message per line
//! from any `AsyncBufRead` and writes replies to any `AsyncWrite`. Exists so
//! the gateway can be exercised end-to-end (real `Router`/`Gateway`, real
//! agent turns) without Telegram, mock HTTP servers, or any other moving
//! part — `LocalChannel::stdio` for a real terminal session, or the raw
//! `new` constructor over in-memory pipes for tests.

use crate::channel::{Channel, ChannelCapabilities};
use crate::error::GatewayError;
use crate::message::{InboundMessage, OutboundMessage};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;

static NEXT_LOCAL_ID: AtomicU64 = AtomicU64::new(1);

pub struct LocalChannel<R, W> {
    chat_id: String,
    reader: AsyncMutex<R>,
    writer: AsyncMutex<W>,
}

impl<R, W> LocalChannel<R, W>
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    pub fn new(chat_id: impl Into<String>, reader: R, writer: W) -> Self {
        Self {
            chat_id: chat_id.into(),
            reader: AsyncMutex::new(reader),
            writer: AsyncMutex::new(writer),
        }
    }
}

impl LocalChannel<tokio::io::BufReader<tokio::io::Stdin>, tokio::io::Stdout> {
    /// The real terminal adapter: one line of stdin in, one line of stdout out.
    pub fn stdio(chat_id: impl Into<String>) -> Self {
        Self::new(
            chat_id,
            tokio::io::BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
        )
    }
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[async_trait::async_trait]
impl<R, W> Channel for LocalChannel<R, W>
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    fn name(&self) -> &str {
        "local"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities::default()
    }

    async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        let mut reader = self.reader.lock().await;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break; // EOF: the source (stdin, a duplex pipe, …) closed.
            }
            let text = line.trim_end_matches(['\n', '\r']).to_string();
            if text.is_empty() {
                continue;
            }
            let msg = InboundMessage {
                channel: "local".into(),
                chat_id: self.chat_id.clone(),
                sender: "local".into(),
                sender_id: None,
                message_id: NEXT_LOCAL_ID.fetch_add(1, Ordering::SeqCst).to_string(),
                text,
                attachments: vec![],
                reply_to: None,
                ts: now_ts(),
            };
            if tx.send(msg).await.is_err() {
                break; // router side dropped — nothing left to serve.
            }
        }
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        let mut writer = self.writer.lock().await;
        writer.write_all(msg.text.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, BufReader};
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn run_forwards_each_line_as_an_inbound_message() {
        let (mut client_write, server_read) = tokio::io::duplex(1024);
        client_write.write_all(b"hello\nworld\n").await.unwrap();
        drop(client_write); // EOF once dropped

        let channel = LocalChannel::new(
            "cli-session",
            BufReader::new(server_read),
            tokio::io::sink(),
        );
        let (tx, mut rx) = mpsc::channel(8);
        timeout(Duration::from_secs(2), channel.run(tx))
            .await
            .unwrap()
            .unwrap();

        let m1 = rx.try_recv().unwrap();
        assert_eq!(m1.channel, "local");
        assert_eq!(m1.chat_id, "cli-session");
        assert_eq!(m1.text, "hello");
        let m2 = rx.try_recv().unwrap();
        assert_eq!(m2.text, "world");
        assert!(rx.try_recv().is_err(), "no third message expected");
    }

    #[tokio::test]
    async fn blank_lines_are_skipped() {
        let (mut client_write, server_read) = tokio::io::duplex(1024);
        client_write.write_all(b"\n\nonly one\n").await.unwrap();
        drop(client_write);

        let channel = LocalChannel::new("s", BufReader::new(server_read), tokio::io::sink());
        let (tx, mut rx) = mpsc::channel(8);
        timeout(Duration::from_secs(2), channel.run(tx))
            .await
            .unwrap()
            .unwrap();

        let m = rx.try_recv().unwrap();
        assert_eq!(m.text, "only one");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn send_writes_text_with_trailing_newline() {
        let (server_write, mut client_read) = tokio::io::duplex(1024);
        let channel = LocalChannel::new("s", BufReader::new(tokio::io::empty()), server_write);
        channel
            .send(OutboundMessage {
                channel: "local".into(),
                chat_id: "s".into(),
                text: "hi there".into(),
                reply_to: None,
                attachments: vec![],
            })
            .await
            .unwrap();

        let mut buf = [0u8; 64];
        let n = timeout(Duration::from_secs(2), client_read.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"hi there\n");
    }
}
