//! Telegram Bot API adapter over long-polling `getUpdates` / `sendMessage`.
//! `base_url` is a full constructor argument (not hardcoded) specifically so
//! tests can point it at a local mock HTTP server instead of
//! `https://api.telegram.org` — there is no real bot token available in
//! this environment, so every behavior here is verified against a mock.
//!
//! Anyone can find a bot and message it, so only the chats in
//! `allowed_chats` reach the agent. With none configured the bot answers
//! each new chat once with that chat's id — the id is what goes into the
//! allow-list — and forwards nothing.

use crate::channel::{Channel, ChannelCapabilities};
use crate::error::GatewayError;
use crate::message::{InboundMessage, OutboundMessage};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc;

pub struct TelegramChannel {
    base_url: String,
    token: String,
    client: reqwest::Client,
    /// `getUpdates` offset: one past the highest `update_id` seen so far,
    /// so Telegram doesn't redeliver already-processed updates.
    offset: AtomicI64,
    /// Chats whose messages are forwarded. A group id admits the whole group.
    allowed_chats: Vec<i64>,
    /// Chats already told their id, while `allowed_chats` is empty.
    told: Mutex<HashSet<i64>>,
}

impl TelegramChannel {
    /// Real usage: talks to `https://api.telegram.org`.
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base_url(token, "https://api.telegram.org")
    }

    /// Test/self-hosted-Bot-API-server usage: talks to an arbitrary base URL.
    pub fn with_base_url(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        let mut builder = reqwest::Client::builder();
        // Test builds only: bypass any ambient proxy so tests against a local
        // mock server don't depend on NO_PROXY being set. Never affects release binaries.
        if cfg!(test) {
            builder = builder.no_proxy();
        }
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            client: builder.build().expect("reqwest client"),
            offset: AtomicI64::new(0),
            allowed_chats: Vec::new(),
            told: Mutex::new(HashSet::new()),
        }
    }

    /// The chats this bot answers; everyone else is ignored.
    pub fn with_allowed_chats(mut self, chats: Vec<i64>) -> Self {
        self.allowed_chats = chats;
        self
    }

    /// Whether `msg` may reach the agent. A refused chat gets one reply with
    /// its id while no chat is allowed yet (so setup can finish), and
    /// silence once the list is in use.
    async fn admits(&self, msg: &InboundMessage) -> bool {
        let Ok(chat) = msg.chat_id.parse::<i64>() else {
            return false;
        };
        if self.allowed_chats.contains(&chat) {
            return true;
        }
        tracing::info!(chat, sender = %msg.sender, "telegram: ignoring a chat that isn't in telegram_allowed_chats");
        if self.allowed_chats.is_empty() && self.told.lock().unwrap().insert(chat) {
            let text = format!(
                "This bot is private. Your chat id is {chat} — add it to telegram_allowed_chats in the ferrule config, or run `ferrule setup`."
            );
            let reply = OutboundMessage {
                channel: "telegram".into(),
                chat_id: msg.chat_id.clone(),
                text,
                reply_to: None,
                attachments: vec![],
            };
            if let Err(e) = self.send(reply).await {
                tracing::warn!(error = %e, "telegram: couldn't tell a chat its id");
            }
        }
        false
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.base_url, self.token, method)
    }

    fn parse_update(update: &Value) -> Option<InboundMessage> {
        let message = update.get("message")?;
        let chat_id = message.get("chat")?.get("id")?.as_i64()?.to_string();
        let message_id = message.get("message_id")?.as_i64()?.to_string();
        let text = message.get("text")?.as_str()?.to_string();
        let sender = message
            .get("from")
            .and_then(|f| f.get("username").or_else(|| f.get("first_name")))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let ts = message.get("date").and_then(|d| d.as_i64()).unwrap_or(0);
        Some(InboundMessage {
            channel: "telegram".into(),
            chat_id,
            sender,
            message_id,
            text,
            attachments: vec![],
            reply_to: None,
            ts,
        })
    }
}

