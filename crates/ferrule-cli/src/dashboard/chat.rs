//! M37 §4.3: chat from the page, in its own session. [`DashboardChannel`]
//! is a gateway channel like Telegram's: a message typed on the page is an
//! inbound message on chat `owner`, so it runs in session
//! `dashboard__owner` and goes through the same doors (`/stop`, `yes
//! CODE`); the agent's replies, streamed edits and buttons land in a log
//! the page polls with `GET /api/chat?from=N`.

use super::api::{arg, bad, ok, Answer};
use super::http::Request;
use super::Ctx;
use async_trait::async_trait;
use base64::Engine;
use ferrule_gateway::channels::files::{self, Inbox};
use ferrule_gateway::{
    Attachment, Button, ButtonAction, Channel, ChannelCapabilities, GatewayError, InboundMessage,
    OutboundMessage,
};
use serde_json::{json, Value};
use std::sync::Mutex;
use tokio::sync::mpsc;

pub const CHANNEL: &str = "dashboard";
pub const CHAT: &str = "owner";
/// The most entries the log keeps; the session's own history keeps the
/// rest.
const KEEP: usize = 300;
/// The longest message the page may send.
pub const MAX_TEXT: usize = 16 * 1024;
/// The most a photo from the page may weigh (M47): what a provider takes.
pub const MAX_PHOTO: usize = ferrule_providers::vision::MAX_BYTES;
/// The most the request for one may weigh: the photo as base64, and words.
pub const MAX_PHOTO_BODY: usize = MAX_PHOTO.div_ceil(3) * 4 + MAX_TEXT + 1024;

struct Entry {
    id: u64,
    /// Bumped on every change, so the page reads only what changed.
    rev: u64,
    from_owner: bool,
    text: String,
    buttons: Vec<Value>,
    at: i64,
    /// A photo the owner sent with it: its name, never its bytes (M47).
    photo: Option<String>,
}

#[derive(Default)]
struct Log {
    entries: Vec<Entry>,
    next_id: u64,
    rev: u64,
}

#[derive(Default)]
pub struct DashboardChannel {
    log: Mutex<Log>,
    tx: Mutex<Option<mpsc::Sender<InboundMessage>>>,
    /// Where a photo from the page is saved, in the agent's workspace.
    inbox: Option<Inbox>,
}

impl DashboardChannel {
    /// Photos from the page are saved here; without one the page can't send them.
    pub fn with_inbox(mut self, inbox: Option<Inbox>) -> Self {
        self.inbox = inbox;
        self
    }

    fn push(&self, from_owner: bool, text: &str, buttons: Vec<Value>) -> u64 {
        self.push_with(from_owner, text, buttons, None)
    }

    fn push_with(
        &self,
        from_owner: bool,
        text: &str,
        buttons: Vec<Value>,
        photo: Option<String>,
    ) -> u64 {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.next_id += 1;
        log.rev += 1;
        let (id, rev) = (log.next_id, log.rev);
        log.entries.push(Entry {
            id,
            rev,
            from_owner,
            text: text.to_string(),
            buttons,
            at: chrono::Utc::now().timestamp(),
            photo,
        });
        let over = log.entries.len().saturating_sub(KEEP);
        log.entries.drain(..over);
        id
    }

