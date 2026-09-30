//! The HTTP API (M39 §8): for programs rather than people. A listener on
//! `127.0.0.1:<port>` unless told otherwise (a quick tunnel to it when `public = "tunnel"`),
//! keys per client (`clients`), and three routes (`server`):
//!
//! - `POST /v1/messages` — a message in; the answer back, as one JSON
//!   response or as server-sent events (`delta`, `message`, `done`).
//! - `GET /v1/events?after=<n>&wait=<s>` — the client's outbox: what
//!   ferrule sent that no request was waiting for (approval asks, notices,
//!   task results), the last 200, kept in `state.json` across restarts.
//! - `GET /v1/files/<token>` — a file the agent sent, for an hour.
//!
//! A client's chat is its name, or `<name>/<conversation>`. The router
//! answers with `reply_to` = the request's id, which is how an answer finds
//! its request; [`Channel::answered`] closes it. What arrives for a request
//! nobody waits on any more goes to the outbox instead, never lost. Every
//! outbox message is also POSTed to the client's webhook, if it has one,
//! signed with its webhook secret.

pub mod clients;
mod server;

use crate::channel::{Button, ButtonAction, Channel, ChannelCapabilities};
use crate::channels::files::Inbox;
use crate::channels::hmac;
use crate::error::GatewayError;
use crate::message::{Attachment, InboundMessage, OutboundMessage};
use clients::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, Notify};

/// The gateway's own file: the outboxes and when each client was last seen.
pub const STATE: &str = "state.json";
/// Outbox messages kept per client.
const OUTBOX: usize = 200;
/// How long a file link works.
const FILE_TTL: Duration = Duration::from_secs(3600);
/// Request bodies.
pub const MAX_BODY: usize = 64 * 1024;

/// Everything the adapter needs.
#[derive(Clone)]
pub struct HttpConfig {
    /// `<data>/gateway/http`: `clients.json` and `state.json`.
    pub dir: PathBuf,
    /// The address it listens on: 127.0.0.1, or 0.0.0.0 in a container.
    pub bind: std::net::IpAddr,
    /// 0: any free port (tests).
    pub port: u16,
    pub requests_per_minute: u32,
    /// cloudflared, when `public = "tunnel"`.
    pub tunnel: Option<PathBuf>,
    /// Where files clients send are saved; `None`: none are taken.
    pub inbox: Option<Inbox>,
}

/// Waits, shortened in tests.
#[derive(Clone)]
struct Timing {
    /// Before each webhook try.
    webhook: Vec<Duration>,
    /// An SSE comment this often, so a proxy keeps the stream open.
    keepalive: Duration,
    /// The longest a plain request waits for its answer.
    answer: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            webhook: vec![
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(5),
                Duration::from_secs(30),
            ],
            keepalive: Duration::from_secs(15),
            answer: Duration::from_secs(30 * 60),
        }
    }
}

/// One outbox message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub n: u64,
    pub id: String,
    #[serde(default)]
    pub conversation: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Value>,
    /// The request it answers, when the answer came after the request
    /// stopped waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub ts: i64,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct State {
    #[serde(default)]
    pub(crate) last_used: HashMap<String, i64>,
    #[serde(default)]
    outbox: HashMap<String, VecDeque<Event>>,
    /// The last `n` given per client.
    #[serde(default)]
    next: HashMap<String, u64>,
}

/// What a waiting request hears.
enum Ev {
    /// The answer so far.
    Delta(String),
    /// Something else ferrule sent the chat meanwhile (an approval ask).
    Message(Value),
    /// The whole answer.
    Done(Value),
}

/// A request waiting for its answer.
struct Pending {
    chat: String,
    conversation: Option<String>,
    /// The answer's messages, by the order they were posted.
    parts: Vec<(String, String)>,
    files: Vec<Value>,
    choices: Vec<Value>,
    tx: mpsc::UnboundedSender<Ev>,
}

