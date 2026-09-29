//! WhatsApp adapter (M39 §3): Meta's Cloud API, and nothing unofficial.
//! What goes out is a Graph API call with the business number's token;
//! what comes in is Meta's webhook, taken either from the relay Worker's
//! mailbox (polled every 3 s with the relay key) or from a listener on
//! `127.0.0.1:<port>` behind the owner's own tunnel. Every webhook body's
//! `X-Hub-Signature-256` is checked here with the app secret, whichever
//! way it came.
//!
//! Who gets in: DMs only (a Cloud API number can't join groups), from an
//! allow-listed `wa_id`. A stranger is told nothing: a reply would open a
//! conversation Meta bills.
//!
//! The 24-hour window (`window`): a free-form message goes only within a
//! day of the chat's last message. Outside it the message is held, and a
//! template (if one is set) tells the person there's something waiting;
//! without one `send` says so as an error. Nothing is dropped silently.

mod inbound;
pub mod window;

pub use inbound::{configure_mailbox, mailbox_box, parse};

use crate::channel::{buttons_as_text, Button, ButtonAction, Channel, ChannelCapabilities};
use crate::channels::access::Access;
use crate::channels::files::{self, Inbox};
use crate::channels::slack::mrkdwn;
use crate::error::GatewayError;
use crate::message::{Attachment, InboundMessage, OutboundMessage};
use crate::stream::chunks;
use crate::typing::Typing;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tokio::time::Instant;
use window::{Window, Windows};

/// Meta's Graph API.
pub const API_URL: &str = "https://graph.facebook.com";
pub const API_VERSION: &str = "v23.0";
/// A text message's limit.
pub const MESSAGE_LIMIT: usize = 4096;
/// An interactive message's body.
const BODY_LIMIT: usize = 1024;
const BUTTON_TITLE: usize = 20;
const BUTTON_ID: usize = 256;
const CAPTION_LIMIT: usize = 1024;
/// A template's `{{1}}`: one line, this long at most.
const TEMPLATE_PARAM: usize = 200;
/// Meta's own caps on media, checked before an upload.
const IMAGE_MAX: u64 = 5 * 1024 * 1024;
const AV_MAX: u64 = 16 * 1024 * 1024;
const DOCUMENT_MAX: u64 = 100 * 1024 * 1024;

const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// How many message ids are remembered to drop Meta's redeliveries, and
/// sent ids to find a failed one's text.
const SEEN: usize = 2000;
const SENT: usize = 200;

/// Waits and retries, shortened in tests.
#[derive(Clone)]
struct Timing {
    /// The mailbox is polled this often.
    poll: Duration,
    backoff_min: Duration,
    backoff_max: Duration,
    /// Messages to one chat are at least this far apart.
    post_gap: Duration,
    /// 130429's waits: 2 s, 4 s, 8 s.
    throughput: Duration,
    /// 131056's wait.
    pair: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(3),
            backoff_min: Duration::from_secs(2),
            backoff_max: Duration::from_secs(60),
            post_gap: Duration::from_secs(1),
            throughput: Duration::from_secs(2),
            pair: Duration::from_secs(6),
        }
    }
}

/// Where webhooks come from.
#[derive(Clone, Debug)]
pub enum Inbound {
    /// The relay Worker's mailbox.
    Relay { url: String, key: String },
    /// A listener on 127.0.0.1 (the owner's tunnel points at it).
    Listen { port: u16 },
    /// Nothing comes in (`ferrule tasks run-now` only sends).
    None,
}

/// A template for a closed window: its name, language, and whether its
/// body takes the `{{1}}` parameter.
#[derive(Clone, Debug)]
pub struct Template {
    pub name: String,
    pub language: String,
}

/// Everything the adapter needs.
#[derive(Clone, Debug)]
pub struct WhatsAppConfig {
    pub phone_number_id: String,
    pub token: String,
    pub app_secret: String,
    pub verify_token: String,
    /// `https://graph.facebook.com` (or a mock).
    pub api_url: String,
    pub api_version: String,
    pub inbound: Inbound,
    pub template: Option<Template>,
    /// `<data>/gateway/whatsapp`: windows and held messages.
    pub state_dir: Option<PathBuf>,
    /// Where files people send are saved; `None`: not saved.
    pub inbox: Option<Inbox>,
}