    /// Whether the gateway is reading this channel (it isn't under
    /// `ferrule dashboard` on its own, or before the gateway starts).
    pub fn listening(&self) -> bool {
        self.tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|tx| !tx.is_closed())
    }

    /// The owner typed `text` on the page.
    pub async fn say(&self, text: &str) -> Result<(), String> {
        let tx = self
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .filter(|tx| !tx.is_closed())
            .ok_or("the agent isn't listening here: chat works when the dashboard runs inside the gateway (the service)")?;
        let id = self.push(true, text, Vec::new());
        let msg = InboundMessage {
            channel: CHANNEL.into(),
            chat_id: CHAT.into(),
            sender: "owner".into(),
            sender_id: Some(CHAT.into()),
            message_id: id.to_string(),
            text: text.to_string(),
            attachments: Vec::new(),
            reply_to: None,
            ts: chrono::Utc::now().timestamp(),
        };
        tx.send(msg)
            .await
            .map_err(|_| "the agent stopped listening; restart the service".to_string())
    }

    /// The owner sent a photo (and maybe words) from the page: saved in the
    /// workspace's inbox, then handed to the agent like a Telegram photo.
    /// The log keeps the words and the photo's name, not its path.
    pub async fn say_photo(&self, text: &str, mime: &str, bytes: Vec<u8>) -> Result<(), String> {
        let tx = self
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .filter(|tx| !tx.is_closed())
            .ok_or("the agent isn't listening here: chat works when the dashboard runs inside the gateway (the service)")?;
        let inbox = self
            .inbox
            .clone()
            .ok_or("this agent has no workspace to keep photos in")?;
        let ext = match mime {
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => "jpg",
        };
        let name = format!("photo.{ext}");
        let id = self.push_with(true, text, Vec::new(), Some(name.clone()));
        let saved = {
            let (name, mime, id) = (name.clone(), mime.to_string(), id.to_string());
            tokio::task::spawn_blocking(move || {
                inbox.save(CHANNEL, &id, &name, Some(&mime), &bytes)
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("the photo couldn't be saved: {e}"))?
        };
        let msg = InboundMessage {
            channel: CHANNEL.into(),
            chat_id: CHAT.into(),
            sender: "owner".into(),
            sender_id: Some(CHAT.into()),
            message_id: id.to_string(),
            text: files::with_notes(text, std::slice::from_ref(&saved), &[]),
            attachments: vec![Attachment {
                kind: saved.mime.clone(),
                url: saved.path.to_string_lossy().into_owned(),
                name: Some(saved.rel.clone()),
            }],
            reply_to: None,
            ts: chrono::Utc::now().timestamp(),
        };
        tx.send(msg)
            .await
            .map_err(|_| "the agent stopped listening; restart the service".to_string())
    }

    /// Everything changed after revision `from`, and the revision to ask
    /// from next. An entry comes again whole when it was edited.
    pub fn since(&self, from: u64) -> Value {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let entries: Vec<Value> = log
            .entries
            .iter()
            .filter(|e| e.rev > from)
            .map(|e| {
                let mut v = json!({
                    "id": e.id,
                    "who": if e.from_owner { "you" } else { "agent" },
                    "text": e.text,
                    "buttons": e.buttons,
                    "at": e.at,
                });
                if let Some(name) = &e.photo {
                    v["photo"] = json!({ "name": name });
                }
                v
            })
            .collect();
        // The agent owes an answer while the owner spoke last.
        let waiting = log.entries.last().is_some_and(|e| e.from_owner);
        json!({
            "entries": entries,
            "next": log.rev,
            "waiting": waiting,
            "listening": self.listening(),
            "first": log.entries.first().map(|e| e.id),
        })
    }
}

/// `GET /api/chat?from=REV`.
pub fn view(ctx: &Ctx, req: &Request) -> Answer {
    let Some(ch) = &ctx.chat else {
        return ok(json!({
            "entries": [], "next": 0, "waiting": false, "listening": false,
            "why": "Chat from the page works when the dashboard runs inside the gateway (the service).",
        }));
    };
    let from = req
        .query
        .get("from")
        .and_then(|f| f.parse().ok())
        .unwrap_or(0);
    ok(ch.since(from))
}

/// `POST /api/chat/send {text}`.
pub async fn send(ctx: &Ctx, body: &Value) -> Answer {
    let text = match arg(body, "text") {
        Ok(t) => t,
        Err(answer) => return answer,
    };
    if text.len() > MAX_TEXT {
        return bad(413, format!("that message is over {} KB", MAX_TEXT / 1024));
    }
    let Some(ch) = &ctx.chat else {
        return bad(
            503,
            "chat from the page works when the dashboard runs inside the gateway (the service)",
        );
    };
    match ch.say(text).await {
        Ok(()) => ok(json!({ "ok": true })),
        Err(why) => bad(503, why),
    }
}

/// `POST /api/chat/photo {text, mime, data}`: a photo from the page, as
/// base64. The page shrinks it first; the size and the type are checked
/// again here, by the bytes and not by the label.
pub async fn photo(ctx: &Ctx, body: &Value) -> Answer {
    let text = body.get("text").and_then(Value::as_str).unwrap_or("");
    if text.len() > MAX_TEXT {
        return bad(413, format!("that message is over {} KB", MAX_TEXT / 1024));
    }
    let mime = body.get("mime").and_then(Value::as_str).unwrap_or("");
    if !["image/jpeg", "image/png", "image/webp", "image/gif"].contains(&mime) {
        return bad(415, "Send a JPEG, PNG, WebP or GIF photo.");
    }
    let too_big = || {
        bad(
            413,
            "That photo is too big: the page sends up to 3.5 MB. Try a smaller one.",
        )
    };
    let data = body.get("data").and_then(Value::as_str).unwrap_or("");
    // Base64 is 4 bytes for 3: refuse by length before decoding.
    if data.len() > MAX_PHOTO.div_ceil(3) * 4 {
        return too_big();
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return bad(400, "That file isn't a photo the bot can read.");
    };
    if bytes.len() > MAX_PHOTO {
        return too_big();
    }
    if ferrule_providers::vision::sniff(&bytes) != Some(mime) {
        return bad(400, "That file isn't a photo the bot can read.");
    }
    let Some(ch) = &ctx.chat else {
        return bad(
            503,
            "chat from the page works when the dashboard runs inside the gateway (the service)",
        );
    };
    match ch.say_photo(text, mime, bytes).await {
        Ok(()) => ok(json!({ "ok": true })),
        Err(why) => bad(503, why),
    }
}