impl Pending {
    fn text(&self) -> String {
        self.parts
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A file link.
struct FileLink {
    client: String,
    path: PathBuf,
    name: String,
    expires: Instant,
}

/// The loaded clients and the file's stamp when they were read.
#[derive(Default)]
struct Loaded {
    clients: Vec<Client>,
    stamp: Option<(SystemTime, u64)>,
}

struct Shared {
    cfg: HttpConfig,
    timing: Timing,
    loaded: Mutex<Loaded>,
    pending: Mutex<HashMap<String, Pending>>,
    /// A posted message's id → the request it's part of.
    posted: Mutex<HashMap<String, String>>,
    state: Mutex<State>,
    /// When `state.json` was last written for `last_used` alone.
    saved_seen: Mutex<Option<Instant>>,
    outbox_grew: Notify,
    files: Mutex<HashMap<String, FileLink>>,
    rate: Mutex<HashMap<String, VecDeque<Instant>>>,
    port: AtomicU16,
    seq: AtomicU64,
    /// Ids this process makes start with it, so they don't repeat after a
    /// restart.
    run: String,
    tunnel_url: Mutex<Option<String>>,
    /// By source: `clients`, `tunnel`, `webhook:<name>`.
    problems: Arc<Mutex<HashMap<String, String>>>,
    http: reqwest::Client,
}

pub struct HttpChannel {
    shared: Arc<Shared>,
}

impl HttpChannel {
    pub fn new(cfg: HttpConfig) -> Self {
        let state = std::fs::read_to_string(cfg.dir.join(STATE))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let run: [u8; 4] = ferrule_connections::seal::random();
        Self {
            shared: Arc::new(Shared {
                cfg,
                timing: Timing::default(),
                loaded: Mutex::new(Loaded::default()),
                pending: Mutex::new(HashMap::new()),
                posted: Mutex::new(HashMap::new()),
                state: Mutex::new(state),
                saved_seen: Mutex::new(None),
                outbox_grew: Notify::new(),
                files: Mutex::new(HashMap::new()),
                rate: Mutex::new(HashMap::new()),
                port: AtomicU16::new(0),
                seq: AtomicU64::new(0),
                run: hmac::hex(&run),
                tunnel_url: Mutex::new(None),
                problems: Arc::new(Mutex::new(HashMap::new())),
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap_or_default(),
            }),
        }
    }

    /// Tests: the webhook's waits before each try, and the SSE keepalive.
    #[doc(hidden)]
    pub fn with_timing(mut self, webhook: Vec<Duration>, keepalive: Duration) -> Self {
        if let Some(s) = Arc::get_mut(&mut self.shared) {
            s.timing.webhook = webhook;
            s.timing.keepalive = keepalive;
        }
        self
    }

    /// The port it listens on, once `run` has bound it.
    pub fn port(&self) -> u16 {
        self.shared.port.load(Ordering::Relaxed)
    }

    /// The quick tunnel's address, while one is open.
    pub fn public_url(&self) -> Option<String> {
        self.shared.tunnel_url.lock().unwrap().clone()
    }
}

/// `<client>` or `<client>/<conversation>`.
fn split_chat(chat: &str) -> (&str, Option<&str>) {
    match chat.split_once('/') {
        Some((c, conv)) => (c, Some(conv)),
        None => (chat, None),
    }
}

/// A conversation id: 1–64 of letters, digits and `._:-`.
fn valid_conversation(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 64
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_:".contains(&b))
}

impl Shared {
    fn next_id(&self, prefix: &str) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{prefix}-{}-{n}", self.run)
    }

    fn set_problem(&self, source: &str, problem: Option<String>) {
        let mut p = self.problems.lock().unwrap();
        match problem {
            Some(why) => {
                p.insert(source.to_string(), why);
            }
            None => {
                p.remove(source);
            }
        }
    }

    /// The clients, re-read when `clients.json` changed. A file that can't
    /// be read lets no one in, and says so.
    fn clients(&self) -> Vec<Client> {
        let path = clients::path(&self.cfg.dir);
        let stamp = std::fs::metadata(&path)
            .ok()
            .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        let mut l = self.loaded.lock().unwrap();
        if l.stamp != stamp || stamp.is_none() {
            match clients::load(&self.cfg.dir) {
                Ok(c) => {
                    l.clients = c;
                    self.set_problem("clients", None);
                }
                Err(e) => {
                    l.clients.clear();
                    self.set_problem("clients", Some(e));
                }
            }
            l.stamp = stamp;
        }
        l.clients.clone()
    }

    /// Counts a request against the client's minute; `Some(secs)` to wait
    /// when it's over.
    fn over_rate(&self, client: &str) -> Option<u64> {
        let per_minute = self.cfg.requests_per_minute.max(1) as usize;
        let now = Instant::now();
        let mut rate = self.rate.lock().unwrap();
        let q = rate.entry(client.to_string()).or_default();
        while q
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            q.pop_front();
        }
        if q.len() >= per_minute {
            let wait = Duration::from_secs(60).saturating_sub(now.duration_since(q[0]));
            return Some(wait.as_secs().max(1));
        }
        q.push_back(now);
        None
    }