#[async_trait::async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            edits: true,
            ..Default::default()
        }
    }

    /// Blocks forever, long-polling `getUpdates`. Returns only on a fatal
    /// error (bad response shape, request failure) or once the receiving
    /// side of `tx` is gone — matching every other `Channel::run` in this
    /// crate, an `Err` here just means "this adapter stopped," not a crash.
    async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        loop {
            let offset = self.offset.load(Ordering::SeqCst);
            let url = format!(
                "{}?timeout=30&offset={}",
                self.api_url("getUpdates"),
                offset
            );
            let resp =
                self.client.get(&url).send().await.map_err(|e| {
                    GatewayError::Channel(format!("getUpdates request failed: {e}"))
                })?;
            let body: Value = resp
                .json()
                .await
                .map_err(|e| GatewayError::Channel(format!("getUpdates: bad json: {e}")))?;
            if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                return Err(GatewayError::Channel(format!(
                    "getUpdates returned not-ok: {body}"
                )));
            }
            let updates = body
                .get("result")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default();
            for update in &updates {
                if let Some(update_id) = update.get("update_id").and_then(|v| v.as_i64()) {
                    self.offset.store(update_id + 1, Ordering::SeqCst);
                }
                if let Some(inbound) = Self::parse_update(update) {
                    if !self.admits(&inbound).await {
                        continue;
                    }
                    if tx.send(inbound).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        let mut payload = json!({ "chat_id": msg.chat_id, "text": msg.text });
        if let Some(reply_to) = &msg.reply_to {
            if let Ok(id) = reply_to.parse::<i64>() {
                payload["reply_to_message_id"] = json!(id);
            }
        }
        let resp = self
            .client
            .post(self.api_url("sendMessage"))
            .json(&payload)
            .send()
            .await
            .map_err(|e| GatewayError::Channel(format!("sendMessage request failed: {e}")))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(json!({}));
        if !status.is_success() || !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(GatewayError::Channel(format!(
                "sendMessage failed (status {status}): {body}"
            )));
        }
        Ok(())
    }

    async fn edit(&self, chat_id: &str, message_id: &str, text: &str) -> Result<(), GatewayError> {
        let payload = json!({ "chat_id": chat_id, "message_id": message_id.parse::<i64>().unwrap_or(0), "text": text });
        let resp = self
            .client
            .post(self.api_url("editMessageText"))
            .json(&payload)
            .send()
            .await
            .map_err(|e| GatewayError::Channel(format!("editMessageText request failed: {e}")))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(json!({}));
        if !status.is_success() || !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(GatewayError::Channel(format!(
                "editMessageText failed (status {status}): {body}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use tokio::time::{timeout, Duration};

    /// A minimal multi-request mock Bot API server: the first `getUpdates`
    /// call returns one canned update, every call after that returns an
    /// empty result (so a long-running `run()` loop doesn't spin on
    /// redelivering the same update); `sendMessage`/`editMessageText`
    /// bodies are captured for assertions.
    fn mock_telegram_server() -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sent_clone = sent.clone();
        let served_update = Arc::new(AtomicBool::new(false));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut buf = vec![0u8; 65536];
                let n = match stream.read(&mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if request.contains("getUpdates") {
                    if !served_update.swap(true, Ordering::SeqCst) {
                        r#"{"ok":true,"result":[{"update_id":100,"message":{"message_id":55,"chat":{"id":9999},"from":{"username":"max"},"text":"hi bot","date":1700000000}}]}"#.to_string()
                    } else {
                        r#"{"ok":true,"result":[]}"#.to_string()
                    }
                } else if request.contains("sendMessage") || request.contains("editMessageText") {
                    if let Some(idx) = request.find("\r\n\r\n") {
                        if let Ok(v) = serde_json::from_str::<Value>(&request[idx + 4..]) {
                            sent_clone.lock().unwrap().push(v);
                        }
                    }
                    r#"{"ok":true,"result":{}}"#.to_string()
                } else {
                    "{}".to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}"), sent)
    }

    #[tokio::test]
    async fn long_poll_forwards_message_then_send_and_edit_post_to_the_api() {
        let (base_url, sent) = mock_telegram_server();
        let channel = Arc::new(
            TelegramChannel::with_base_url("TESTTOKEN", base_url).with_allowed_chats(vec![9999]),
        );
        let (tx, mut rx) = mpsc::channel(8);
        let run_channel = channel.clone();
        let handle = tokio::spawn(async move { run_channel.run(tx).await });

        let inbound = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inbound.channel, "telegram");
        assert_eq!(inbound.chat_id, "9999");
        assert_eq!(inbound.message_id, "55");
        assert_eq!(inbound.text, "hi bot");
        assert_eq!(inbound.sender, "max");
        assert_eq!(inbound.ts, 1700000000);

        channel
            .send(OutboundMessage {
                channel: "telegram".into(),
                chat_id: "9999".into(),
                text: "reply".into(),
                reply_to: Some("55".into()),
                attachments: vec![],
            })
            .await
            .unwrap();
        channel.edit("9999", "55", "edited text").await.unwrap();

        let sent_bodies = sent.lock().unwrap();
        assert_eq!(sent_bodies.len(), 2);
        assert_eq!(sent_bodies[0]["text"], "reply");
        assert_eq!(sent_bodies[0]["chat_id"], "9999");
        assert_eq!(sent_bodies[0]["reply_to_message_id"], 55);
        assert_eq!(sent_bodies[1]["text"], "edited text");
        assert_eq!(sent_bodies[1]["message_id"], 55);
        drop(sent_bodies);

        handle.abort();
    }

    /// Runs a channel against the mock (whose one update is from chat 9999)
    /// and returns what reached the agent and what the bot sent.
    async fn poll_once(allowed: Vec<i64>) -> (Option<InboundMessage>, Vec<Value>) {
        let (base_url, sent) = mock_telegram_server();
        let channel = Arc::new(
            TelegramChannel::with_base_url("TESTTOKEN", base_url).with_allowed_chats(allowed),
        );
        let (tx, mut rx) = mpsc::channel(8);
        let run_channel = channel.clone();
        let handle = tokio::spawn(async move { run_channel.run(tx).await });
        let inbound = timeout(Duration::from_millis(700), rx.recv())
            .await
            .ok()
            .flatten();
        handle.abort();
        let sent = sent.lock().unwrap().clone();
        (inbound, sent)
    }

    #[tokio::test]
    async fn with_no_allowed_chats_the_bot_tells_the_sender_its_id_and_forwards_nothing() {
        let (inbound, sent) = poll_once(vec![]).await;
        assert!(inbound.is_none());
        assert_eq!(sent.len(), 1, "told exactly once: {sent:?}");
        assert_eq!(sent[0]["chat_id"], "9999");
        assert!(sent[0]["text"]
            .as_str()
            .unwrap()
            .contains("Your chat id is 9999"));
    }

    #[tokio::test]
    async fn each_refused_chat_is_told_only_once() {
        let (base_url, sent) = mock_telegram_server();
        let channel = TelegramChannel::with_base_url("TESTTOKEN", base_url);
        let msg = |chat: &str| InboundMessage {
            channel: "telegram".into(),
            chat_id: chat.into(),
            sender: "x".into(),
            message_id: "1".into(),
            text: "hi".into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        };
        for chat in ["5", "5", "6", "5"] {
            assert!(!channel.admits(&msg(chat)).await);
        }
        let sent = sent.lock().unwrap();
        let chats: Vec<&str> = sent
            .iter()
            .map(|b| b["chat_id"].as_str().unwrap())
            .collect();
        assert_eq!(chats, ["5", "6"]);
    }

    #[tokio::test]
    async fn a_chat_outside_a_non_empty_list_gets_silence() {
        let (inbound, sent) = poll_once(vec![1, -1001]).await;
        assert!(inbound.is_none());
        assert!(sent.is_empty(), "strangers learn nothing: {sent:?}");
    }

    #[test]
    fn capabilities_report_edits_but_not_reactions() {
        let channel = TelegramChannel::new("t");
        let caps = channel.capabilities();
        assert!(caps.edits);
        assert!(!caps.reactions);
    }
}