/// A Graph API error, with Meta's code.
#[derive(Debug)]
struct GraphError {
    code: i64,
    message: String,
}

pub struct WhatsAppChannel {
    cfg: WhatsAppConfig,
    /// `<api_url>/<version>`.
    graph: String,
    client: reqwest::Client,
    access: Access,
    timing: Timing,
    windows: Windows,
    /// Chat → when its last message went (or is booked to go).
    posted: Mutex<HashMap<String, Instant>>,
    seen: Mutex<(VecDeque<String>, HashSet<String>)>,
    /// Sent message id → (chat, text), to hold one Meta later fails with
    /// 131047.
    sent: Mutex<VecDeque<(String, String, String)>>,
    last_poll: Mutex<Option<SystemTime>>,
    /// The token was refused (190).
    token_refused: Mutex<Option<String>>,
    /// Inbound trouble: the mailbox or the listener.
    inbound_problem: Mutex<Option<String>>,
    /// A delivery failure Meta reported, and when (shown for an hour).
    failure: Mutex<Option<(String, Instant)>>,
    /// Codes already put in `problem` this hour.
    failures_said: Mutex<HashMap<i64, Instant>>,
}

impl WhatsAppChannel {
    pub fn new(cfg: WhatsAppConfig) -> Self {
        let api = cfg.api_url.trim_end_matches('/').to_string();
        let graph = format!("{api}/{}", cfg.api_version.trim_matches('/'));
        Self {
            client: crate::channels::ws::http_client(&api, CONNECT_DEADLINE, REQUEST_DEADLINE),
            graph,
            access: Access::new("whatsapp", "WhatsApp number", vec![], vec![])
                .with_keys("[gateway.whatsapp] allowed_users", "-")
                .silent(),
            timing: Timing::default(),
            windows: Windows::open(cfg.state_dir.clone()),
            cfg,
            posted: Mutex::new(HashMap::new()),
            seen: Mutex::new((VecDeque::new(), HashSet::new())),
            sent: Mutex::new(VecDeque::new()),
            last_poll: Mutex::new(None),
            token_refused: Mutex::new(None),
            inbound_problem: Mutex::new(None),
            failure: Mutex::new(None),
            failures_said: Mutex::new(HashMap::new()),
        }
    }

    /// Who may talk to the bot: `wa_id` digits.
    pub fn with_allowed(mut self, users: Vec<String>) -> Self {
        self.access = Access::new("whatsapp", "WhatsApp number", users, vec![])
            .with_keys("[gateway.whatsapp] allowed_users", "-")
            .silent();
        self
    }

    /// Setup only: the first message that is exactly `code` pairs its
    /// author.
    pub fn with_pairing(mut self, code: impl Into<String>) -> Self {
        let access = std::mem::replace(
            &mut self.access,
            Access::new("whatsapp", "WhatsApp number", vec![], vec![]),
        );
        self.access = access.with_pairing(code);
        self
    }

    /// Who paired during setup, `(wa_id, name)`.
    pub fn paired(&self) -> Option<(String, String)> {
        self.access.paired()
    }

    /// Tests: waits in milliseconds, not seconds.
    #[doc(hidden)]
    pub fn with_fast_retries(mut self) -> Self {
        self.timing = Timing {
            poll: Duration::from_millis(30),
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(100),
            post_gap: Duration::from_millis(10),
            throughput: Duration::from_millis(10),
            pair: Duration::from_millis(20),
        };
        self
    }