    /// Notes a client was seen; `state.json` is written at most once a
    /// minute for this alone.
    fn seen(&self, client: &str) {
        self.state
            .lock()
            .unwrap()
            .last_used
            .insert(client.to_string(), clients::now());
        let mut last = self.saved_seen.lock().unwrap();
        if last.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
            *last = Some(Instant::now());
            drop(last);
            self.save_state();
        }
    }

    fn save_state(&self) {
        let body = {
            let st = self.state.lock().unwrap();
            serde_json::to_vec_pretty(&*st)
        };
        if let Ok(body) = body {
            if let Err(e) = clients::write_private(&self.cfg.dir, STATE, &body) {
                tracing::warn!(error = %e, "http api: couldn't save state.json");
            }
        }
    }

    /// Links for the files a message carries, each usable by `client`
    /// for an hour.
    fn links(&self, client: &str, atts: &[Attachment]) -> Vec<Value> {
        let mut files = self.files.lock().unwrap();
        let now = Instant::now();
        files.retain(|_, f| f.expires > now);
        atts.iter()
            .map(|a| {
                let path = PathBuf::from(&a.url);
                let name = a.name.clone().unwrap_or_else(|| {
                    path.file_name()
                        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned())
                });
                let token: [u8; 24] = ferrule_connections::seal::random();
                let token = hmac::b64url(&token);
                files.insert(
                    token.clone(),
                    FileLink {
                        client: client.to_string(),
                        path,
                        name: name.clone(),
                        expires: now + FILE_TTL,
                    },
                );
                json!({ "name": name, "url": format!("/v1/files/{token}") })
            })
            .collect()
    }

    /// A message for the chat: part of a waiting request's answer when it
    /// answers one, else an outbox message. Its id.
    fn deliver(&self, msg: &OutboundMessage, buttons: &[Button]) -> String {
        let (client, conversation) = split_chat(&msg.chat_id);
        let id = self.next_id("o");
        let mut text = msg.text.clone();
        let mut choices = vec![];
        for b in buttons {
            match &b.action {
                ButtonAction::Command(reply) => {
                    choices.push(json!({ "label": b.text, "reply": reply }))
                }
                ButtonAction::Url(url) => text.push_str(&format!("\n• {}: {url}", b.text)),
            }
        }
        let files = self.links(client, &msg.attachments);
        {
            let mut pending = self.pending.lock().unwrap();
            if let Some(p) = msg.reply_to.as_ref().and_then(|r| pending.get_mut(r)) {
                p.parts.push((id.clone(), text));
                p.files.extend(files);
                p.choices.extend(choices);
                let _ = p.tx.send(Ev::Delta(p.text()));
                self.posted
                    .lock()
                    .unwrap()
                    .insert(id.clone(), msg.reply_to.clone().unwrap_or_default());
                return id;
            }
        }
        let ev = Event {
            n: 0,
            id: id.clone(),
            conversation: conversation.map(str::to_string),
            text,
            files,
            choices,
            reply_to: None,
            ts: clients::now(),
        };
        self.to_outbox(client, ev, Some(&msg.chat_id));
        id
    }

    /// Adds to `client`'s outbox, tells a request streaming in `chat`, and
    /// sends it to the webhook.
    fn to_outbox(&self, client: &str, mut ev: Event, chat: Option<&str>) {
        {
            let mut st = self.state.lock().unwrap();
            let n = st.next.entry(client.to_string()).or_insert(0);
            *n += 1;
            ev.n = *n;
            let q = st.outbox.entry(client.to_string()).or_default();
            q.push_back(ev.clone());
            while q.len() > OUTBOX {
                q.pop_front();
            }
        }
        self.save_state();
        self.outbox_grew.notify_waiters();
        if let Some(chat) = chat {
            let value = serde_json::to_value(&ev).unwrap_or(Value::Null);
            for p in self.pending.lock().unwrap().values() {
                if p.chat == chat {
                    let _ = p.tx.send(Ev::Message(value.clone()));
                }
            }
        }
        self.webhook(client, ev);
    }

    /// The outbox after `after`.
    fn events(&self, client: &str, after: u64) -> (Vec<Event>, u64) {
        let st = self.state.lock().unwrap();
        let last = st.next.get(client).copied().unwrap_or(0);
        let evs = st
            .outbox
            .get(client)
            .map(|q| q.iter().filter(|e| e.n > after).cloned().collect())
            .unwrap_or_default();
        (evs, last)
    }

    /// Closes a request: its answer to whoever waits, else to the outbox.
    fn answered(&self, message_id: &str) {
        let Some(p) = self.pending.lock().unwrap().remove(message_id) else {
            return;
        };
        self.posted.lock().unwrap().retain(|_, r| r != message_id);
        let mut done = json!({
            "id": message_id,
            "conversation": p.conversation,
            "text": p.text(),
            "files": p.files,
        });
        if !p.choices.is_empty() {
            done["choices"] = Value::Array(p.choices.clone());
        }
        if p.tx.send(Ev::Done(done)).is_ok() {
            return;
        }
        let (client, _) = split_chat(&p.chat);
        let ev = Event {
            n: 0,
            id: self.next_id("o"),
            conversation: p.conversation.clone(),
            text: p.text(),
            files: p.files.clone(),
            choices: p.choices.clone(),
            reply_to: Some(message_id.to_string()),
            ts: clients::now(),
        };
        self.to_outbox(client, ev, None);
    }

    /// POSTs an outbox message to the client's webhook, signed, retrying;
    /// a delivery that never lands is a problem until one does.
    fn webhook(&self, client: &str, ev: Event) {
        let Some(c) = self.clients().into_iter().find(|c| c.name == client) else {
            return;
        };
        let (Some(url), Some(secret)) = (c.webhook, c.webhook_secret) else {
            return;
        };
        let mut body = serde_json::to_value(&ev).unwrap_or(Value::Null);
        body["client"] = json!(client);
        let body = serde_json::to_vec(&body).unwrap_or_default();
        let sig = hmac::sign(secret.as_bytes(), &body);
        let (http, waits) = (self.http.clone(), self.timing.webhook.clone());
        let source = format!("webhook:{client}");
        let problems = self.problems.clone();
        let client = client.to_string();
        tokio::spawn(async move {
            let mut why = String::new();
            for wait in waits {
                tokio::time::sleep(wait).await;
                let sent = http
                    .post(&url)
                    .header("content-type", "application/json")
                    .header("x-ferrule-signature", &sig)
                    .header("user-agent", "ferrule")
                    .body(body.clone())
                    .send()
                    .await;
                match sent {
                    Ok(r) if r.status().is_success() => {
                        problems.lock().unwrap().remove(&source);
                        return;
                    }
                    Ok(r) => why = format!("it answered {}", r.status().as_u16()),
                    Err(e) => {
                        why = if e.is_timeout() {
                            "it didn't answer within 10 s".into()
                        } else {
                            "it couldn't be reached".into()
                        }
                    }
                }
            }
            tracing::warn!(client = %client, "http api: a webhook delivery failed: {why}");
            problems.lock().unwrap().insert(
                source,
                format!(
                    "the webhook of `{client}` failed ({why}); the message stays in its outbox"
                ),
            );
        });
    }
}

