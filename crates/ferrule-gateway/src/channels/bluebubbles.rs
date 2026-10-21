//! M59: iMessage through BlueBubbles Server (<https://bluebubbles.app>),
//! which the owner runs on a Mac signed in to Messages. Ferrule talks to
//! its REST API only: what comes in is a poll of `POST
//! /api/v1/message/query` every few seconds (nothing listens on this
//! machine, and the Mac may be another one on the LAN); what goes out is
//! `POST /api/v1/message/text` and `/api/v1/message/attachment`. Every call
//! carries the server password as BlueBubbles' documented `password` query
//! parameter, so no URL is ever logged (errors leave it out).
//!
//! Who gets in: a direct conversation with an allow-listed phone number or
//! Apple ID email. Group chats and the Mac's own messages are ignored, and
//! strangers are never answered. No buttons, edits or reactions; typing
//! and quoted replies need BlueBubbles' Private API (`private_api`).

use crate::channel::{Channel, ChannelCapabilities};
use crate::channels::access::{self, Access, Dm};
use crate::channels::files::{self, Inbox, Refused, Saved};
use crate::channels::ws;
use crate::error::GatewayError;
use crate::message::{Attachment, InboundMessage, OutboundMessage};
use crate::stream::chunks;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

/// The adapter's name, the session ids' prefix, `[gateway.imessage]`.
pub const NAME: &str = "imessage";
/// One message's text; a longer reply goes as several.
pub const MESSAGE_LIMIT: usize = 4000;
const REQUEST_DEADLINE: Duration = Duration::from_secs(60);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// Messages asked for per page.
const PAGE: usize = 100;
/// Each poll looks this far behind the newest message seen, so a message
/// Messages files late (synced from iCloud with its own time) still comes
/// in; what was already seen is skipped by its guid.
const LOOKBACK_MS: i64 = 120_000;
/// A saved position older than this isn't replayed at start.
const MAX_REPLAY_MS: i64 = 24 * 3600 * 1000;
/// Guids remembered against repeats.
const SEEN_MAX: usize = 2_000;

/// Waits and retries, shortened in tests.
#[derive(Clone)]
struct Timing {
    backoff_min: Duration,
    backoff_max: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff_min: Duration::from_secs(2),
            backoff_max: Duration::from_secs(60),
        }
    }
}

/// Everything the adapter needs.
#[derive(Clone, Debug)]
pub struct BlueBubblesConfig {
    /// The server's address, `http://192.168.1.20:1234`.
    pub url: String,
    /// The server password.
    pub password: String,
    /// BlueBubbles' Private API is set up on the Mac: sends go through it,
    /// with typing and quoted replies.
    pub private_api: bool,
    /// How often to look for new messages.
    pub poll: Duration,
    /// Where files people send are saved; `None`: not saved.
    pub inbox: Option<Inbox>,
    /// Where the poll's position is kept across restarts.
    pub cursor: Option<PathBuf>,
}

pub struct BlueBubblesChannel {
    cfg: BlueBubblesConfig,
    base: String,
    client: reqwest::Client,
    access: Access,
    timing: Timing,
    /// Each person's conversation guid (`iMessage;-;+1555…`), as seen.
    chats: Mutex<HashMap<String, String>>,
    seen: Mutex<(HashSet<String>, VecDeque<String>)>,
    last_poll: Mutex<Option<SystemTime>>,
    problem: Mutex<Option<String>>,
}

/// An address as a chat id: an Apple ID email in lower case, a phone
/// number as `+` and its digits (`+1 (555) 123-4567` → `+15551234567`).
pub fn normalize(address: &str) -> String {
    let a = address.trim();
    if a.contains('@') {
        return a.to_ascii_lowercase();
    }
    let digits: String = a.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        a.to_string()
    } else {
        format!("+{digits}")
    }
}