    /// Chats with held messages, and how many.
    pub fn held(&self) -> std::collections::BTreeMap<String, usize> {
        self.windows.held_counts()
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// Whether message `id` is new (and remembers it).
    fn first_time(&self, id: &str) -> bool {
        let mut seen = self.seen.lock().unwrap();
        if !seen.1.insert(id.to_string()) {
            return false;
        }
        seen.0.push_back(id.to_string());
        if seen.0.len() > SEEN {
            if let Some(old) = seen.0.pop_front() {
                seen.1.remove(&old);
            }
        }
        true
    }

    /// Waits for `chat`'s turn: one message a second per chat keeps a
    /// split answer under Meta's pair rate limit.
    async fn pace(&self, chat: &str) {
        let slot = {
            let mut posted = self.posted.lock().unwrap();
            let now = Instant::now();
            let slot = posted
                .get(chat)
                .map_or(now, |last| (*last + self.timing.post_gap).max(now));
            posted.insert(chat.to_string(), slot);
            slot
        };
        tokio::time::sleep_until(slot).await;
    }

    /// One Graph API request; Meta's error as `GraphError`, a transport
    /// failure as code 0. Never says the token.
    async fn graph(&self, req: reqwest::RequestBuilder, what: &str) -> Result<Value, GraphError> {
        let resp = req
            .bearer_auth(&self.cfg.token)
            .send()
            .await
            .map_err(|e| GraphError {
                code: 0,
                message: format!("whatsapp {what} failed: {}", e.without_url()),
            })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() && json.get("error").is_none() {
            return Ok(json);
        }
        let err = &json["error"];
        let code = err["code"]
            .as_i64()
            .unwrap_or(if status.as_u16() == 429 { 130429 } else { -1 });
        let detail = err["error_data"]["details"]
            .as_str()
            .or_else(|| err["message"].as_str())
            .map(|m| crate::health::clip(m, 300))
            .unwrap_or_else(|| format!("status {status}"));
        if code == 190 {
            *self.token_refused.lock().unwrap() = Some(format!(
                "the WhatsApp token was refused (expired?): {detail}. Make a permanent one (a system user's) and save it again"
            ));
        }
        Err(GraphError {
            code,
            message: format!("whatsapp {what} failed ({code}): {detail}"),
        })
    }

    /// `POST /<phone_number_id>/messages`, with Meta's limits waited out:
    /// 130429 backs off 2, 4, 8 s; 131056 waits 6 s; three tries each.
    async fn post_message(&self, body: Value) -> Result<Value, GraphError> {
        let url = format!("{}/{}/messages", self.graph, self.cfg.phone_number_id);
        let mut body = body;
        body["messaging_product"] = json!("whatsapp");
        let mut tries = 0u32;
        loop {
            tries += 1;
            match self.graph(self.client.post(&url).json(&body), "send").await {
                Err(e) if e.code == 130429 && tries <= 3 => {
                    let wait = self.timing.throughput * 2u32.pow(tries - 1);
                    tracing::warn!("whatsapp: throughput limit, waiting {wait:?}");
                    tokio::time::sleep(wait).await;
                }
                Err(e) if e.code == 131056 && tries < 3 => {
                    tracing::warn!("whatsapp: pair rate limit, waiting");
                    tokio::time::sleep(self.timing.pair).await;
                }
                r => {
                    if r.is_ok() {
                        *self.token_refused.lock().unwrap() = None;
                    }
                    return r;
                }
            }
        }
    }

    /// Sends one message to `chat`; its id. A closed window comes back as
    /// `Err(131047)` for the caller to hold.
    async fn send_one(
        &self,
        chat: &str,
        mut body: Value,
        text: &str,
    ) -> Result<String, GraphError> {
        body["to"] = json!(chat);
        body["recipient_type"] = json!("individual");
        self.pace(chat).await;
        let v = self.post_message(body).await?;
        let id = v["messages"][0]["id"].as_str().unwrap_or("").to_string();
        if !id.is_empty() {
            let mut sent = self.sent.lock().unwrap();
            sent.push_back((id.clone(), chat.to_string(), text.to_string()));
            if sent.len() > SENT {
                sent.pop_front();
            }
        }
        Ok(id)
    }

    /// Everything in `msg` goes out, text split and files uploaded; a
    /// closed window holds it (§3.4).
    async fn deliver(
        &self,
        msg: &OutboundMessage,
        buttons: Option<&[Button]>,
    ) -> Result<Option<String>, GatewayError> {
        let chat = msg.chat_id.as_str();
        if self.windows.state(chat, Self::now()) == Window::Closed {
            let text = match buttons {
                Some(b) => buttons_as_text(&msg.text, b),
                None => msg.text.clone(),
            };
            return self.hold(chat, &text, &msg.attachments).await.map(|_| None);
        }
        match self.deliver_open(msg, buttons).await {
            Ok(id) => Ok(id),
            Err((e, rest)) if e.code == 131047 => {
                self.windows.closed(chat);
                self.hold(chat, &rest.0, &rest.1).await.map(|_| None)
            }
            Err((e, _)) => Err(self.explain(e)),
        }
    }

    /// Sends in an open (or unknown) window. On an error, what didn't go:
    /// the text from the failed piece on, and the files not yet sent.
    async fn deliver_open(
        &self,
        msg: &OutboundMessage,
        buttons: Option<&[Button]>,
    ) -> Result<Option<String>, (GraphError, (String, Vec<Attachment>))> {
        let chat = msg.chat_id.as_str();
        let mut last = None;
        // Files first, the text as the first file's caption when it fits.
        let mut text = msg.text.clone();
        for (i, att) in msg.attachments.iter().enumerate() {
            let caption = (i == 0
                && !text.trim().is_empty()
                && text.chars().count() <= CAPTION_LIMIT
                && buttons.is_none())
            .then(|| std::mem::take(&mut text));
            match self.send_file(chat, att, caption.as_deref()).await {
                Ok(id) => last = Some(id),
                Err(e) => {
                    let mut back = caption.unwrap_or(text);
                    if back.is_empty() {
                        back = msg.text.clone();
                    }
                    return Err((e, (back, msg.attachments[i..].to_vec())));
                }
            }
        }
        if text.trim().is_empty() && buttons.is_none() {
            return Ok(last);
        }
        let interactive = buttons.filter(|b| Self::fits_buttons(b));
        let converted = mrkdwn::whatsapp(&text);
        let (pieces, body_for_buttons) = match interactive {
            Some(_) if converted.chars().count() <= BODY_LIMIT && !converted.trim().is_empty() => {
                (vec![], converted.clone())
            }
            Some(_) => (
                chunks(&converted, MESSAGE_LIMIT),
                "Your answer:".to_string(),
            ),
            None => {
                let all = match buttons {
                    Some(b) => mrkdwn::whatsapp(&buttons_as_text(&text, b)),
                    None => converted.clone(),
                };
                (chunks(&all, MESSAGE_LIMIT), String::new())
            }
        };
        let pieces: Vec<String> = pieces
            .into_iter()
            .filter(|p| !p.trim().is_empty())
            .collect();
        for (i, piece) in pieces.iter().enumerate() {
            let body = json!({ "type": "text", "text": { "body": piece, "preview_url": false } });
            match self.send_one(chat, body, piece).await {
                Ok(id) => last = Some(id),
                Err(e) => {
                    let rest = pieces[i..].join("\n\n");
                    let rest = match buttons {
                        Some(b) => buttons_as_text(&rest, b),
                        None => rest,
                    };
                    return Err((e, (rest, vec![])));
                }
            }
        }
        if let Some(b) = interactive {
            let body = json!({
                "type": "interactive",
                "interactive": {
                    "type": "button",
                    "body": { "text": body_for_buttons },
                    "action": { "buttons": Self::reply_buttons(b) },
                },
            });
            match self.send_one(chat, body, &body_for_buttons).await {
                Ok(id) => last = Some(id),
                Err(e) => return Err((e, (buttons_as_text(&text, b), vec![]))),
            }
        }
        Ok(last)
    }

    /// Up to three reply buttons, each a command short enough for an id.
    fn fits_buttons(buttons: &[Button]) -> bool {
        !buttons.is_empty()
            && buttons.len() <= 3
            && buttons.iter().all(|b| {
                matches!(&b.action, ButtonAction::Command(c) if !c.is_empty() && c.len() <= BUTTON_ID)
            })
    }

    fn reply_buttons(buttons: &[Button]) -> Vec<Value> {
        buttons
            .iter()
            .filter_map(|b| match &b.action {
                ButtonAction::Command(c) => Some(json!({
                    "type": "reply",
                    "reply": { "id": c, "title": title(&b.text) },
                })),
                ButtonAction::Url(_) => None,
            })
            .collect()
    }

    /// Uploads a local file and sends it; the message id.
    async fn send_file(
        &self,
        chat: &str,
        att: &Attachment,
        caption: Option<&str>,
    ) -> Result<String, GraphError> {
        let fail = |message: String| GraphError { code: -1, message };
        let (name, mime, bytes) = files::read_outgoing(att).map_err(|e| fail(e.to_string()))?;
        let mime = if att.kind.contains('/') {
            att.kind.as_str()
        } else {
            mime
        };
        let kind = media_kind(mime);
        let max = match kind {
            "image" => IMAGE_MAX,
            "audio" | "video" => AV_MAX,
            _ => DOCUMENT_MAX,
        };
        if bytes.len() as u64 > max {
            return Err(fail(format!(
                "{name} is {}, over WhatsApp's {} limit for {kind} files",
                files::human(bytes.len() as u64),
                files::human(max)
            )));
        }
        let (boundary, body) = files::multipart(
            &[("messaging_product", "whatsapp"), ("type", mime)],
            ("file", &name, mime, &bytes),
        );
        let url = format!("{}/{}/media", self.graph, self.cfg.phone_number_id);
        let up = self
            .graph(
                self.client
                    .post(url)
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(body),
                "upload",
            )
            .await?;
        let id = up["id"]
            .as_str()
            .ok_or_else(|| fail("whatsapp upload gave no media id".into()))?;
        let mut media = json!({ "id": id });
        if let Some(c) = caption.filter(|_| kind != "audio") {
            media["caption"] = json!(mrkdwn::whatsapp(c));
        }
        if kind == "document" {
            media["filename"] = json!(name);
        }
        let body = json!({ "type": kind, kind: media });
        self.send_one(chat, body, caption.unwrap_or(&name)).await
    }

    /// Holds a message for `chat` while its window is closed; with a
    /// template, the person is told something waits (once per closed
    /// window), else `send` fails saying so.
    async fn hold(
        &self,
        chat: &str,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<(), GatewayError> {
        let first = self.windows.hold(chat, text, attachments, Self::now());
        let Some(t) = &self.cfg.template else {
            return Err(GatewayError::Channel(format!(
                "WhatsApp's 24-hour window for {chat} is closed; the message is held until they write again (set a template to reach them first)"
            )));
        };
        if !first {
            tracing::info!("whatsapp: another message held for {chat}; the template already went");
            return Ok(());
        }
        let param = template_param(text);
        let body = json!({
            "type": "template",
            "template": {
                "name": t.name,
                "language": { "code": t.language },
                "components": [{ "type": "body", "parameters": [{ "type": "text", "text": param }] }],
            },
        });
        self.pace(chat).await;
        let mut body = body;
        body["to"] = json!(chat);
        match self.post_message(body).await {
            Ok(_) => {
                tracing::info!("whatsapp: window closed for {chat}; sent template {} and held the message", t.name);
                Ok(())
            }
            Err(e) => Err(GatewayError::Channel(format!(
                "WhatsApp's 24-hour window for {chat} is closed and the template {} didn't go ({}); the message is held until they write again",
                t.name, e.message
            ))),
        }
    }

    /// Meta's code, in words the owner can act on.
    fn explain(&self, e: GraphError) -> GatewayError {
        match e.code {
            130429 | 131056 => GatewayError::RateLimited {
                retry_after: self.timing.backoff_max.min(Duration::from_secs(60)),
            },
            131030 => GatewayError::Channel(format!(
                "{}. The test number can only message numbers on its list: add them under API Setup → To, or use a real number",
                e.message
            )),
            _ => GatewayError::Channel(e.message),
        }
    }

    /// Sends what was held for `chat`, now that they wrote.
    async fn flush_held(&self, chat: &str) {
        let (held, dropped) = self.windows.take(chat, Self::now());
        if held.is_empty() && dropped == 0 {
            return;
        }
        let mut head = "Held while WhatsApp's 24-hour window was closed:".to_string();
        if dropped > 0 {
            head.push_str(&format!(" ({dropped} older message(s) weren't kept)"));
        }
        let say = OutboundMessage {
            channel: "whatsapp".into(),
            chat_id: chat.to_string(),
            text: head,
            reply_to: None,
            attachments: vec![],
        };
        if let Err((e, _)) = self.deliver_open(&say, None).await {
            tracing::warn!("whatsapp: couldn't send the held messages: {}", e.message);
            self.windows.put_back(chat, held);
            return;
        }
        for (i, m) in held.iter().enumerate() {
            let msg = OutboundMessage {
                channel: "whatsapp".into(),
                chat_id: chat.to_string(),
                text: m.text.clone(),
                reply_to: None,
                attachments: m.attachments.clone(),
            };
            if let Err((e, _)) = self.deliver_open(&msg, None).await {
                tracing::warn!("whatsapp: a held message didn't go: {}", e.message);
                self.windows.put_back(chat, held[i..].to_vec());
                return;
            }
        }
    }

    /// A delivery failure from a `statuses` webhook.
    fn on_failed(&self, id: &str, chat: &str, code: i64, title: &str) {
        tracing::warn!("whatsapp: a message to {chat} failed ({code}): {title}");
        if code == 131047 {
            self.windows.closed(chat);
            let text = {
                let mut sent = self.sent.lock().unwrap();
                sent.iter()
                    .position(|(i, _, _)| i == id)
                    .and_then(|n| sent.remove(n))
                    .map(|(_, _, t)| t)
            };
            if let Some(text) = text {
                self.windows.hold(chat, &text, &[], Self::now());
            }
        }
        let mut said = self.failures_said.lock().unwrap();
        let due = said
            .get(&code)
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(3600));
        if due {
            said.insert(code, Instant::now());
            *self.failure.lock().unwrap() = Some((
                format!("a message to {chat} failed ({code}): {title}"),
                Instant::now(),
            ));
        }
    }

    /// Downloads media `id` into the inbox.
    async fn fetch_media(
        &self,
        id: &str,
        name: &str,
        message_id: &str,
    ) -> Result<files::Saved, files::Refused> {
        let refuse = |why: String| files::Refused {
            name: name.to_string(),
            why,
        };
        let Some(inbox) = &self.cfg.inbox else {
            return Err(refuse("this gateway doesn't save files".into()));
        };
        let meta = self
            .graph(
                self.client.get(format!("{}/{id}", self.graph)),
                "media lookup",
            )
            .await
            .map_err(|e| refuse(e.message))?;
        let size = meta["file_size"].as_u64().unwrap_or(0);
        if size > inbox.max_bytes() {
            return Err(inbox.too_big(name, size));
        }
        let url = meta["url"]
            .as_str()
            .ok_or_else(|| refuse("Meta gave no download link".into()))?;
        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.cfg.token)
            .send()
            .await
            .map_err(|e| refuse(format!("the download failed: {}", e.without_url())))?;
        if !resp.status().is_success() {
            return Err(refuse(format!(
                "the download failed (status {})",
                resp.status()
            )));
        }
        let bytes = files::read_capped(resp, inbox.max_bytes())
            .await
            .map_err(refuse)?;
        let mime = meta["mime_type"].as_str();
        inbox
            .save("whatsapp", message_id, name, mime, &bytes)
            .map_err(|e| refuse(format!("it couldn't be saved: {e}")))
    }
}