#[async_trait::async_trait]
impl Channel for HttpChannel {
    fn name(&self) -> &str {
        "http"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            reactions: false,
            edits: true,
            attachments: true,
            buttons: true,
        }
    }

    async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        server::run(self.shared.clone(), tx).await
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        self.shared.deliver(&msg, &[]);
        Ok(())
    }

    async fn post(&self, msg: OutboundMessage) -> Result<Option<String>, GatewayError> {
        Ok(Some(self.shared.deliver(&msg, &[])))
    }

    async fn send_buttons(
        &self,
        msg: OutboundMessage,
        buttons: &[Button],
    ) -> Result<(), GatewayError> {
        self.shared.deliver(&msg, buttons);
        Ok(())
    }

    async fn edit(&self, _chat_id: &str, message_id: &str, text: &str) -> Result<(), GatewayError> {
        let Some(request) = self.shared.posted.lock().unwrap().get(message_id).cloned() else {
            // A message the outbox already holds stays as it was sent.
            return Ok(());
        };
        let mut pending = self.shared.pending.lock().unwrap();
        if let Some(p) = pending.get_mut(&request) {
            if let Some(part) = p.parts.iter_mut().find(|(id, _)| id == message_id) {
                part.1 = text.to_string();
            }
            let _ = p.tx.send(Ev::Delta(p.text()));
        }
        Ok(())
    }

    async fn answered(&self, _chat_id: &str, message_id: &str) {
        self.shared.answered(message_id);
    }

    fn busy_notices(&self) -> bool {
        false
    }

    fn problem(&self) -> Option<String> {
        let p = self.shared.problems.lock().unwrap();
        if p.is_empty() {
            return None;
        }
        let mut all: Vec<&String> = p.values().collect();
        all.sort();
        Some(
            all.into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    fn note(&self) -> Option<String> {
        let port = self.port();
        let ip = self.shared.cfg.bind;
        (port != 0).then(|| match self.public_url() {
            Some(url) => format!("listening on {ip}:{port}, public at {url}"),
            None => format!("listening on {ip}:{port}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chats_and_conversations() {
        assert_eq!(split_chat("ci"), ("ci", None));
        assert_eq!(split_chat("ci/build-7/x"), ("ci", Some("build-7/x")));
        assert!(valid_conversation("build-7.a:b_c"));
        assert!(!valid_conversation(""));
        assert!(!valid_conversation("a/b"));
        assert!(!valid_conversation(&"x".repeat(65)));
    }
}
