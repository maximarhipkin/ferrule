//! M59: WhatsApp on a personal number, through WAHA (WhatsApp HTTP API,
//! <https://waha.devlike.pro>), a bridge the owner runs (a Docker
//! container) and pairs by scanning its QR code with WhatsApp → Linked
//! devices. Ferrule never speaks the WhatsApp Web protocol itself and never
//! sees the pairing: it is WAHA's REST API and its WebSocket to it.
//!
//! What comes in: WAHA's WebSocket (`/ws?session=…&events=message.any`),
//! which ferrule opens, so nothing listens on this machine. What goes out:
//! `POST /api/sendText`, `/api/sendImage` and `/api/sendFile`. The API key
//! (`X-Api-Key`) is WAHA's own; the socket takes it as its documented
//! `x-api-key` query parameter, so its address is never logged (the
//! socket's errors leave the URL out).
//!
//! Who gets in: a DM from an allow-listed number; an allow-listed group
//! when the account is @mentioned or replied to; and, when the account's
//! own number is allowed, the owner's "Message yourself" chat. Strangers
//! are never told anything: this is the owner's own WhatsApp. No buttons,
//! no edits, no reactions (a 👀 on a friend's message would come from the
//! owner).

use crate::channel::{Channel, ChannelCapabilities};
use crate::channels::access::{self, Access, Dm};
use crate::channels::files::{self, Inbox, Refused, Saved};
use crate::channels::slack::mrkdwn;
use crate::channels::ws::{self, Message};
use crate::error::GatewayError;
use crate::message::{Attachment, InboundMessage, OutboundMessage};
use crate::stream::chunks;
use base64::Engine;
use futures_util::SinkExt;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// The adapter's name, the session ids' prefix, `[gateway.whatsapp_personal]`.
pub const NAME: &str = "whatsapp_personal";
/// One message's text; a longer reply goes as several.
pub const MESSAGE_LIMIT: usize = 4096;
const REQUEST_DEADLINE: Duration = Duration::from_secs(60);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// How long what ferrule sent is remembered, so its echo in the owner's
/// own chat isn't taken for the owner writing.
const ECHO_MEMORY: Duration = Duration::from_secs(600);

/// Waits and retries, shortened in tests.
#[derive(Clone)]
struct Timing {
    backoff_min: Duration,
    backoff_max: Duration,
    /// How often an open socket is pinged; three silent pings end it.
    ping: Duration,
    /// A socket up this long started well: its failures count afresh.
    healthy: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff_min: Duration::from_secs(2),
            backoff_max: Duration::from_secs(60),
            ping: Duration::from_secs(30),
            healthy: Duration::from_secs(60),
        }
    }
}

/// Everything the adapter needs.
#[derive(Clone, Debug)]
pub struct WahaConfig {
    /// WAHA's base address, `http://127.0.0.1:3000`.
    pub url: String,
    /// The WAHA session, `default`.
    pub session: String,
    /// `X-Api-Key`; `None` when WAHA runs without one.
    pub api_key: Option<String>,
    /// Where files people send are saved; `None`: not saved.
    pub inbox: Option<Inbox>,
}

/// Something ferrule sent, remembered for the echo check.
struct Sent {
    at: std::time::Instant,
    chat: String,
    key: Option<String>,
    text: String,
}

pub struct WahaChannel {
    cfg: WahaConfig,
    base: String,
    client: reqwest::Client,
    access: Access,
    timing: Timing,
    /// The account's own id (`972…@c.us`), from the session.
    me: Mutex<Option<String>>,
    sent: Mutex<VecDeque<Sent>>,
    last_poll: Mutex<Option<SystemTime>>,
    problem: Mutex<Option<String>>,
}

/// `972501234567@c.us` → `972501234567`; a device part goes too. Groups
/// (`@g.us`) and hidden ids (`@lid`) stay whole.
pub fn short(id: &str) -> String {
    let id = id.trim();
    for suffix in ["@c.us", "@s.whatsapp.net"] {
        if let Some(n) = id.strip_suffix(suffix) {
            return n.split(':').next().unwrap_or(n).to_string();
        }
    }
    id.to_string()
}

/// An allow-list entry as a chat id: `+972 50-123-4567` → `972501234567`.
pub fn normalize(entry: &str) -> String {
    let e = entry.trim();
    if e.contains('@') {
        return short(e);
    }
    e.chars().filter(char::is_ascii_digit).collect()
}