/// What the probe learned about the business number.
#[derive(Debug, Clone)]
pub struct Probe {
    /// `+1 555 0100`.
    pub number: String,
    /// The display name Meta verified (or the pending one).
    pub name: String,
}

/// `GET /<phone_number_id>` with the token: nothing is sent or changed.
pub async fn probe(
    api: &str,
    version: &str,
    phone_number_id: &str,
    token: &str,
) -> Result<Probe, String> {
    if phone_number_id.is_empty() || !phone_number_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "the phone number id should be digits (API Setup → From → Phone number ID), not {phone_number_id:?}"
        ));
    }
    let ch = WhatsAppChannel::new(WhatsAppConfig {
        phone_number_id: phone_number_id.into(),
        token: token.into(),
        app_secret: String::new(),
        verify_token: String::new(),
        api_url: api.into(),
        api_version: version.into(),
        inbound: Inbound::None,
        template: None,
        state_dir: None,
        inbox: None,
    });
    let url = format!(
        "{}/{phone_number_id}?fields=display_phone_number,verified_name",
        ch.graph
    );
    match ch.graph(ch.client.get(url), "probe").await {
        Ok(v) => Ok(Probe {
            number: v["display_phone_number"].as_str().unwrap_or("").to_string(),
            name: v["verified_name"].as_str().unwrap_or("").to_string(),
        }),
        Err(e) if e.code == 190 => Err(format!(
            "Meta refused the token ({}): make a system user's permanent token with whatsapp_business_messaging (business.facebook.com → Settings → System users)",
            e.message
        )),
        Err(e) if e.code == 100 => Err(format!(
            "{}: check the phone number id (API Setup → From), and that the token's app has this number",
            e.message
        )),
        Err(e) => Err(e.message),
    }
}