/// The person in a direct conversation's guid (`iMessage;-;+1555…`).
fn address_in(guid: &str) -> Option<&str> {
    guid.split_once(";-;")
        .map(|(_, a)| a)
        .filter(|a| !a.is_empty())
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// What the server said about itself.
#[derive(Debug, Clone)]
pub struct Probe {
    pub server_version: String,
    pub os_version: Option<String>,
    pub private_api: bool,
    /// The Apple ID Messages is signed in with, when the server says.
    pub icloud: Option<String>,
}

impl Probe {
    /// "BlueBubbles 1.9.9 · macOS 14.5 · max@icloud.com · Private API".
    pub fn summary(&self) -> String {
        let mut s = format!("BlueBubbles {}", self.server_version);
        if let Some(os) = &self.os_version {
            s.push_str(&format!(" · macOS {os}"));
        }
        if let Some(id) = &self.icloud {
            s.push_str(&format!(" · {id}"));
        }
        s.push_str(if self.private_api {
            " · Private API"
        } else {
            " · no Private API"
        });
        s
    }
}

/// Asks the server about itself: nothing is sent.
pub async fn probe(cfg: BlueBubblesConfig) -> Result<Probe, String> {
    if !(cfg.url.starts_with("http://") || cfg.url.starts_with("https://")) {
        return Err(format!(
            "the server's address is a URL like http://192.168.1.20:1234, not {:?}",
            cfg.url
        ));
    }
    let ch = BlueBubblesChannel::new(cfg);
    let v = ch
        .api(ch.client.get(ch.url("/api/v1/server/info")), "server/info")
        .await?;
    let d = &v["data"];
    let Some(version) = d["server_version"].as_str() else {
        return Err(format!("{} doesn't answer as BlueBubbles Server", ch.base));
    };
    Ok(Probe {
        server_version: version.to_string(),
        os_version: d["os_version"].as_str().map(str::to_string),
        private_api: d["private_api"].as_bool().unwrap_or(false),
        icloud: d["detected_icloud"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

impl BlueBubblesChannel {
    pub fn new(cfg: BlueBubblesConfig) -> Self {
        let base = cfg.url.trim_end_matches('/').to_string();
        Self {
            client: ws::http_client(&base, CONNECT_DEADLINE, REQUEST_DEADLINE),
            base,
            access: Self::access(vec![]),
            timing: Timing::default(),
            chats: Mutex::new(HashMap::new()),
            seen: Mutex::new((HashSet::new(), VecDeque::new())),
            last_poll: Mutex::new(None),
            problem: Mutex::new(None),
            cfg,
        }
    }

    fn access(users: Vec<String>) -> Access {
        Access::new(NAME, "iMessage address", users, vec![])
            .with_keys("[gateway.imessage] allowed_users", "group chats")
            // The owner's own Messages: a stranger is never answered.
            .silent()
    }

    /// Who may write to it: phone numbers and Apple ID emails.
    pub fn with_allowed(mut self, users: Vec<String>) -> Self {
        self.access = Self::access(users.iter().map(|u| normalize(u)).collect());
        self
    }

    /// Tests: waits in milliseconds, not seconds.
    #[doc(hidden)]
    pub fn with_fast_retries(mut self) -> Self {
        self.timing = Timing {
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(100),
        };
        self
    }

    /// `path` on the server, with the password. Never logged.
    fn url(&self, path: &str) -> String {
        format!(
            "{}{path}?password={}",
            self.base,
            urlencode(&self.cfg.password)
        )
    }

    fn method(&self) -> &'static str {
        if self.cfg.private_api {
            "private-api"
        } else {
            "apple-script"
        }
    }

    /// One call: its JSON answer, or why not in words (never the URL).
    async fn api(&self, req: reqwest::RequestBuilder, what: &str) -> Result<Value, String> {
        let resp = req.send().await.map_err(|e| {
            if e.is_connect() {
                format!(
                    "nothing answers at {}: is BlueBubbles Server running on the Mac?",
                    self.base
                )
            } else {
                format!("BlueBubbles at {} failed: {}", self.base, e.without_url())
            }
        })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(v);
        }
        if status.as_u16() == 401 {
            return Err(
                "BlueBubbles refused the server password ([gateway.imessage] password_env)".into(),
            );
        }
        let said = v["error"]["message"]
            .as_str()
            .or(v["message"].as_str())
            .map(|m| format!(": {}", crate::health::clip(m, 300)))
            .unwrap_or_default();
        Err(format!("BlueBubbles {what} failed: status {status}{said}"))
    }

    /// Where a reply to `chat` goes: its conversation as seen, else the
    /// iMessage conversation with that address.
    fn chat_guid(&self, chat: &str) -> String {
        if chat.contains(';') {
            return chat.to_string();
        }
        self.chats
            .lock()
            .unwrap()
            .get(chat)
            .cloned()
            .unwrap_or_else(|| format!("iMessage;-;{chat}"))
    }

    /// Text and files: all of `msg`; the last message's guid.
    async fn deliver(&self, msg: &OutboundMessage) -> Result<Option<String>, GatewayError> {
        let chat = self.chat_guid(&msg.chat_id);
        let mut reply = msg.reply_to.clone().filter(|_| self.cfg.private_api);
        let mut last = None;
        for piece in chunks(&msg.text, MESSAGE_LIMIT)
            .into_iter()
            .filter(|p| !p.trim().is_empty())
        {
            let mut body = json!({
                "chatGuid": chat,
                "tempGuid": uuid::Uuid::new_v4().to_string(),
                "message": piece,
                "method": self.method(),
            });
            if let Some(r) = reply.take() {
                body["selectedMessageGuid"] = json!(r);
            }
            let v = self
                .api(
                    self.client
                        .post(self.url("/api/v1/message/text"))
                        .json(&body),
                    "message/text",
                )
                .await
                .map_err(|e| GatewayError::Channel(format!("{NAME}: {e}")))?;
            last = v["data"]["guid"].as_str().map(str::to_string).or(last);
        }
        for att in &msg.attachments {
            let (name, mime, bytes) = files::read_outgoing(att)?;
            let mime = if att.kind.contains('/') {
                att.kind.as_str()
            } else {
                mime
            };
            let name = files::safe_name(&name);
            let temp = uuid::Uuid::new_v4().to_string();
            let (boundary, body) = files::multipart(
                &[
                    ("chatGuid", chat.as_str()),
                    ("tempGuid", temp.as_str()),
                    ("name", name.as_str()),
                    ("method", self.method()),
                ],
                ("attachment", &name, mime, &bytes),
            );
            let v = self
                .api(
                    self.client
                        .post(self.url("/api/v1/message/attachment"))
                        .header(
                            "content-type",
                            format!("multipart/form-data; boundary={boundary}"),
                        )
                        .body(body),
                    "message/attachment",
                )
                .await
                .map_err(|e| GatewayError::Channel(format!("{NAME}: {e}")))?;
            last = v["data"]["guid"].as_str().map(str::to_string).or(last);
        }
        Ok(last)
    }

    /// A short answer from the adapter itself (a refused file).
    async fn tell(&self, chat: &str, text: String) {
        let msg = OutboundMessage {
            channel: NAME.into(),
            chat_id: chat.to_string(),
            text,
            reply_to: None,
            attachments: vec![],
        };
        if let Err(e) = self.deliver(&msg).await {
            tracing::warn!("{NAME}: couldn't answer {chat}: {e}");
        }
    }

    fn set_problem(&self, p: Option<String>) {
        *self.problem.lock().unwrap() = p;
    }

    /// Whether `guid` is new (and now seen).
    fn first_time(&self, guid: &str) -> bool {
        let mut seen = self.seen.lock().unwrap();
        if !seen.0.insert(guid.to_string()) {
            return false;
        }
        seen.1.push_back(guid.to_string());
        while seen.1.len() > SEEN_MAX {
            if let Some(old) = seen.1.pop_front() {
                seen.0.remove(&old);
            }
        }
        true
    }

    /// Where the poll starts: the saved position when it's recent, else
    /// now (history isn't replayed).
    fn start_cursor(&self) -> i64 {
        let now = now_ms();
        self.cfg
            .cursor
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|c| *c <= now && now - c <= MAX_REPLAY_MS)
            .unwrap_or(now)
    }

    fn save_cursor(&self, cursor: i64) {
        let Some(path) = &self.cfg.cursor else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(path, cursor.to_string()) {
            tracing::debug!("{NAME}: couldn't save the poll's position: {e}");
        }
    }

    /// One page of messages after `after` (ms), oldest first.
    async fn page(&self, after: i64, offset: usize) -> Result<Vec<Value>, String> {
        let body = json!({
            "limit": PAGE,
            "offset": offset,
            "sort": "ASC",
            "after": after.max(0),
            "with": ["chat", "attachment", "handle"],
        });
        let v = self
            .api(
                self.client
                    .post(self.url("/api/v1/message/query"))
                    .json(&body),
                "message/query",
            )
            .await?;
        Ok(v["data"].as_array().cloned().unwrap_or_default())
    }

    /// Every message after `cursor` (less the look-back): each one not
    /// seen yet is handed to `each`. The newest time seen.
    async fn sweep(&self, cursor: i64, mut each: impl FnMut(Value)) -> Result<i64, String> {
        let mut newest = cursor;
        let after = cursor - LOOKBACK_MS;
        for n in 0..10 {
            let list = self.page(after, n * PAGE).await?;
            let full = list.len() >= PAGE;
            for m in list {
                newest = newest.max(m["dateCreated"].as_i64().unwrap_or(0));
                let Some(guid) = m["guid"].as_str() else {
                    continue;
                };
                if self.first_time(guid) {
                    each(m);
                }
            }
            if !full {
                break;
            }
        }
        Ok(newest)
    }

    async fn run_poll(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        let mut cursor = self.start_cursor();
        let mut primed = false;
        let mut backoff = self.timing.backoff_min;
        loop {
            if tx.is_closed() {
                return Ok(());
            }
            let mut fresh = vec![];
            let swept = if primed {
                self.sweep(cursor, |m| fresh.push(m)).await
            } else {
                // What the look-back finds at or before the start is
                // history: seen, not answered.
                let start = cursor;
                self.sweep(cursor, |m| {
                    if m["dateCreated"].as_i64().unwrap_or(0) > start {
                        fresh.push(m);
                    }
                })
                .await
            };
            match swept {
                Ok(newest) => {
                    primed = true;
                    cursor = newest;
                    *self.last_poll.lock().unwrap() = Some(SystemTime::now());
                    self.set_problem(None);
                    backoff = self.timing.backoff_min;
                    for m in fresh {
                        if let Some(msg) = self.message(&m).await {
                            if tx.send(msg).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    self.save_cursor(cursor);
                    tokio::time::sleep(self.cfg.poll).await;
                }
                Err(why) => {
                    tracing::warn!("{NAME}: {why}");
                    self.set_problem(Some(why));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.timing.backoff_max);
                }
            }
        }
    }

    /// A message for the agent, when it is one and allowed.
    async fn message(&self, m: &Value) -> Option<InboundMessage> {
        if m["isFromMe"].as_bool().unwrap_or(false) {
            return None;
        }
        // Tapbacks and group events.
        if m["associatedMessageGuid"]
            .as_str()
            .is_some_and(|g| !g.is_empty())
            || m["itemType"].as_i64().is_some_and(|t| t != 0)
        {
            return None;
        }
        let guid = m["guid"].as_str()?.to_string();
        let chat = m["chats"][0]["guid"].as_str()?.to_string();
        let Some(in_guid) = address_in(&chat) else {
            tracing::debug!("{NAME}: a group chat's message is ignored");
            return None;
        };
        let address = normalize(m["handle"]["address"].as_str().unwrap_or(in_guid));
        let text: String = m["text"]
            .as_str()
            .unwrap_or("")
            .chars()
            .filter(|c| *c != '\u{FFFC}')
            .collect();
        let text = text.trim().to_string();
        match self.access.dm(&address, &address, &text) {
            Dm::Admit => {}
            Dm::Tell(t) | Dm::Paired(t) => {
                self.tell(&address, t).await;
                return None;
            }
            Dm::Drop => return None,
        }
        self.chats.lock().unwrap().insert(address.clone(), chat);
        self.data(m, &address, &guid, text).await
    }

    /// A message's text and files, for `chat`.
    async fn data(
        &self,
        m: &Value,
        chat: &str,
        guid: &str,
        mut text: String,
    ) -> Option<InboundMessage> {
        let mut saved: Vec<Saved> = vec![];
        let mut refused: Vec<Refused> = vec![];
        for att in m["attachments"].as_array().into_iter().flatten() {
            let mime = att["mimeType"].as_str();
            match self.fetch(att, guid).await {
                Ok(s) => saved.push(s),
                Err(_) if self.cfg.inbox.is_none() => {
                    let kind = access::file_kind(mime);
                    if text.trim().is_empty() {
                        self.tell(chat, access::cannot_read_text(kind)).await;
                        return None;
                    }
                    text = access::unread_note(&text, kind);
                }
                Err(r) => {
                    self.tell(chat, files::refused_reply(&r)).await;
                    refused.push(r);
                }
            }
        }
        let text = files::with_notes(&text, &saved, &refused);
        if text.trim().is_empty() {
            return None;
        }
        Some(InboundMessage {
            channel: NAME.into(),
            chat_id: chat.to_string(),
            sender: chat.to_string(),
            sender_id: Some(chat.to_string()),
            message_id: guid.to_string(),
            text,
            attachments: saved
                .iter()
                .map(|s| Attachment {
                    kind: s.mime.clone(),
                    url: s.path.to_string_lossy().into_owned(),
                    name: Some(s.rel.clone()),
                })
                .collect(),
            reply_to: m["threadOriginatorGuid"]
                .as_str()
                .filter(|g| !g.is_empty())
                .map(str::to_string),
            ts: m["dateCreated"]
                .as_i64()
                .map_or_else(|| chrono::Utc::now().timestamp(), |ms| ms / 1000),
        })
    }

    /// One attachment, from the Mac into the inbox.
    async fn fetch(&self, att: &Value, id: &str) -> Result<Saved, Refused> {
        let name = att["transferName"]
            .as_str()
            .filter(|n| !n.is_empty())
            .unwrap_or("file")
            .to_string();
        let mime = att["mimeType"].as_str().filter(|m| !m.is_empty());
        let fail = |why: String| Refused {
            name: name.clone(),
            why,
        };
        let Some(inbox) = &self.cfg.inbox else {
            return Err(fail("files aren't saved on this gateway".into()));
        };
        if let Some(n) = att["totalBytes"]
            .as_u64()
            .filter(|n| *n > inbox.max_bytes())
        {
            return Err(inbox.too_big(&name, n));
        }
        let Some(att_guid) = att["guid"].as_str() else {
            return Err(fail("BlueBubbles didn't say where it is".into()));
        };
        let path = format!("/api/v1/attachment/{}/download", urlencode(att_guid));
        let resp = self
            .client
            .get(self.url(&path))
            .send()
            .await
            .map_err(|e| fail(format!("it couldn't be fetched ({})", e.without_url())))?;
        if !resp.status().is_success() {
            return Err(fail(format!(
                "it couldn't be fetched (BlueBubbles said {})",
                resp.status()
            )));
        }
        let bytes = files::read_capped(resp, inbox.max_bytes())
            .await
            .map_err(fail)?;
        inbox
            .save(NAME, id, &name, mime, &bytes)
            .map_err(|e| fail(format!("it couldn't be saved ({e})")))
    }
}

#[async_trait::async_trait]
impl Channel for BlueBubblesChannel {
    fn name(&self) -> &str {
        NAME
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            reactions: false,
            edits: false,
            attachments: true,
            buttons: false,
        }
    }

    fn polls(&self) -> bool {
        true
    }

    fn last_ok_poll(&self) -> Option<SystemTime> {
        *self.last_poll.lock().unwrap()
    }

    fn problem(&self) -> Option<String> {
        self.problem.lock().unwrap().clone()
    }

    fn message_limit(&self) -> Option<usize> {
        Some(MESSAGE_LIMIT)
    }

    async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        self.run_poll(tx).await
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        self.deliver(&msg).await.map(|_| ())
    }

    async fn post(&self, msg: OutboundMessage) -> Result<Option<String>, GatewayError> {
        self.deliver(&msg).await
    }

    async fn send_with_id(&self, msg: OutboundMessage) -> Result<Option<String>, GatewayError> {
        self.deliver(&msg).await
    }

    /// Only through the Private API.
    async fn typing(&self, chat_id: &str, _message_id: &str, on: bool) -> crate::Typing {
        if !self.cfg.private_api {
            return crate::Typing::Unsupported;
        }
        let path = format!(
            "/api/v1/chat/{}/typing",
            urlencode(&self.chat_guid(chat_id))
        );
        let req = if on {
            self.client.post(self.url(&path))
        } else {
            self.client.delete(self.url(&path))
        };
        match self.api(req, "typing").await {
            Ok(_) if on => crate::Typing::Shown {
                again_in: Duration::from_secs(10),
            },
            Ok(_) => crate::Typing::Unsupported,
            Err(e) => {
                tracing::debug!("{NAME}: typing: {e}");
                crate::Typing::Failed
            }
        }
    }
}