/// `GET /api/approvals`: every question waiting for the owner, in any chat.
pub fn approvals(ctx: &Ctx) -> Answer {
    let Some(hub) = &ctx.hub else {
        return ok(json!({ "approvals": [] }));
    };
    let list: Vec<Value> = hub
        .approvals()
        .list()
        .into_iter()
        .map(|w| {
            json!({
                "code": w.code,
                "chat": format!("{}:{}", w.chat.channel, w.chat.chat),
                "what": ctx.redactor.redact(&w.what),
                "secs": w.secs,
            })
        })
        .collect();
    ok(json!({ "approvals": list }))
}

/// `POST /api/approvals/answer {code, allow}`, as the chat's button would.
pub fn answer(ctx: &Ctx, body: &Value) -> Answer {
    let code = match arg(body, "code") {
        Ok(c) => c,
        Err(answer) => return answer,
    };
    let Some(allow) = body.get("allow").and_then(Value::as_bool) else {
        return bad(400, "`allow` is missing");
    };
    let Some(hub) = &ctx.hub else {
        return bad(404, "nothing is waiting");
    };
    let Some(said) = hub
        .approvals()
        .decide(code, allow, "refused on the dashboard")
    else {
        return bad(404, "that question was already answered or withdrawn");
    };
    hub.audit().record(
        chrono::Utc::now(),
        if allow {
            "approval_allowed"
        } else {
            "approval_refused"
        },
        None,
        None,
        json!({ "by": super::api::by(), "code": code }),
    );
    ok(json!({ "ok": true, "said": ctx.redactor.redact(&said) }))
}

fn button_json(b: &Button) -> Value {
    match &b.action {
        ButtonAction::Url(url) => json!({ "text": b.text, "url": url }),
        ButtonAction::Command(cmd) => json!({ "text": b.text, "send": cmd }),
    }
}

#[async_trait]
impl Channel for DashboardChannel {
    fn name(&self) -> &str {
        CHANNEL
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            reactions: false,
            edits: true,
            // Outbound: the page's log shows text; `send_file` says so.
            attachments: false,
            buttons: true,
        }
    }

    async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        *self.tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
        // The page pushes messages through `say`; this loop only has to
        // last as long as the gateway.
        tx.closed().await;
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        self.push(false, &msg.text, Vec::new());
        Ok(())
    }

    async fn post(&self, msg: OutboundMessage) -> Result<Option<String>, GatewayError> {
        Ok(Some(self.push(false, &msg.text, Vec::new()).to_string()))
    }

    async fn send_buttons(
        &self,
        msg: OutboundMessage,
        buttons: &[Button],
    ) -> Result<(), GatewayError> {
        self.push(false, &msg.text, buttons.iter().map(button_json).collect());
        Ok(())
    }

    async fn edit(&self, _chat_id: &str, message_id: &str, text: &str) -> Result<(), GatewayError> {
        let id: u64 = message_id
            .parse()
            .map_err(|_| GatewayError::Channel(format!("no message {message_id}")))?;
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.rev += 1;
        let rev = log.rev;
        match log.entries.iter_mut().find(|e| e.id == id) {
            Some(e) => {
                e.text = text.to_string();
                e.rev = rev;
                Ok(())
            }
            None => Err(GatewayError::Channel(format!("no message {message_id}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(text: &str) -> OutboundMessage {
        OutboundMessage {
            channel: CHANNEL.into(),
            chat_id: CHAT.into(),
            text: text.into(),
            reply_to: None,
            attachments: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_page_message_goes_in_and_replies_edits_and_buttons_come_back_in_order() {
        let ch = std::sync::Arc::new(DashboardChannel::default());
        assert!(ch.say("hi").await.is_err(), "nobody listening yet");
        let (tx, mut rx) = mpsc::channel(4);
        let run = {
            let ch = ch.clone();
            tokio::spawn(async move { ch.run(tx).await })
        };
        while !ch.listening() {
            tokio::task::yield_now().await;
        }
        ch.say("שלום, what's up?").await.unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(
            (got.channel.as_str(), got.chat_id.as_str()),
            ("dashboard", "owner")
        );
        assert_eq!(
            ferrule_gateway::session::session_id(&got.channel, &got.chat_id),
            "dashboard__owner"
        );
        let v = ch.since(0);
        assert_eq!(v["waiting"], true);
        let id = ch.post(out("thinking")).await.unwrap().unwrap();
        let mid = ch.since(0)["next"].as_u64().unwrap();
        ch.edit(CHAT, &id, "the answer").await.unwrap();
        let v = ch.since(mid);
        assert_eq!(
            v["entries"].as_array().unwrap().len(),
            1,
            "only the edit: {v}"
        );
        assert_eq!(v["entries"][0]["text"], "the answer");
        assert_eq!(v["waiting"], false);
        ch.send_buttons(
            out("Allow it?"),
            &[Button {
                text: "Allow".into(),
                action: ButtonAction::Command("yes k7".into()),
            }],
        )
        .await
        .unwrap();
        let all = ch.since(0);
        let e = all["entries"].as_array().unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(e[0]["who"], "you");
        assert_eq!(e[2]["buttons"][0]["send"], "yes k7");
        drop(rx);
        run.await.unwrap().unwrap();
        assert!(!ch.listening());
    }
}