/// A button title: WhatsApp's 20 characters, cut with "…".
fn title(text: &str) -> String {
    if text.chars().count() <= BUTTON_TITLE {
        text.to_string()
    } else {
        let mut t: String = text.chars().take(BUTTON_TITLE - 1).collect();
        t.push('…');
        t
    }
}

/// A template's `{{1}}`: one line (Meta refuses newlines, tabs and four
/// spaces in a row), at most 200 characters.
fn template_param(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let flat = if flat.is_empty() {
        "a message".to_string()
    } else {
        flat
    };
    if flat.chars().count() <= TEMPLATE_PARAM {
        flat
    } else {
        let mut t: String = flat.chars().take(TEMPLATE_PARAM - 1).collect();
        t.push('…');
        t
    }
}

/// WhatsApp's message type for a MIME type. Images are JPEG or PNG only,
/// so anything else goes as a document.
fn media_kind(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/png" => "image",
        "video/mp4" | "video/3gpp" => "video",
        "audio/aac" | "audio/mp4" | "audio/mpeg" | "audio/amr" | "audio/ogg" => "audio",
        _ => "document",
    }
}

#[async_trait::async_trait]
impl Channel for WhatsAppChannel {
    fn name(&self) -> &str {
        "whatsapp"
    }

    /// A read receipt with a typing indicator on the message being
    /// answered; it shows for up to 25 s or until the reply: again every
    /// 20. Nothing clears it but the reply.
    async fn typing(&self, _chat_id: &str, message_id: &str, on: bool) -> Typing {
        if !on || message_id.is_empty() {
            return Typing::Unsupported;
        }
        let url = format!("{}/{}/messages", self.graph, self.cfg.phone_number_id);
        let body = json!({
            "messaging_product": "whatsapp",
            "status": "read",
            "message_id": message_id,
            "typing_indicator": { "type": "text" },
        });
        match self
            .graph(self.client.post(&url).json(&body), "typing")
            .await
        {
            Ok(_) => Typing::Shown {
                again_in: Duration::from_secs(20),
            },
            Err(e) if matches!(e.code, 130429 | 131056) => Typing::Limited,
            Err(e) => {
                tracing::debug!("whatsapp: typing: {}", e.message);
                Typing::Failed
            }
        }
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            reactions: true,
            edits: false,
            attachments: true,
            buttons: true,
        }
    }

    /// The mailbox is polled; a listener has nothing to go stale.
    fn polls(&self) -> bool {
        matches!(self.cfg.inbound, Inbound::Relay { .. })
    }

    fn last_ok_poll(&self) -> Option<SystemTime> {
        *self.last_poll.lock().unwrap()
    }

    fn problem(&self) -> Option<String> {
        if let Some(p) = self.token_refused.lock().unwrap().clone() {
            return Some(p);
        }
        if let Some(p) = self.inbound_problem.lock().unwrap().clone() {
            return Some(p);
        }
        let held = self.windows.held_counts();
        if !held.is_empty() && self.cfg.template.is_none() {
            let who: Vec<String> = held.iter().map(|(c, n)| format!("{n} for {c}")).collect();
            return Some(format!(
                "messages held until they write again (the 24-hour window is closed, and no template is set): {}",
                who.join(", ")
            ));
        }
        let mut failure = self.failure.lock().unwrap();
        match &*failure {
            Some((p, at)) if at.elapsed() < Duration::from_secs(3600) => Some(p.clone()),
            Some(_) => {
                *failure = None;
                None
            }
            None => None,
        }
    }

    fn message_limit(&self) -> Option<usize> {
        Some(MESSAGE_LIMIT)
    }

    async fn run(&self, tx: tokio::sync::mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        match self.cfg.inbound.clone() {
            Inbound::Relay { url, key } => self.run_mailbox(&url, &key, tx).await,
            Inbound::Listen { port } => self.run_listener(port, tx).await,
            Inbound::None => {
                std::future::pending::<()>().await;
                Ok(())
            }
        }
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        self.deliver(&msg, None).await.map(|_| ())
    }

    async fn post(&self, msg: OutboundMessage) -> Result<Option<String>, GatewayError> {
        self.deliver(&msg, None).await
    }

    async fn send_buttons(
        &self,
        msg: OutboundMessage,
        buttons: &[Button],
    ) -> Result<(), GatewayError> {
        self.deliver(&msg, Some(buttons)).await.map(|_| ())
    }

    /// The 👀: the blue ticks (a read receipt) and the reaction.
    async fn react(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), GatewayError> {
        if message_id.is_empty() {
            return Ok(());
        }
        let read = json!({ "status": "read", "message_id": message_id });
        if let Err(e) = self.post_message(read).await {
            tracing::debug!("whatsapp: read receipt: {}", e.message);
        }
        let body =
            json!({ "type": "reaction", "reaction": { "message_id": message_id, "emoji": emoji } });
        self.send_one(chat_id, body, "")
            .await
            .map(|_| ())
            .map_err(|e| self.explain(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_params_and_kinds() {
        assert_eq!(title("Approve"), "Approve");
        assert_eq!(
            title("Approve this and everything after"),
            "Approve this and ev…"
        );
        assert_eq!(
            title("Approve this and everything after").chars().count(),
            20
        );
        assert_eq!(
            template_param("line one\n\nline    two\t3"),
            "line one line two 3"
        );
        assert_eq!(template_param(&"x".repeat(500)).chars().count(), 200);
        assert_eq!(template_param(" \n"), "a message");
        assert_eq!(media_kind("image/png"), "image");
        assert_eq!(media_kind("image/webp"), "document");
        assert_eq!(media_kind("audio/ogg"), "audio");
        assert_eq!(media_kind("application/pdf"), "document");
    }

    #[test]
    fn only_three_short_commands_become_buttons() {
        let b = |t: &str, a: ButtonAction| Button {
            text: t.into(),
            action: a,
        };
        let yes = b("Yes", ButtonAction::Command("yes a1".into()));
        assert!(WhatsAppChannel::fits_buttons(&[
            yes.clone(),
            yes.clone(),
            yes.clone()
        ]));
        assert!(!WhatsAppChannel::fits_buttons(&[
            yes.clone(),
            yes.clone(),
            yes.clone(),
            yes.clone()
        ]));
        assert!(!WhatsAppChannel::fits_buttons(&[
            yes.clone(),
            b("Docs", ButtonAction::Url("https://x".into()))
        ]));
        assert!(!WhatsAppChannel::fits_buttons(&[b(
            "Long",
            ButtonAction::Command("x".repeat(257))
        )]));
        assert!(!WhatsAppChannel::fits_buttons(&[]));
    }
}