/// A chat id as WAHA wants it: digits are a person (`…@c.us`).
pub fn full(chat: &str) -> String {
    let c = chat.trim().trim_start_matches('+');
    if !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()) {
        format!("{c}@c.us")
    } else {
        chat.trim().to_string()
    }
}

pub fn is_group(chat: &str) -> bool {
    chat.ends_with("@g.us")
}

/// A message id's key: `true_972…@c.us_3EB0…` (and a group's
/// `false_…@g.us_3EB0…_972…@c.us`) → `3EB0…`.
fn key_of(id: &str) -> String {
    let parts: Vec<&str> = id.split('_').collect();
    if parts.len() >= 3 && matches!(parts[0], "true" | "false") {
        parts[2].to_string()
    } else {
        id.to_string()
    }
}

/// A message id in any of WAHA's engines' shapes.
fn id_of(v: &Value) -> Option<String> {
    v.as_str()
        .or(v["_serialized"].as_str())
        .or(v["id"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The id of what a send made: `id` (a string or WEBJS's object) or
/// NOWEB's `key.id`.
fn sent_id(v: &Value) -> Option<String> {
    id_of(&v["id"]).or_else(|| id_of(&v["key"]["id"]))
}

/// A file name's extension for a MIME type, when WAHA gave no name
/// (photos and voice notes).
fn ext_for(mime: Option<&str>) -> &'static str {
    match mime.unwrap_or("").split(';').next().unwrap_or("") {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/webp" => ".webp",
        "audio/ogg" => ".ogg",
        "audio/mpeg" => ".mp3",
        "video/mp4" => ".mp4",
        "application/pdf" => ".pdf",
        _ => "",
    }
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

/// What the session said about itself.
#[derive(Debug, Clone)]
pub struct Probe {
    pub session: String,
    /// `WORKING`, `SCAN_QR_CODE`, `STARTING`, `FAILED`, `STOPPED`, …
    pub status: String,
    /// The account's id, once paired.
    pub me: Option<String>,
    pub name: Option<String>,
    /// WAHA's version and engine, when it says.
    pub version: Option<String>,
}

impl Probe {
    pub fn working(&self) -> bool {
        self.status == "WORKING"
    }

    /// "+972501234567 (Max) · session default WORKING · WAHA 2025.6 WEBJS".
    pub fn summary(&self) -> String {
        let who = match (&self.me, &self.name) {
            (Some(me), Some(n)) => format!("+{} ({n}) · ", short(me)),
            (Some(me), None) => format!("+{} · ", short(me)),
            _ => String::new(),
        };
        let version = self
            .version
            .as_ref()
            .map(|v| format!(" · WAHA {v}"))
            .unwrap_or_default();
        format!("{who}session {} {}{version}", self.session, self.status)
    }

    /// What to do when the session isn't working, in words.
    pub fn explain(&self) -> String {
        match self.status.as_str() {
            "SCAN_QR_CODE" => format!(
                "the WAHA session `{}` waits for its QR code: open WAHA's dashboard (or GET /api/{}/auth/qr) and scan it with WhatsApp → Linked devices",
                self.session, self.session
            ),
            "STOPPED" => format!(
                "the WAHA session `{}` is stopped: start it in WAHA's dashboard",
                self.session
            ),
            "STARTING" => format!("the WAHA session `{}` is still starting", self.session),
            other => format!(
                "the WAHA session `{}` is {other}: see WAHA's dashboard and its log",
                self.session
            ),
        }
    }
}

/// Asks WAHA about the session (and its version): nothing is sent.
pub async fn probe(cfg: WahaConfig) -> Result<Probe, String> {
    if !(cfg.url.starts_with("http://") || cfg.url.starts_with("https://")) {
        return Err(format!(
            "WAHA's address is a URL like http://127.0.0.1:3000, not {:?}",
            cfg.url
        ));
    }
    let ch = WahaChannel::new(cfg);
    ch.session().await
}

/// How one socket ended.
enum End {
    /// The gateway is shutting down.
    Closed,
    Lost(String),
}

impl WahaChannel {
    pub fn new(cfg: WahaConfig) -> Self {
        let base = cfg.url.trim_end_matches('/').to_string();
        Self {
            client: ws::http_client(&base, CONNECT_DEADLINE, REQUEST_DEADLINE),
            base,
            access: Self::access(vec![], vec![]),
            timing: Timing::default(),
            me: Mutex::new(None),
            sent: Mutex::new(VecDeque::new()),
            last_poll: Mutex::new(None),
            problem: Mutex::new(None),
            cfg,
        }
    }

    fn access(users: Vec<String>, groups: Vec<String>) -> Access {
        Access::new(NAME, "WhatsApp number", users, groups)
            .with_keys(
                "[gateway.whatsapp_personal] allowed_users",
                "[gateway.whatsapp_personal] allowed_groups",
            )
            // The owner's own WhatsApp: a stranger is never answered.
            .silent()
    }

    /// Who may DM it (numbers, `972…`), and the groups (`…@g.us`) where a
    /// mention reaches it.
    pub fn with_allowed(mut self, users: Vec<String>, groups: Vec<String>) -> Self {
        self.access = Self::access(
            users.iter().map(|u| normalize(u)).collect(),
            groups.iter().map(|g| g.trim().to_string()).collect(),
        );
        self
    }

    /// Tests: waits in milliseconds, not seconds.
    #[doc(hidden)]
    pub fn with_fast_retries(mut self) -> Self {
        self.timing = Timing {
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(100),
            ping: Duration::from_millis(200),
            healthy: Duration::from_millis(500),
        };
        self
    }

    fn keyed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.cfg.api_key {
            Some(k) => req.header("X-Api-Key", k),
            None => req,
        }
    }

    fn transport(&self, e: reqwest::Error) -> String {
        if e.is_connect() {
            format!("nothing answers at {}: is WAHA running?", self.base)
        } else {
            format!("WAHA at {} failed: {}", self.base, e.without_url())
        }
    }

    /// `GET /api/sessions/<session>` (and `/api/server/version`).
    async fn session(&self) -> Result<Probe, String> {
        let url = format!(
            "{}/api/sessions/{}",
            self.base,
            urlencode(&self.cfg.session)
        );
        let resp = self
            .keyed(self.client.get(&url))
            .send()
            .await
            .map_err(|e| self.transport(e))?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(format!(
                "WAHA refused the API key (status {status}): the key in [gateway.whatsapp_personal] api_key_env must be the one WAHA was started with (its WAHA_API_KEY)"
            ));
        }
        if status.as_u16() == 404 {
            return Err(format!(
                "WAHA has no session `{}`: start it in WAHA's dashboard, or set [gateway.whatsapp_personal] session",
                self.cfg.session
            ));
        }
        if !status.is_success() {
            return Err(format!("WAHA's session lookup said {status}"));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|_| format!("{} doesn't answer as WAHA", self.base))?;
        let Some(state) = v["status"].as_str() else {
            return Err(format!("{} doesn't answer as WAHA", self.base));
        };
        let me = v["me"]["id"].as_str().map(str::to_string);
        let name = v["me"]["pushName"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        let version = match self
            .keyed(self.client.get(format!("{}/api/server/version", self.base)))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r.json::<Value>().await.ok().and_then(|v| {
                let ver = v["version"].as_str()?;
                Some(match v["engine"].as_str() {
                    Some(e) => format!("{ver} {e}"),
                    None => ver.to_string(),
                })
            }),
            _ => None,
        };
        Ok(Probe {
            session: self.cfg.session.clone(),
            status: state.to_string(),
            me,
            name,
            version,
        })
    }

    /// One call, `POST /api/<endpoint>`: its JSON answer.
    async fn call(&self, endpoint: &str, body: Value) -> Result<Value, GatewayError> {
        let url = format!("{}/api/{endpoint}", self.base);
        let resp = self
            .keyed(self.client.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| GatewayError::Channel(self.transport(e)))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            return Ok(serde_json::from_str(&text).unwrap_or(Value::Null));
        }
        let said = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v["message"]
                    .as_str()
                    .or(v["error"].as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let why = match status.as_u16() {
            401 | 403 => {
                "WAHA refused the API key ([gateway.whatsapp_personal] api_key_env)".to_string()
            }
            429 => {
                return Err(GatewayError::RateLimited {
                    retry_after: Duration::from_secs(30),
                })
            }
            _ if said.contains("SCAN_QR_CODE") || said.contains("STOPPED") => format!(
                "the WAHA session `{}` isn't connected: scan its QR code in WAHA's dashboard",
                self.cfg.session
            ),
            _ => format!(
                "WAHA {endpoint} failed: status {status}{}",
                if said.is_empty() {
                    String::new()
                } else {
                    format!(": {}", crate::health::clip(&said, 300))
                }
            ),
        };
        Err(GatewayError::Channel(format!("{NAME}: {why}")))
    }

    fn remember(&self, chat: &str, id: Option<&str>, text: &str) {
        let mut sent = self.sent.lock().unwrap();
        while sent.front().is_some_and(|s| s.at.elapsed() > ECHO_MEMORY) || sent.len() > 200 {
            sent.pop_front();
        }
        sent.push_back(Sent {
            at: std::time::Instant::now(),
            chat: chat.to_string(),
            key: id.map(key_of),
            text: text.trim().to_string(),
        });
    }

    /// Whether a message from the account itself in `chat` is one ferrule
    /// sent (by its id, else by its text).
    fn is_echo(&self, chat: &str, id: &str, text: &str) -> bool {
        let key = key_of(id);
        let text = text.trim();
        self.sent.lock().unwrap().iter().any(|s| {
            s.at.elapsed() <= ECHO_MEMORY
                && s.chat == chat
                && (s.key.as_deref() == Some(key.as_str()) || s.text == text)
        })
    }

    /// Text and files: all of `msg`; the last message's id.
    async fn deliver(&self, msg: &OutboundMessage) -> Result<Option<String>, GatewayError> {
        let chat = full(&msg.chat_id);
        let text = mrkdwn::whatsapp(&msg.text);
        let mut reply = msg.reply_to.clone();
        let mut last = None;
        for piece in chunks(&text, MESSAGE_LIMIT)
            .into_iter()
            .filter(|p| !p.trim().is_empty())
        {
            let mut body = json!({
                "session": self.cfg.session,
                "chatId": chat,
                "text": piece,
                "linkPreview": false,
            });
            if let Some(r) = reply.take() {
                body["reply_to"] = json!(r);
            }
            let v = self.call("sendText", body).await?;
            let id = sent_id(&v);
            self.remember(&chat, id.as_deref(), &piece);
            last = id.or(last);
        }
        for att in &msg.attachments {
            let (name, mime, bytes) = files::read_outgoing(att)?;
            let mime = if att.kind.contains('/') {
                att.kind.clone()
            } else {
                mime.to_string()
            };
            let endpoint = if matches!(mime.as_str(), "image/jpeg" | "image/png") {
                "sendImage"
            } else {
                "sendFile"
            };
            let body = json!({
                "session": self.cfg.session,
                "chatId": chat,
                "file": {
                    "mimetype": mime,
                    "filename": files::safe_name(&name),
                    "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                },
            });
            let v = self.call(endpoint, body).await?;
            let id = sent_id(&v);
            self.remember(&chat, id.as_deref(), "");
            last = id.or(last);
        }
        Ok(last)
    }

    /// A short answer from the adapter itself (a refused file).
    async fn tell(&self, chat: &str, text: String) {
        let chat = full(chat);
        let body = json!({ "session": self.cfg.session, "chatId": chat, "text": text, "linkPreview": false });
        match self.call("sendText", body).await {
            Ok(v) => self.remember(&chat, sent_id(&v).as_deref(), &text),
            Err(e) => tracing::warn!("{NAME}: couldn't answer {chat}: {e}"),
        }
    }

    fn set_problem(&self, p: Option<String>) {
        *self.problem.lock().unwrap() = p;
    }

    fn polled(&self) {
        *self.last_poll.lock().unwrap() = Some(SystemTime::now());
    }

    /// The socket's address. It holds the key, so it is never logged.
    fn socket_url(&self) -> String {
        let ws_base = if let Some(rest) = self.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else {
            format!(
                "ws://{}",
                self.base.strip_prefix("http://").unwrap_or(&self.base)
            )
        };
        let mut url = format!(
            "{ws_base}/ws?session={}&events=message.any&events=session.status",
            urlencode(&self.cfg.session)
        );
        if let Some(k) = &self.cfg.api_key {
            url.push_str(&format!("&x-api-key={}", urlencode(k)));
        }
        url
    }

    async fn run_socket(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        let mut backoff = self.timing.backoff_min;
        loop {
            if tx.is_closed() {
                return Ok(());
            }
            let started = Instant::now();
            match self.connection(&tx).await {
                End::Closed => return Ok(()),
                End::Lost(why) => {
                    tracing::warn!("{NAME}: {why}");
                    self.set_problem(Some(why));
                }
            }
            if started.elapsed() >= self.timing.healthy {
                backoff = self.timing.backoff_min;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(self.timing.backoff_max);
        }
    }

    /// One socket, from the session check to its end.
    async fn connection(&self, tx: &mpsc::Sender<InboundMessage>) -> End {
        let p = match self.session().await {
            Ok(p) => p,
            Err(e) => return End::Lost(e),
        };
        if !p.working() {
            return End::Lost(p.explain());
        }
        *self.me.lock().unwrap() = p.me.clone();
        let mut socket = match ws::connect(&self.socket_url(), CONNECT_DEADLINE).await {
            Ok(s) => s,
            Err(e) => return End::Lost(format!("couldn't open WAHA's socket ({e})")),
        };
        self.polled();
        self.set_problem(None);
        tracing::info!("{NAME}: listening to WAHA ({})", p.summary());
        let mut next_ping = Instant::now() + self.timing.ping;
        let mut heard = Instant::now();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(next_ping) => {
                    if heard.elapsed() > self.timing.ping * 3 {
                        return End::Lost("WAHA's socket went quiet; reconnecting".into());
                    }
                    next_ping = Instant::now() + self.timing.ping;
                    if let Err(e) = socket.send(Message::Ping(Vec::new().into())).await {
                        return End::Lost(format!("WAHA's socket failed: {e}"));
                    }
                }
                frame = ws::next(&mut socket) => {
                    heard = Instant::now();
                    self.polled();
                    match frame {
                        None | Some(Ok(Message::Close(_))) => {
                            return End::Lost("WAHA closed its socket".into())
                        }
                        Some(Err(e)) => return End::Lost(format!("WAHA's {e}")),
                        Some(Ok(Message::Text(t))) => {
                            let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else {
                                continue;
                            };
                            if let Some(m) = self.on_event(&v).await {
                                if tx.send(m).await.is_err() {
                                    return End::Closed;
                                }
                            }
                        }
                        Some(Ok(_)) => {}
                    }
                }
            }
        }
    }

    /// One event: a message for the agent, or nothing.
    pub(crate) async fn on_event(&self, v: &Value) -> Option<InboundMessage> {
        if v["session"].as_str().is_some_and(|s| s != self.cfg.session) {
            return None;
        }
        match v["event"].as_str()? {
            "session.status" => {
                let status = v["payload"]["status"].as_str().unwrap_or("");
                if status != "WORKING" && !status.is_empty() {
                    let p = Probe {
                        session: self.cfg.session.clone(),
                        status: status.to_string(),
                        me: None,
                        name: None,
                        version: None,
                    };
                    tracing::warn!("{NAME}: {}", p.explain());
                    self.set_problem(Some(p.explain()));
                } else if status == "WORKING" {
                    self.set_problem(None);
                }
                None
            }
            "message" | "message.any" => self.message(&v["payload"]).await,
            _ => None,
        }
    }

    /// A message's payload, when it is for the agent and allowed.
    async fn message(&self, p: &Value) -> Option<InboundMessage> {
        let id = id_of(&p["id"])?;
        let from_me = p["fromMe"].as_bool().unwrap_or(false);
        let chat = if from_me { &p["to"] } else { &p["from"] }.as_str()?;
        if chat.ends_with("@broadcast") || chat.ends_with("@newsletter") {
            return None;
        }
        let body = p["body"].as_str().unwrap_or("").to_string();
        let me = self.me.lock().unwrap().clone();
        let me_short = me.as_deref().map(short);

        if from_me {
            // The owner's "Message yourself" chat, when their number is
            // allowed; never what ferrule itself sent there.
            let mine = me_short?;
            if is_group(chat)
                || short(chat) != mine
                || !self.access.user_allowed(&mine)
                || self.is_echo(&full(&mine), &id, &body)
            {
                return None;
            }
            return self.data(p, &mine, &mine, "you", &id, body).await;
        }

        let name = p["_data"]["notifyName"]
            .as_str()
            .or(p["_data"]["pushName"].as_str())
            .filter(|n| !n.is_empty())
            .map(str::to_string);

        if is_group(chat) {
            let sender = p["participant"]
                .as_str()
                .or(p["author"].as_str())
                .or(p["_data"]["author"].as_str())
                .or(p["_data"]["key"]["participant"].as_str())
                .map(short)?;
            let name = name.unwrap_or_else(|| sender.clone());
            let mention = me_short.as_ref().map(|m| format!("@{m}"));
            let mentioned = mention.as_ref().is_some_and(|m| body.contains(m.as_str()));
            let replied = me_short.as_ref().is_some_and(|m| {
                p["replyTo"]["participant"].as_str().map(short).as_deref() == Some(m.as_str())
            });
            if !self.access.channel_allowed(chat) {
                if mentioned || replied {
                    self.access.ignore(chat, &name, "group");
                }
                return None;
            }
            if !(mentioned || replied) {
                return None;
            }
            let text = match &mention {
                Some(m) => access::strip_leading(&body.replacen(m.as_str(), "", 1), &[]),
                None => body,
            };
            return self.data(p, chat, &sender, &name, &id, text).await;
        }

        let from = short(chat);
        let name = name.unwrap_or_else(|| from.clone());
        match self.access.dm(&from, &name, &body) {
            Dm::Admit => {}
            Dm::Tell(t) | Dm::Paired(t) => {
                self.tell(&from, t).await;
                return None;
            }
            Dm::Drop => return None,
        }
        self.data(p, &from, &from, &name, &id, body).await
    }

    /// A message's text and file, for `chat`.
    async fn data(
        &self,
        p: &Value,
        chat: &str,
        sender: &str,
        name: &str,
        id: &str,
        mut text: String,
    ) -> Option<InboundMessage> {
        let mut saved: Vec<Saved> = vec![];
        let mut refused: Vec<Refused> = vec![];
        if p["hasMedia"].as_bool().unwrap_or(false) {
            let mime = p["media"]["mimetype"].as_str();
            match self.fetch(p, id).await {
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
            sender: name.to_string(),
            sender_id: Some(sender.to_string()),
            message_id: id.to_string(),
            text,
            attachments: saved
                .iter()
                .map(|s| Attachment {
                    kind: s.mime.clone(),
                    url: s.path.to_string_lossy().into_owned(),
                    name: Some(s.rel.clone()),
                })
                .collect(),
            reply_to: id_of(&p["replyTo"]["id"]),
            ts: p["timestamp"]
                .as_i64()
                .unwrap_or_else(|| chrono::Utc::now().timestamp()),
        })
    }

    /// WAHA's address for a file it saved, only on WAHA itself: the key
    /// never goes anywhere else.
    fn own_url(&self, url: &str) -> Option<String> {
        if url.starts_with(&format!("{}/", self.base)) {
            return Some(url.to_string());
        }
        let rest = url.split_once("://")?.1;
        let path = &rest[rest.find('/')?..];
        path.starts_with("/api/files/")
            .then(|| format!("{}{path}", self.base))
    }

    /// The message's file, from WAHA into the inbox.
    async fn fetch(&self, p: &Value, id: &str) -> Result<Saved, Refused> {
        let media = &p["media"];
        let mime = media["mimetype"].as_str().filter(|m| !m.is_empty());
        let name = media["filename"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("file{}", ext_for(mime)));
        let fail = |why: String| Refused {
            name: name.clone(),
            why,
        };
        let Some(inbox) = &self.cfg.inbox else {
            return Err(fail("files aren't saved on this gateway".into()));
        };
        if !media["error"].is_null() {
            return Err(fail("WAHA couldn't download it".into()));
        }
        let Some(url) = media["url"].as_str() else {
            return Err(fail(
                "WAHA didn't download it (its WAHA_EVENTS_DOWNLOAD_MEDIA setting)".into(),
            ));
        };
        let Some(url) = self.own_url(url) else {
            return Err(fail("WAHA named a file outside its own address".into()));
        };
        let resp = self
            .keyed(self.client.get(&url))
            .send()
            .await
            .map_err(|e| fail(format!("it couldn't be fetched ({})", e.without_url())))?;
        if !resp.status().is_success() {
            return Err(fail(format!(
                "it couldn't be fetched (WAHA said {})",
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
impl Channel for WahaChannel {
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
        self.run_socket(tx).await
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

    async fn typing(&self, chat_id: &str, _message_id: &str, on: bool) -> crate::Typing {
        let endpoint = if on { "startTyping" } else { "stopTyping" };
        let body = json!({ "session": self.cfg.session, "chatId": full(chat_id) });
        match self.call(endpoint, body).await {
            Ok(_) if on => crate::Typing::Shown {
                again_in: Duration::from_secs(20),
            },
            Ok(_) => crate::Typing::Unsupported,
            Err(GatewayError::RateLimited { .. }) => crate::Typing::Limited,
            Err(e) => {
                tracing::debug!("{NAME}: typing: {e}");
                crate::Typing::Failed
            }
        }
    }
}
